//! Proving a copy: byte-for-byte contents, then extended-attribute names.

use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::io::{self, Read as _};
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};

use crate::{CopyError, c_path};

/// Block size for the streaming compare. Both files are read to the same
/// boundary before being compared, so short reads cannot fake a difference.
pub const COMPARE_CHUNK: usize = 256 * 1024;

/// What a successful compare checked.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VerifyTally {
    /// Regular files and symlinks compared.
    pub files: u64,
    /// Bytes of regular-file content compared.
    pub bytes: u64,
    /// Directories whose entry names were compared (the root included).
    pub dirs: u64,
    /// Sockets, FIFOs and device nodes seen in the source and skipped: no copy
    /// can carry them.
    pub special: u64,
}

/// Contents AND extended-attribute names: [`compare_contents_tree`] then
/// [`compare_xattr_names`]. This is what "verified" means for a copy.
///
/// # Errors
///
/// [`CopyError::Mismatch`] for a difference, [`CopyError::Io`] for a side
/// that could not be read.
pub fn compare_tree(source: &Path, destination: &Path) -> Result<VerifyTally, CopyError> {
    let tally = compare_contents_tree(source, destination)?;
    compare_xattr_names(source, destination)?;
    Ok(tally)
}

/// Compares two trees (or two files) by type, size, contents and symlink
/// target, recursively. Blind to extended attributes; see [`compare_tree`].
///
/// Every regular file is read in full on both sides. Symlinks are compared by
/// target, never followed — a symlink *is* its target string. Sockets, FIFOs
/// and device nodes are counted in [`VerifyTally::special`] and skipped.
///
/// # Errors
///
/// As [`compare_tree`].
pub fn compare_contents_tree(source: &Path, destination: &Path) -> Result<VerifyTally, CopyError> {
    let mut tally = VerifyTally::default();
    verify_tree(source, destination, &mut tally)?;
    Ok(tally)
}

fn verify_tree(source: &Path, destination: &Path, tally: &mut VerifyTally) -> Result<(), CopyError> {
    let source_meta = fs::symlink_metadata(source).map_err(|error| CopyError::io(source, error))?;
    let destination_meta = fs::symlink_metadata(destination).map_err(|error| CopyError::io(destination, error))?;

    let source_type = source_meta.file_type();
    let destination_type = destination_meta.file_type();

    if source_type.is_symlink() {
        if !destination_type.is_symlink() {
            return Err(CopyError::mismatch(
                destination,
                "the source is a symlink and the copy is not",
            ));
        }
        let a = fs::read_link(source).map_err(|error| CopyError::io(source, error))?;
        let b = fs::read_link(destination).map_err(|error| CopyError::io(destination, error))?;
        if a != b {
            return Err(CopyError::mismatch(destination, "symlink target differs"));
        }
        tally.files = tally.files.saturating_add(1);
        return Ok(());
    }

    if source_type.is_dir() {
        if !destination_type.is_dir() {
            return Err(CopyError::mismatch(
                destination,
                "the source is a directory and the copy is not",
            ));
        }
        tally.dirs = tally.dirs.saturating_add(1);
        return verify_directory(source, destination, tally);
    }

    if source_type.is_file() {
        if !destination_type.is_file() {
            return Err(CopyError::mismatch(
                destination,
                "the source is a regular file and the copy is not",
            ));
        }
        if source_meta.len() != destination_meta.len() {
            return Err(CopyError::mismatch(
                destination,
                format!("size differs: {} vs {}", source_meta.len(), destination_meta.len()),
            ));
        }
        compare_files(source, destination)?;
        tally.files = tally.files.saturating_add(1);
        tally.bytes = tally.bytes.saturating_add(source_meta.len());
        return Ok(());
    }

    if is_special(&source_meta) {
        tally.special = tally.special.saturating_add(1);
        return Ok(());
    }

    Err(CopyError::mismatch(source, "unrecognized file type"))
}

