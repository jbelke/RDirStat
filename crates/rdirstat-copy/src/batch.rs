//! Landing many regular files of one directory with one `ditto` process,
//! and moving a staged copy into place without ever replacing anything.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{CopyError, Landed, Method, c_path, compare_tree, run};

/// Most bytes of source paths one `ditto` command line carries, well under
/// `ARG_MAX` (1 MiB on macOS, shared with the environment).
const MAX_ARG_BYTES: usize = 256 * 1024;
/// Most files one `ditto` copies, so one failed process costs a bounded
/// number of retries in the next wave.
const MAX_FILES_PER_DITTO: usize = 4096;

/// Prefix of the staging directories [`land_files`] makes. A crash can leave
/// one behind; its name says what it is.
pub const STAGING_PREFIX: &str = ".rdirstat-staging-";

/// A fresh name for a staging entry next to the destination:
/// [`STAGING_PREFIX`], the process id, the time and a counter.
#[must_use]
pub fn staging_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{STAGING_PREFIX}{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Renames `from` to `to` only if nothing is at `to` (`renamex_np` with
/// `RENAME_EXCL`). A plain `rename(2)` would replace an existing file, or an
/// empty directory, at `to`.
///
/// # Errors
///
/// [`CopyError::AlreadyExists`] if `to` exists; [`CopyError::Io`] otherwise.
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<(), CopyError> {
    let c_from = c_path(from)?;
    let c_to = c_path(to)?;
    match renamex_excl(&c_from, &c_to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            Err(CopyError::AlreadyExists { path: to.to_path_buf() })
        }
        Err(error) => Err(CopyError::io(to, error)),
    }
}

#[allow(unsafe_code)]
fn renamex_excl(from: &std::ffi::CStr, to: &std::ffi::CStr) -> std::io::Result<()> {
    // SAFETY: both are valid NUL-terminated strings that outlive the call.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Lands the regular files `names` of `source_dir` as the same names in
/// `dest_dir`, with one `ditto` process per chunk instead of one per file.
///
/// Each chunk is copied by `ditto` into a fresh staging directory inside
/// `dest_dir` (so the final move is a rename on one volume). Every staged
/// file is then held to its source with [`compare_tree`] (contents and xattr
/// names) and moved into place with [`rename_no_replace`]: nothing at the
/// destination is ever replaced. The staging directory is removed once it is
/// empty.
///
/// The result has one entry per name, in order, with the same meaning as
/// [`crate::land`]'s: a [`CopyError::Partial`] names a staged copy left on
/// disk (a verify that failed, or a name that appeared at the destination
/// meanwhile). Callers pass regular files only: `ditto` follows a symlink
/// named as its source.
#[must_use]
pub fn land_files(
    source_dir: &Path,
    names: &[OsString],
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Vec<Result<Landed, CopyError>> {
    let mut results: Vec<Result<Landed, CopyError>> = Vec::with_capacity(names.len());
    if names.is_empty() {
        return results;
    }
    if let Err(error) = std::fs::create_dir_all(dest_dir) {
        let message = error.to_string();
        return names
            .iter()
            .map(|_| Err(CopyError::io(dest_dir, std::io::Error::other(message.clone()))))
            .collect();
    }
    let mut start = 0;
    while start < names.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < names.len() && end - start < MAX_FILES_PER_DITTO {
            let length = source_dir.as_os_str().len() + names[end].len() + 2;
            if end > start && bytes + length > MAX_ARG_BYTES {
                break;
            }
            bytes += length;
            end += 1;
        }
        results.extend(land_chunk(source_dir, &names[start..end], dest_dir, cancel));
        start = end;
    }
    results
}

fn land_chunk(
    source_dir: &Path,
    names: &[OsString],
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Vec<Result<Landed, CopyError>> {
    let all = |make: &dyn Fn() -> CopyError| names.iter().map(|_| Err(make())).collect::<Vec<_>>();
    if cancel.load(Ordering::Relaxed) {
        return all(&|| CopyError::Cancelled);
    }
    let staging = dest_dir.join(staging_name());
    if let Err(error) = std::fs::create_dir(&staging) {
        let message = error.to_string();
        return all(&|| CopyError::io(&staging, std::io::Error::other(message.clone())));
    }
    let ditto_phase = crate::phase::enter(crate::phase::Phase::Ditto);
    crate::phase::add_bytes_with(crate::phase::Phase::Ditto, || {
        names
            .iter()
            .map(|name| crate::ditto::logical_bytes(&source_dir.join(name)))
            .sum()
    });
    let copied = run(
        "ditto",
        Command::new("/usr/bin/ditto")
            .args(["--rsrc", "--extattr", "--acl"])
            .args(names.iter().map(|name| source_dir.join(name)))
            .arg(&staging),
    );
    drop(ditto_phase);
    let failure = copied.err().map(|error| error.to_string());
    let mut results = Vec::with_capacity(names.len());
    for name in names {
        let source = source_dir.join(name);
        let staged = staging.join(name);
        let target = dest_dir.join(name);
        let wrote = std::fs::symlink_metadata(&staged).is_ok();
        results.push(match (&failure, wrote) {
            (Some(failure), false) => Err(CopyError::Tool {
                program: "ditto",
                code: -1,
                stderr: failure.clone(),
            }),
            (Some(failure), true) => Err(partial(
                &staged,
                CopyError::Tool {
                    program: "ditto",
                    code: -1,
                    stderr: failure.clone(),
                },
            )),
            (None, false) => Err(CopyError::mismatch(&source, "ditto did not copy this file")),
            (None, true) => settle(&source, &staged, &target, cancel),
        });
    }
    // Only an empty directory is removed: what is left in it is named.
    let _ = std::fs::remove_dir(&staging);
    results
}

/// Verifies one staged copy and moves it into place.
fn settle(source: &Path, staged: &Path, target: &Path, cancel: &AtomicBool) -> Result<Landed, CopyError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(partial(staged, CopyError::Cancelled));
    }
    let tally = compare_tree(source, staged).map_err(|error| partial(staged, error))?;
    rename_no_replace(staged, target).map_err(|error| partial(staged, error))?;
    Ok(Landed {
        method: Method::Copy,
        tally,
    })
}

fn partial(path: &Path, cause: CopyError) -> CopyError {
    CopyError::Partial {
        path: PathBuf::from(path),
        cause: Box::new(cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partial_path;

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn many_files_land_through_one_staging_directory_that_is_then_gone() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("src");
        std::fs::create_dir(&source).expect("mkdir");
        let mut names = Vec::new();
        for i in 0..50 {
            let name = format!("f{i:02}.txt");
            std::fs::write(source.join(&name), format!("contents {i}")).expect("write");
            names.push(OsString::from(name));
        }
        let status = Command::new("/usr/bin/xattr")
            .args(["-w", "user.rdirstat.probe", "kept"])
            .arg(source.join("f07.txt"))
            .status()
            .expect("xattr");
        assert!(status.success());

        let dest = scratch.path().join("out/dir");
        let results = land_files(&source, &names, &dest, &never());
        assert_eq!(results.len(), 50);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        for i in 0..50 {
            assert_eq!(
                std::fs::read_to_string(dest.join(format!("f{i:02}.txt"))).expect("landed"),
                format!("contents {i}")
            );
        }
        let left: Vec<_> = std::fs::read_dir(&dest)
            .expect("readdir")
            .map(|e| e.expect("entry").file_name())
            .filter(|n| n.to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(left.is_empty(), "staging removed: {left:?}");
    }

    #[test]
    fn a_name_that_appeared_meanwhile_is_never_replaced() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("src");
        std::fs::create_dir(&source).expect("mkdir");
        std::fs::write(source.join("a"), b"new a").expect("write");
        std::fs::write(source.join("b"), b"new b").expect("write");
        let dest = scratch.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir");
        std::fs::write(dest.join("b"), b"keep").expect("write");

        let results = land_files(&source, &[OsString::from("a"), OsString::from("b")], &dest, &never());
        assert!(results[0].is_ok(), "{:?}", results[0]);
        let error = results[1].as_ref().expect_err("refused");
        let staged = partial_path(error).expect("the staged copy is named");
        assert_eq!(std::fs::read(staged).expect("staged copy kept"), b"new b");
        assert_eq!(std::fs::read(dest.join("b")).expect("read"), b"keep");
        assert_eq!(std::fs::read(dest.join("a")).expect("read"), b"new a");
    }

    #[test]
    fn rename_no_replace_refuses_an_empty_directory_target() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let from = scratch.path().join("from");
        let to = scratch.path().join("to");
        std::fs::create_dir(&from).expect("mkdir");
        std::fs::write(from.join("x"), b"x").expect("write");
        std::fs::create_dir(&to).expect("mkdir");
        let error = rename_no_replace(&from, &to).expect_err("refused");
        assert!(matches!(error, CopyError::AlreadyExists { .. }), "{error:?}");
        assert!(from.join("x").exists());
    }
}
