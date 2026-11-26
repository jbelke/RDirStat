//! Landing one item at a destination: clone a known duplicate or `ditto` it,
//! then prove it. A failure never deletes what was written; it names it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::{CopyError, VerifyTally, clone_file, compare_contents_tree, compare_tree, contents_differ, ditto};

/// How an item reached its destination. Recorded on the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// `ditto`, then a full compare (contents and xattr names).
    Copy,
    /// `clonefile(2)` of an already-landed destination file with the same
    /// size and SHA-256, then a contents compare against the source.
    Clone,
}

/// A landed, verified item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landed {
    pub method: Method,
    pub tally: VerifyTally,
}

/// Puts `source` at `destination` and verifies it against `source`.
///
/// When `clone_from` names a destination file the catalog says has the same
/// size and SHA-256 as `source`, it is first compared byte for byte against
/// `source`. Only if it still matches is the new path a `clonefile(2)` of it
/// (zero new blocks). A catalog row gone stale — the landed copy was edited,
/// replaced, or removed — costs a read, never the item: it falls back to
/// `ditto`, as does a volume that cannot clone or any clone failure that left
/// nothing at `destination`. The result is still compared against `source`.
///
/// `cancel` is checked before anything is written and again before the
/// verify. `source` is only read. `destination` must not exist: nothing is
/// ever replaced.
///
/// # Errors
///
/// - [`CopyError::AlreadyExists`] if `destination` exists (nothing written).
/// - [`CopyError::Cancelled`] if cancelled before anything was written.
/// - [`CopyError::Partial`] once anything may have been written: the copy or
///   clone failed, the verify failed, or the run was cancelled. The partial
///   is left on disk and its path is in the error.
/// - Any other variant when the failure happened before a byte was written.
pub fn land(
    source: &Path,
    destination: &Path,
    clone_from: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Landed, CopyError> {
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(CopyError::AlreadyExists {
            path: destination.to_path_buf(),
        });
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(CopyError::Cancelled);
    }

    let clone_from = clone_from.filter(|existing| clone_source_matches(source, existing));
    let cloned = match clone_from.map(|existing| clone_file(existing, destination)) {
        Some(Ok(())) => true,
        None => false,
        Some(Err(error @ CopyError::AlreadyExists { .. })) => return Err(error),
        Some(Err(error)) => {
            if std::fs::symlink_metadata(destination).is_ok() {
                return Err(partial(destination, error));
            }
            false
        }
    };
    let method = if cloned {
        Method::Clone
    } else {
        ditto(source, destination).map_err(|error| partial_or(destination, error))?;
        Method::Copy
    };

    if cancel.load(Ordering::Relaxed) {
        return Err(partial(destination, CopyError::Cancelled));
    }

    // A clone carries the existing file's xattrs, not the source's, so only
    // its contents can be held to the source. A copy is held to both.
    let verified = match method {
        Method::Clone => compare_contents_tree(source, destination),
        Method::Copy => compare_tree(source, destination),
    };
    let tally = verified.map_err(|error| partial(destination, error))?;
    Ok(Landed { method, tally })
}

/// `existing` is a regular file whose bytes equal `source`'s. Anything
/// unreadable or different is not a clone source.
fn clone_source_matches(source: &Path, existing: &Path) -> bool {
    let is_file = std::fs::symlink_metadata(existing).is_ok_and(|meta| meta.file_type().is_file());
    is_file && matches!(contents_differ(source, existing), Ok(false))
}

fn partial(destination: &Path, cause: CopyError) -> CopyError {
    CopyError::Partial {
        path: destination.to_path_buf(),
        cause: Box::new(cause),
    }
}

/// [`partial`] if anything is now at `destination`, otherwise `cause` as is.
fn partial_or(destination: &Path, cause: CopyError) -> CopyError {
    if std::fs::symlink_metadata(destination).is_ok() {
        partial(destination, cause)
    } else {
        cause
    }
}