fn verify_directory(source: &Path, destination: &Path, tally: &mut VerifyTally) -> Result<(), CopyError> {
    let mut source_names = read_names(source)?;
    let mut destination_names = read_names(destination)?;
    source_names.sort_unstable();
    destination_names.sort_unstable();

    // Name sets before recursing, so "the copy is missing a file" is reported
    // as exactly that rather than as a confusing failure ten levels down.
    if source_names != destination_names {
        let missing: Vec<&PathBuf> = source_names
            .iter()
            .filter(|name| destination_names.binary_search(name).is_err())
            .take(3)
            .collect();
        let extra: Vec<&PathBuf> = destination_names
            .iter()
            .filter(|name| source_names.binary_search(name).is_err())
            .take(3)
            .collect();
        return Err(CopyError::mismatch(
            destination,
            format!(
                "directory contents differ ({} entries in the source, {} in the copy; missing {missing:?}, unexpected {extra:?})",
                source_names.len(),
                destination_names.len(),
            ),
        ));
    }

    for name in source_names {
        verify_tree(&source.join(&name), &destination.join(&name), tally)?;
    }
    Ok(())
}

fn read_names(directory: &Path) -> Result<Vec<PathBuf>, CopyError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| CopyError::io(directory, error))? {
        let entry = entry.map_err(|error| CopyError::io(directory, error))?;
        names.push(PathBuf::from(entry.file_name()));
    }
    Ok(names)
}

/// Streams two regular files and compares them block by block.
///
/// # Errors
///
/// [`CopyError::Mismatch`] naming the byte offset of the first differing
/// block, or [`CopyError::Io`].
pub fn compare_files(source: &Path, destination: &Path) -> Result<(), CopyError> {
    let mut a = fs::File::open(source).map_err(|error| CopyError::io(source, error))?;
    let mut b = fs::File::open(destination).map_err(|error| CopyError::io(destination, error))?;
    let mut buffer_a = vec![0_u8; COMPARE_CHUNK];
    let mut buffer_b = vec![0_u8; COMPARE_CHUNK];
    let mut offset = 0_u64;

    loop {
        let read_a = read_full(&mut a, &mut buffer_a).map_err(|error| CopyError::io(source, error))?;
        let read_b = read_full(&mut b, &mut buffer_b).map_err(|error| CopyError::io(destination, error))?;
        if read_a != read_b {
            return Err(CopyError::mismatch(
                destination,
                format!("length differs near byte {offset}"),
            ));
        }
        if read_a == 0 {
            return Ok(());
        }
        if buffer_a[..read_a] != buffer_b[..read_b] {
            return Err(CopyError::mismatch(
                destination,
                format!("contents differ near byte {offset}"),
            ));
        }
        offset = offset.saturating_add(read_a as u64);
    }
}

/// True when two files' bytes differ. Streaming rather than hashing: both
/// sides are local, so reading both costs the same I/O and has no collision to
/// argue about.
///
/// # Errors
///
/// Open or read failure on either side.
pub fn contents_differ(a: &Path, b: &Path) -> io::Result<bool> {
    let mut left = fs::File::open(a)?;
    let mut right = fs::File::open(b)?;
    let mut buffer_a = vec![0_u8; COMPARE_CHUNK];
    let mut buffer_b = vec![0_u8; COMPARE_CHUNK];
    loop {
        let read_a = read_full(&mut left, &mut buffer_a)?;
        let read_b = read_full(&mut right, &mut buffer_b)?;
        if read_a != read_b {
            return Ok(true);
        }
        if read_a == 0 {
            return Ok(false);
        }
        if buffer_a[..read_a] != buffer_b[..read_b] {
            return Ok(true);
        }
    }
}

