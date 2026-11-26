//! `clonefile(2)`: a second path to the same blocks, on APFS.

use std::io;
use std::path::Path;

use crate::{CopyError, c_path};

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`; not exported by `libc` 0.2.
const CLONE_NOFOLLOW: u32 = 0x0001;

/// Makes `new_path` an APFS clone of `existing`: same contents, metadata and
/// xattrs, zero new data blocks until one side is written.
///
/// Used for duplicates: when the catalog already holds a landed destination
/// file with the same size and SHA-256, the new mirror path is a clone of it.
/// `existing` is only read. Parent directories of `new_path` are created. A
/// symlink at `existing` is cloned as a symlink (`CLONE_NOFOLLOW`).
///
/// # Errors
///
/// [`CopyError::AlreadyExists`] if anything is at `new_path` (it is never
/// replaced), [`CopyError::CloneUnsupported`] for `ENOTSUP` / `EXDEV` — the
/// caller falls back to [`crate::ditto`] — and [`CopyError::Io`] otherwise.
pub fn clone_file(existing: &Path, new_path: &Path) -> Result<(), CopyError> {
    if std::fs::symlink_metadata(new_path).is_ok() {
        return Err(CopyError::AlreadyExists {
            path: new_path.to_path_buf(),
        });
    }
    if let Some(parent) = new_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| CopyError::io(parent, error))?;
    }
    let source = c_path(existing)?;
    let target = c_path(new_path)?;
    match clonefile_raw(&source, &target) {
        Ok(()) => Ok(()),
        Err(error) => Err(match error.raw_os_error() {
            Some(libc::EEXIST) => CopyError::AlreadyExists {
                path: new_path.to_path_buf(),
            },
            Some(libc::ENOTSUP | libc::EXDEV) => CopyError::CloneUnsupported {
                path: new_path.to_path_buf(),
                source: error,
            },
            _ => CopyError::io(new_path, error),
        }),
    }
}

/// One `clonefile(2)` call with `CLONE_NOFOLLOW`. The kernel refuses with
/// `EEXIST` if `target` exists, so the existence check above is advisory and
/// this is the authority.
#[allow(unsafe_code)]
fn clonefile_raw(source: &std::ffi::CStr, target: &std::ffi::CStr) -> io::Result<()> {
    // SAFETY: both arguments are valid NUL-terminated strings borrowed for the
    // duration of the call; clonefile reads them and writes no user memory.
    let result = unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), CLONE_NOFOLLOW) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compare_tree, identity};

    #[test]
    fn a_clone_has_the_same_bytes_and_a_new_inode() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let existing = scratch.path().join("landed.bin");
        std::fs::write(&existing, vec![3_u8; 1 << 20]).expect("write");
        let new_path = scratch.path().join("Backups/label/a/b/dupe.bin");

        match clone_file(&existing, &new_path) {
            Ok(()) => {}
            // The temp dir is APFS on every supported Mac; anything else is an
            // environment this test cannot speak for.
            Err(CopyError::CloneUnsupported { .. }) => return,
            Err(other) => panic!("clone failed: {other:?}"),
        }
        compare_tree(&existing, &new_path).expect("a clone is identical");
        assert_ne!(
            identity(&existing).expect("id").ino,
            identity(&new_path).expect("id").ino,
            "a clone is a separate file, not a hard link"
        );
    }

    #[test]
    fn an_existing_target_is_refused_and_left_alone() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let existing = scratch.path().join("a");
        let occupied = scratch.path().join("b");
        std::fs::write(&existing, b"new").expect("write");
        std::fs::write(&occupied, b"keep me").expect("write");
        let error = clone_file(&existing, &occupied).expect_err("refused");
        assert!(matches!(error, CopyError::AlreadyExists { .. }), "got {error:?}");
        assert_eq!(std::fs::read(&occupied).expect("read"), b"keep me");
    }

    #[test]
    fn a_missing_source_is_an_io_error() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let error = clone_file(&scratch.path().join("absent"), &scratch.path().join("x")).expect_err("fails");
        assert!(matches!(error, CopyError::Io { .. }), "got {error:?}");
    }
}
