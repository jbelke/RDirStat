//! `ditto(1)`, the only copier.
//!
//! Not `cp` and not `rsync`: the system `rsync` on macOS 15 is openrsync,
//! which rejects `--xattrs` and `--acls`, and `cp` has no single flag set that
//! promises extended attributes, ACLs and resource forks. `ditto` is Apple's
//! own copier and carries all three.

use std::path::Path;
use std::process::Command;

use crate::{CopyError, run};

const DITTO: &str = "/usr/bin/ditto";

/// The metadata-preserving flags. Modern `ditto` implies them for an
/// APFS-to-APFS copy, but stating them means the behaviour does not change on
/// a different destination filesystem, and it documents what the copy keeps.
const PRESERVE: [&str; 3] = ["--rsrc", "--extattr", "--acl"];

fn create_parent(destination: &Path) -> Result<(), CopyError> {
    if let Some(parent) = destination.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| CopyError::io(parent, error))?;
    }
    Ok(())
}

/// Copies `source` onto `destination` with `ditto`, keeping xattrs, ACLs and
/// resource forks. Creates `destination`'s parent directories first.
///
/// `ditto src dst` writes `src`'s *contents* into `dst` when `src` is a
/// directory, so `destination` is the full target path, not its parent.
///
/// A failure leaves whatever `ditto` wrote where it is; nothing is cleaned up.
///
/// # Errors
///
/// [`CopyError::Io`] if a parent cannot be created, [`CopyError::Spawn`] if
/// `ditto` cannot start, [`CopyError::Tool`] if it exits non-zero.
pub fn ditto(source: &Path, destination: &Path) -> Result<(), CopyError> {
    create_parent(destination)?;
    run("ditto", Command::new(DITTO).args(PRESERVE).arg(source).arg(destination)).map(drop)
}

/// `ditto --bom <bom>`: copies only the objects named in the bill of materials,
/// from `source` into `destination`, with the same metadata flags as [`ditto`].
///
/// `ditto` exits 0 for a bom naming a path that is not in the source; the
/// caller reconciles against the filesystem afterwards.
///
/// # Errors
///
/// As [`ditto`].
pub fn ditto_bom(source: &Path, destination: &Path, bom: &Path) -> Result<(), CopyError> {
    run(
        "ditto",
        Command::new(DITTO)
            .arg("--bom")
            .arg(bom)
            .args(PRESERVE)
            .arg(source)
            .arg(destination),
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare_tree;

    #[test]
    fn a_tree_round_trips_with_its_metadata_and_parents_are_created() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("source");
        std::fs::create_dir_all(source.join("sub")).expect("mkdir");
        std::fs::write(source.join("a.txt"), b"alpha").expect("write");
        std::fs::write(source.join("sub/b.bin"), vec![7_u8; 300_000]).expect("write");
        std::os::unix::fs::symlink("a.txt", source.join("link")).expect("symlink");
        let status = Command::new("/usr/bin/xattr")
            .args(["-w", "user.rdirstat.probe", "kept"])
            .arg(source.join("a.txt"))
            .status()
            .expect("xattr");
        assert!(status.success());

        let destination = scratch.path().join("deep/er/destination");
        ditto(&source, &destination).expect("ditto");
        let tally = compare_tree(&source, &destination).expect("a ditto copy verifies");
        assert_eq!(tally.files, 3, "two files and one symlink");
        assert_eq!(tally.bytes, 5 + 300_000);
        assert_eq!(tally.dirs, 2);
    }

    #[test]
    fn a_single_file_copies_to_a_new_name() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("one.txt");
        std::fs::write(&source, b"payload").expect("write");
        let destination = scratch.path().join("mirror/x/one.txt");
        ditto(&source, &destination).expect("ditto");
        compare_tree(&source, &destination).expect("verifies");
    }

    #[test]
    fn a_missing_source_is_a_tool_error() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let error = ditto(&scratch.path().join("absent"), &scratch.path().join("out")).expect_err("fails");
        assert!(
            matches!(error, CopyError::Tool { program: "ditto", .. }),
            "got {error:?}"
        );
        assert!(error.to_string().starts_with("ditto exited "), "got {error}");
    }
}