/// The path a [`CopyError::Partial`] left on disk, if this is one.
#[must_use]
pub fn partial_path(error: &CopyError) -> Option<&PathBuf> {
    match error {
        CopyError::Partial { path, .. } => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn a_new_item_is_copied_and_verified() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("src.bin");
        std::fs::write(&source, vec![9_u8; 70_000]).expect("write");
        let destination = scratch.path().join("Backups/label/src.bin");
        let landed = land(&source, &destination, None, &never()).expect("lands");
        assert_eq!(landed.method, Method::Copy);
        assert_eq!(landed.tally.bytes, 70_000);
        assert_eq!(
            std::fs::read(&source).expect("read"),
            vec![9_u8; 70_000],
            "source untouched"
        );
    }

    #[test]
    fn a_duplicate_is_cloned_from_the_landed_copy() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("dupe.bin");
        std::fs::write(&source, b"same bytes").expect("write");
        let landed_copy = scratch.path().join("Backups/a/first.bin");
        land(&source, &landed_copy, None, &never()).expect("first lands");
        let second = scratch.path().join("Backups/a/second.bin");
        let landed = land(&source, &second, Some(&landed_copy), &never()).expect("second lands");
        // APFS clones; any other temp filesystem falls back to a copy.
        assert!(matches!(landed.method, Method::Clone | Method::Copy));
        assert_eq!(std::fs::read(&second).expect("read"), b"same bytes");
    }

    #[test]
    fn a_stale_clone_source_is_not_cloned_and_the_item_is_copied() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("source.bin");
        std::fs::write(&source, b"what the source holds").expect("write");
        // Same length, other bytes: a landed copy edited in place since its
        // receipt.
        let stale = scratch.path().join("stale.bin");
        std::fs::write(&stale, b"what the source HOLDS").expect("write");
        let destination = scratch.path().join("out/dest.bin");

        let landed = land(&source, &destination, Some(&stale), &never()).expect("lands by copy");
        assert_eq!(landed.method, Method::Copy);
        assert_eq!(std::fs::read(&destination).expect("read"), b"what the source holds");
        assert_eq!(
            std::fs::read(&stale).expect("read"),
            b"what the source HOLDS",
            "untouched"
        );
    }

    #[test]
    fn a_vanished_clone_source_falls_back_to_a_copy() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("source.bin");
        std::fs::write(&source, b"bytes").expect("write");
        let destination = scratch.path().join("dest.bin");
        let landed =
            land(&source, &destination, Some(&scratch.path().join("gone.bin")), &never()).expect("lands by copy");
        assert_eq!(landed.method, Method::Copy);
        assert_eq!(std::fs::read(&destination).expect("read"), b"bytes");
    }

    #[test]
    fn an_existing_destination_is_refused_and_untouched() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("s");
        let destination = scratch.path().join("d");
        std::fs::write(&source, b"new").expect("write");
        std::fs::write(&destination, b"keep").expect("write");
        let error = land(&source, &destination, None, &never()).expect_err("refused");
        assert!(matches!(error, CopyError::AlreadyExists { .. }), "got {error:?}");
        assert_eq!(std::fs::read(&destination).expect("read"), b"keep");
    }

    #[test]
    fn cancel_before_writing_writes_nothing() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("s");
        std::fs::write(&source, b"x").expect("write");
        let destination = scratch.path().join("d");
        let error = land(&source, &destination, None, &AtomicBool::new(true)).expect_err("cancelled");
        assert!(matches!(error, CopyError::Cancelled), "got {error:?}");
        assert!(!destination.exists());
    }

    #[test]
    fn a_failed_ditto_that_wrote_nothing_is_not_called_partial() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let destination = scratch.path().join("d");
        let error = land(&scratch.path().join("absent"), &destination, None, &never()).expect_err("fails");
        assert!(partial_path(&error).is_none(), "got {error:?}");
        assert!(matches!(error, CopyError::Tool { .. }), "got {error:?}");
    }
}