/// Fills `buffer` unless EOF is reached first.
///
/// `Read::read` may return short at any time; comparing two short reads of
/// different lengths from identical files would be a false mismatch.
fn read_full(file: &mut fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn is_special(metadata: &Metadata) -> bool {
    let file_type = metadata.file_type();
    file_type.is_socket() || file_type.is_fifo() || file_type.is_block_device() || file_type.is_char_device()
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

/// An extended attribute macOS applies and reapplies on its own. It tracks
/// which process wrote a file and is regenerated at the destination, so
/// comparing it would fail every faithful copy.
const SELF_MANAGED_XATTR: &str = "com.apple.provenance";

/// Proves the copy carried the extended attributes, not just the bytes.
///
/// A copy stripped of every xattr has identical contents and passes
/// [`compare_contents_tree`]; this is the check that sees it. Resource forks
/// ride here too, as `com.apple.ResourceFork`.
///
/// NAMES are compared, not values: wholesale loss — every attribute gone
/// because the copy went through a tool that cannot carry them — is the
/// failure this has actually seen. A truncated attribute value would not be
/// caught.
///
/// A SUBSET test, not equality: only LOSS is a failure. The destination
/// legitimately gains attributes (`com.apple.macl`, minted by TCC when a path
/// came from a file picker).
///
/// Symlinks are read as themselves (`XATTR_NOFOLLOW`), so a dangling symlink
/// is not an error and a link is never credited with its target's attributes.
///
/// # Errors
///
/// [`CopyError::Mismatch`] naming up to three missing `"./relative: name"`
/// entries, or [`CopyError::Io`] if a side cannot be walked or listed.
pub fn compare_xattr_names(source: &Path, destination: &Path) -> Result<(), CopyError> {
    let expected = attribute_set(source)?;
    let actual = attribute_set(destination)?;
    let missing: Vec<&String> = expected.difference(&actual).take(3).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(CopyError::mismatch(
        destination,
        format!(
            "the copy is missing {} of the source's {} extended attributes, including {missing:?}",
            expected.difference(&actual).count(),
            expected.len(),
        ),
    ))
}

/// Every `"<relative path>: <attribute name>"` in a tree, root included as
/// `"."`. A set: directory order differs between a source and a fresh copy.
fn attribute_set(root: &Path) -> Result<BTreeSet<String>, CopyError> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), ".".to_owned())];
    while let Some((path, label)) = stack.pop() {
        for name in list_xattr_names(&path)? {
            if name != SELF_MANAGED_XATTR {
                set.insert(format!("{label}: {name}"));
            }
        }
        let meta = fs::symlink_metadata(&path).map_err(|error| CopyError::io(&path, error))?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path).map_err(|error| CopyError::io(&path, error))? {
                let entry = entry.map_err(|error| CopyError::io(&path, error))?;
                let child_label = format!("{label}/{}", entry.file_name().to_string_lossy());
                stack.push((entry.path(), child_label));
            }
        }
    }
    Ok(set)
}

/// The extended-attribute names on `path` itself (symlinks not followed).
/// A filesystem without xattr support reports none.
fn list_xattr_names(path: &Path) -> Result<Vec<String>, CopyError> {
    let c_path = c_path(path)?;
    loop {
        let size = listxattr_raw(&c_path, None).map_err(|error| xattr_error(path, &error))?;
        let Some(size) = size else { return Ok(Vec::new()) };
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buffer = vec![0_u8; size];
        match listxattr_raw(&c_path, Some(&mut buffer)) {
            Ok(Some(written)) => {
                buffer.truncate(written);
                return Ok(buffer
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .collect());
            }
            Ok(None) => return Ok(Vec::new()),
            // The list grew between the two calls; size it again.
            Err(error) if error.raw_os_error() == Some(libc::ERANGE) => {}
            Err(error) => return Err(xattr_error(path, &error)),
        }
    }
}

fn xattr_error(path: &Path, error: &io::Error) -> CopyError {
    CopyError::mismatch(path, format!("could not read extended attributes: {error}"))
}

