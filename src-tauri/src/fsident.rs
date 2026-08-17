//! Path reconstruction and identity revalidation.
//!
//! A [`DisplayPath`] is never action authority. Every Reveal and Trash resolves
//! its target from `CompletedScan::root_path` plus the components stored in the
//! arena, then **re-`lstat`s and compares identity** before touching anything.
//! The tree is a past observation; the filesystem has moved on.

use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use rdirstat_core::{
    ActionError, CompletedScan, DisplayPath, ErrorClass, Kind, NodeId, Operation, QueryError, ScanError, flags,
};

/// What a fresh `lstat` says about a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    /// `st_dev`.
    pub device: u64,
    /// `st_ino`.
    pub inode: u64,
    /// The object kind, mapped onto the arena's [`Kind`].
    pub kind: Kind,
    /// Logical size in bytes.
    pub size: u64,
    /// `st_blocks * 512`.
    pub allocated: u64,
    /// Modification time in Unix seconds.
    pub mtime: i64,
    /// `st_nlink`.
    pub links: u64,
    /// Whether any execute bit is set.
    pub executable: bool,
}

/// Maps a `std` file type onto the arena [`Kind`].
#[must_use]
pub(crate) fn kind_of(metadata: &Metadata) -> Kind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else if file_type.is_socket() {
        Kind::Socket
    } else if file_type.is_fifo() {
        Kind::Fifo
    } else if file_type.is_char_device() {
        Kind::CharDevice
    } else if file_type.is_block_device() {
        Kind::BlockDevice
    } else {
        Kind::Unknown
    }
}

/// Reads one `lstat`, without following the final symlink.
#[must_use]
pub(crate) fn observe_metadata(metadata: &Metadata) -> Observation {
    Observation {
        device: metadata.dev(),
        inode: metadata.ino(),
        kind: kind_of(metadata),
        size: metadata.len(),
        allocated: metadata.blocks().saturating_mul(512),
        mtime: metadata.mtime(),
        links: metadata.nlink(),
        executable: metadata.permissions().mode() & 0o111 != 0,
    }
}

/// `lstat`s `path`. Never follows the final component.
///
/// # Errors
///
/// Whatever `symlink_metadata` reports.
pub(crate) fn observe(path: &Path) -> io::Result<Observation> {
    std::fs::symlink_metadata(path).map(|metadata| observe_metadata(&metadata))
}

/// Classifies an `io::Error` for [`ScanError`] and [`ErrorClassCount`].
///
/// [`ErrorClassCount`]: rdirstat_core::ErrorClassCount
#[must_use]
pub(crate) fn error_class(error: &io::Error) -> ErrorClass {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ErrorClass::PermissionDenied,
        io::ErrorKind::NotFound => ErrorClass::NotFound,
        io::ErrorKind::NotADirectory => ErrorClass::NotADirectory,
        // `io::ErrorKind::FilesystemLoop` / `InvalidFilename` are still
        // unstable, so the remaining classes come from `errno` directly.
        _ => match error.raw_os_error() {
            // EMFILE / ENFILE
            Some(24 | 23) => ErrorClass::TooManyOpenFiles,
            // ELOOP
            Some(62) => ErrorClass::SymlinkLoop,
            // ENAMETOOLONG
            Some(63) => ErrorClass::NameTooLong,
            // EILSEQ
            Some(92) => ErrorClass::InvalidName,
            // EIO
            Some(5) => ErrorClass::InputOutput,
            // ESTALE / EHOSTDOWN
            Some(70 | 64) => ErrorClass::RemoteUnavailable,
            _ => ErrorClass::Other,
        },
    }
}

/// Turns an `io::Error` at `path` into a recorded, usually non-fatal
/// [`ScanError`].
#[must_use]
pub(crate) fn scan_error(path: &Path, operation: Operation, error: &io::Error) -> ScanError {
    let display = DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());
    let class = error_class(error);
    let os_code = error.raw_os_error().unwrap_or(0);
    match class {
        ErrorClass::PermissionDenied => ScanError::PermissionDenied {
            path: display,
            operation,
            os_code,
        },
        ErrorClass::NotFound => ScanError::Vanished {
            path: display,
            operation,
        },
        ErrorClass::InvalidName => ScanError::InvalidName { path: display },
        _ => ScanError::Io {
            path: display,
            operation,
            class,
            os_code,
        },
    }
}

/// Turns an `io::Error` at `path` into an [`ActionError`].
#[must_use]
pub(crate) fn action_error(path: &Path, operation: Operation, error: &io::Error) -> ActionError {
    let display = DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());
    if error.kind() == io::ErrorKind::PermissionDenied {
        ActionError::PermissionDenied {
            path: display,
            operation,
            os_code: error.raw_os_error().unwrap_or(0),
        }
    } else if error.kind() == io::ErrorKind::NotFound {
        ActionError::ChangedSinceScan { path: display }
    } else {
        ActionError::TrashFailed {
            path: display,
            reason: error.to_string(),
        }
    }
}

/// Reconstructs a node's absolute path from the arena.
///
/// The root node's name is the scan root's full path, so the walk yields an
/// absolute path without storing one per node.
///
/// # Errors
///
/// [`QueryError::UnknownNode`], [`QueryError::VirtualGroup`] (a group has no
/// path of its own), or [`QueryError::PathTooDeep`].
pub(crate) fn node_path(scan: &CompletedScan, node: NodeId) -> Result<PathBuf, QueryError> {
    let mut bytes = Vec::with_capacity(128);
    scan.tree.path_bytes(node, &mut bytes)?;
    Ok(PathBuf::from(OsStr::from_bytes(&bytes).to_os_string()))
}

