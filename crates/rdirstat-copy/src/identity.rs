//! The identity re-stat: is this still the object the scan saw?

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a backup re-checks before copying an item.
///
/// `(dev, ino)` names the object; `size` and `mtime_ns` say whether its
/// contents could have changed. Any field differing from the scan's value is
/// `ChangedSinceScan` for that item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    /// Logical length in bytes (`st_size`).
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
}

/// `lstat(2)` of `path`. A symlink is reported as itself, never followed.
///
/// # Errors
///
/// Whatever `lstat` reports: missing path, permission denied.
pub fn identity(path: &Path) -> std::io::Result<Identity> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Identity {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_metadata_and_tracks_a_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"hello").expect("write");
        let first = identity(&file).expect("identity");
        let meta = std::fs::symlink_metadata(&file).expect("meta");
        assert_eq!(first.dev, meta.dev());
        assert_eq!(first.ino, meta.ino());
        assert_eq!(first.size, 5);
        assert!(first.mtime_ns > 0);

        std::fs::write(&file, b"hello, world").expect("rewrite");
        let second = identity(&file).expect("identity");
        assert_eq!(second.ino, first.ino, "same object");
        assert_ne!(second, first, "size changed");
    }

    #[test]
    fn identity_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, b"0123456789").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert_ne!(
            identity(&link).expect("link").ino,
            identity(&target).expect("target").ino
        );

        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink("/nonexistent/x", &dangling).expect("symlink");
        identity(&dangling).expect("a dangling link still has an identity");
    }
}