/// One `listxattr(2)` call with `XATTR_NOFOLLOW`. `None` for a buffer asks for
/// the size. `Ok(None)` means the filesystem does not support xattrs.
#[allow(unsafe_code)]
fn listxattr_raw(path: &std::ffi::CStr, buffer: Option<&mut [u8]>) -> io::Result<Option<usize>> {
    let (pointer, length) = match buffer {
        Some(buffer) => (buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()),
        None => (std::ptr::null_mut(), 0),
    };
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
    // `pointer` is either null with `length` 0 (the documented size query) or
    // points at a live, exclusively borrowed buffer of exactly `length` bytes,
    // which the kernel writes at most `length` bytes into.
    let result = unsafe { libc::listxattr(path.as_ptr(), pointer, length, libc::XATTR_NOFOLLOW) };
    if result >= 0 {
        return Ok(Some(usize::try_from(result).unwrap_or(0)));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOTSUP) {
        return Ok(None);
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::ditto;

    fn set_xattr(path: &Path, name: &str, value: &str) {
        let status = Command::new("/usr/bin/xattr")
            .args(["-w", name, value])
            .arg(path)
            .status()
            .expect("xattr");
        assert!(status.success(), "fixture xattr must be written");
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("sub")).expect("mkdir");
        fs::write(root.join("doc.txt"), b"payload").expect("write");
        fs::write(root.join("sub/other.txt"), vec![b'z'; 4096]).expect("write");
        set_xattr(&root.join("doc.txt"), "user.rdirstat.probe", "value");
    }

    #[test]
    fn a_one_byte_change_is_caught() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (source, destination) = (scratch.path().join("s"), scratch.path().join("d"));
        tree(&source);
        ditto(&source, &destination).expect("ditto");
        compare_tree(&source, &destination).expect("identical copy verifies");

        let mut corrupted = vec![b'z'; 4096];
        corrupted[2048] = b'q';
        fs::write(destination.join("sub/other.txt"), &corrupted).expect("write");
        let error = compare_tree(&source, &destination).expect_err("corruption is caught");
        assert!(
            matches!(&error, CopyError::Mismatch { reason, .. } if reason.contains("contents differ near byte 0")),
            "got {error:?}"
        );
    }

    #[test]
    fn a_lost_xattr_is_caught_and_a_gained_one_is_not() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (source, destination) = (scratch.path().join("s"), scratch.path().join("d"));
        tree(&source);
        ditto(&source, &destination).expect("ditto");

        set_xattr(&destination.join("sub/other.txt"), "com.apple.rdirstat.granted", "yes");
        compare_tree(&source, &destination).expect("a gained attribute is not a loss");

        let status = Command::new("/usr/bin/xattr")
            .args(["-d", "user.rdirstat.probe"])
            .arg(destination.join("doc.txt"))
            .status()
            .expect("xattr");
        assert!(status.success());
        compare_contents_tree(&source, &destination).expect("contents alone still match");
        let error = compare_tree(&source, &destination).expect_err("the loss is caught");
        let CopyError::Mismatch { reason, .. } = &error else {
            panic!("expected a mismatch, got {error:?}");
        };
        assert!(reason.contains("./doc.txt: user.rdirstat.probe"), "got {reason}");
    }

    #[test]
    fn a_dangling_symlink_does_not_fail_the_attribute_walk() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (source, destination) = (scratch.path().join("s"), scratch.path().join("d"));
        tree(&source);
        std::os::unix::fs::symlink("/nonexistent/target", source.join("dangling")).expect("symlink");
        ditto(&source, &destination).expect("ditto");
        compare_tree(&source, &destination).expect("verifies");
    }

    #[test]
    fn single_files_compare_including_their_xattrs() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let a = scratch.path().join("a");
        let b = scratch.path().join("b");
        fs::write(&a, b"same").expect("write");
        fs::write(&b, b"same").expect("write");
        compare_tree(&a, &b).expect("same bytes, no xattrs");
        set_xattr(&a, "user.only.here", "1");
        assert!(compare_tree(&a, &b).is_err(), "b lost a's attribute");
        assert!(!contents_differ(&a, &b).expect("readable"));
    }

    #[test]
    fn a_missing_file_and_a_retargeted_symlink_are_caught() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (source, destination) = (scratch.path().join("s"), scratch.path().join("d"));
        tree(&source);
        ditto(&source, &destination).expect("ditto");
        fs::remove_file(destination.join("doc.txt")).expect("remove from the copy");
        let error = compare_contents_tree(&source, &destination).expect_err("missing");
        assert!(error.to_string().contains("directory contents differ"), "got {error}");

        let (x, y) = (scratch.path().join("x"), scratch.path().join("y"));
        std::os::unix::fs::symlink("one", &x).expect("symlink");
        std::os::unix::fs::symlink("two", &y).expect("symlink");
        let error = compare_contents_tree(&x, &y).expect_err("targets differ");
        assert!(error.to_string().contains("symlink target"), "got {error}");
    }

    #[test]
    fn short_reads_do_not_produce_a_false_mismatch() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let (a, b) = (scratch.path().join("a"), scratch.path().join("b"));
        let payload: Vec<u8> = (0..(COMPARE_CHUNK * 2 + 517))
            .map(|i| u8::try_from(i % 251).unwrap_or_default())
            .collect();
        fs::write(&a, &payload).expect("write");
        fs::write(&b, &payload).expect("write");
        compare_files(&a, &b).expect("identical multi-chunk files compare equal");
        assert!(!contents_differ(&a, &b).expect("readable"));
    }
}