/// Reconstructs a path for a *destructive or revealing* action and checks that
/// it is inside the scan root.
///
/// Rejects the scan root itself and every virtual group up front: neither can
/// be revealed or trashed, and there is no identity to revalidate for a group.
///
/// # Errors
///
/// [`ActionError::UnknownNode`], [`ActionError::NotActionable`], or
/// [`ActionError::OutsideScanRoot`].
pub(crate) fn action_path(scan: &CompletedScan, node: NodeId, allow_root: bool) -> Result<PathBuf, ActionError> {
    if node.is_virtual_group() {
        return Err(ActionError::NotActionable { node });
    }
    if !allow_root && node == scan.root {
        return Err(ActionError::NotActionable { node });
    }
    if !scan.tree.contains(node) {
        return Err(ActionError::UnknownNode { node });
    }
    let path = node_path(scan, node).map_err(|error| match error {
        QueryError::UnknownNode { node } => ActionError::UnknownNode { node },
        QueryError::VirtualGroup { node } => ActionError::NotActionable { node },
        other => ActionError::Internal(other.to_string()),
    })?;

    // A reconstructed path is only trusted once it is absolute, free of `..`,
    // and lexically under the recorded root.
    if !path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err(ActionError::OutsideScanRoot {
            path: DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes()),
        });
    }
    if !path.starts_with(&scan.root_path) {
        return Err(ActionError::OutsideScanRoot {
            path: DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes()),
        });
    }
    Ok(path)
}

/// Resolves symlinked ancestors and confirms the target is still under the
/// scan root.
///
/// A component that became a symlink *after* the scan is exactly how a
/// path-based action escapes its root, so the parent directory is canonicalized
/// (which follows every symlink) and compared against the canonical root.
///
/// # Errors
///
/// [`ActionError::OutsideScanRoot`] when the resolved parent escapes,
/// [`ActionError::ChangedSinceScan`] when the parent no longer exists.
pub(crate) fn confirm_within_root(scan: &CompletedScan, path: &Path) -> Result<(), ActionError> {
    let display = || DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());
    let Some(parent) = path.parent() else {
        return Err(ActionError::OutsideScanRoot { path: display() });
    };
    let real_parent = std::fs::canonicalize(parent).map_err(|_| ActionError::ChangedSinceScan { path: display() })?;
    let real_root = std::fs::canonicalize(&scan.root_path).map_err(|error| ActionError::Internal(error.to_string()))?;
    if real_parent.starts_with(&real_root) {
        Ok(())
    } else {
        Err(ActionError::OutsideScanRoot { path: display() })
    }
}

/// Re-`lstat`s `path` and rejects it unless it is still the object the arena
/// recorded.
///
/// Checks, in order: the object exists; it is on the volume the scan ran
/// against; its kind matches; and — for a regular file that is not a repeated
/// hard link — its size and mtime match. Any disagreement is
/// [`ActionError::ChangedSinceScan`], never a silent move of the wrong item.
///
/// The `(dev, ino)` pair is returned so the caller can bind it into the
/// confirmation token; the arena does not store an inode (a [`Node`] is 48
/// bytes) so preview-to-confirm is where inode stability is enforced.
///
/// [`Node`]: rdirstat_core::Node
///
/// # Errors
///
/// [`ActionError::ChangedSinceScan`] or [`ActionError::PermissionDenied`].
pub(crate) fn revalidate(scan: &CompletedScan, node: NodeId, path: &Path) -> Result<Observation, ActionError> {
    let display = || DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());
    let recorded = scan.tree.node(node).copied().ok_or(ActionError::UnknownNode { node })?;

    let observed = observe(path).map_err(|error| action_error(path, Operation::Metadata, &error))?;

    if !scan.options.cross_filesystems && observed.device != scan.volume.device {
        return Err(ActionError::ChangedSinceScan { path: display() });
    }
    if observed.kind != recorded.kind() {
        return Err(ActionError::ChangedSinceScan { path: display() });
    }
    let repeated_link = recorded.has_flags(flags::HARD_LINK_REPEAT);
    if recorded.kind() == Kind::File
        && !repeated_link
        && (observed.size != recorded.size || observed.mtime != recorded.mtime)
    {
        return Err(ActionError::ChangedSinceScan { path: display() });
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_from_real_filesystem_objects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"hello").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");

        assert_eq!(observe(dir.path()).expect("stat dir").kind, Kind::Directory);
        assert_eq!(observe(&file).expect("stat file").kind, Kind::File);
        assert_eq!(observe(&link).expect("stat link").kind, Kind::Symlink);
    }

    #[test]
    fn observation_reports_size_and_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"0123456789").expect("write");
        let observed = observe(&file).expect("stat");
        assert_eq!(observed.size, 10);
        assert!(observed.inode > 0);
        assert!(observed.device > 0);
        assert!(!observed.executable);
    }

    #[test]
    fn a_missing_path_classifies_as_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = observe(&dir.path().join("nope")).expect_err("must fail");
        assert_eq!(error_class(&error), ErrorClass::NotFound);
        assert!(matches!(
            scan_error(&dir.path().join("nope"), Operation::Metadata, &error),
            ScanError::Vanished { .. }
        ));
    }
}
