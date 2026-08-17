//! The one-directory reading contract.
//!
//! Reading a directory and scheduling the traversal are separate concerns
//! (docs/02-SCANNER.md#reader-contract). A reader is handed one [`DirHandle`],
//! emits bounded [`RawEntryBatch`]es, and never touches the arena: the builder
//! owns paths and ids. That separation is what lets the sequential and the
//! bounded parallel scheduler drive the *same* reader and be compared against
//! each other.

use core::fmt;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use rdirstat_core::{ErrorClass, NodeId, Operation, ScanError};

use crate::cancel::CancelToken;
use crate::entry::RawEntryBatch;

/// Darwin `SF_FIRMLINK`, from `sys/stat.h`.
///
/// Declared here because `libc` does not export it. A firmlink is never
/// descended during a root-volume scan; the `/System/Volumes/Data` exclusion is
/// defense in depth for readers that cannot see the flag
/// (docs/02-SCANNER.md#traversal-rules).
pub const SF_FIRMLINK: u32 = 0x0080_0000;

/// One directory the scheduler wants read.
///
/// Carries the id the builder already assigned, the device observed when the
/// entry was enumerated, and the depth, so a reader can recheck kind and device
/// before descending and the scheduler can refuse an untrusted depth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirHandle {
    path: PathBuf,
    node: NodeId,
    device: u64,
    depth: u32,
}

impl DirHandle {
    /// A handle for `path`, already interned in the arena as `node`.
    #[must_use]
    pub const fn new(path: PathBuf, node: NodeId, device: u64, depth: u32) -> Self {
        Self {
            path,
            node,
            device,
            depth,
        }
    }

    /// The absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The arena node for this directory.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// The device observed when this directory was enumerated. A reader
    /// rechecks against it so a directory replaced between enumeration and
    /// descent is not walked by accident.
    #[must_use]
    pub const fn device(&self) -> u64 {
        self.device
    }

    /// Depth below the scan root; the root itself is zero.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }
}

/// Reading one directory failed, or was abandoned.
///
/// This is a *per-directory* outcome. It is recorded and the scan continues;
/// only [`ReadDirError::Aborted`] carries something the builder already decided
/// was fatal.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReadDirError {
    /// The OS refused, or the kind/device recheck disagreed with enumeration.
    #[error("{operation:?} failed: {class:?} (os error {os_code})")]
    Os {
        /// What was being attempted.
        operation: Operation,
        /// Stable class.
        class: ErrorClass,
        /// Raw `errno`, or `0` when the failure was not an OS error.
        os_code: i32,
    },

    /// Cancellation was observed. Nothing further is read.
    #[error("cancelled")]
    Cancelled,

    /// The sink returned [`ControlFlow::Break`]: the builder hit a fatal error
    /// such as an exhausted arena.
    #[error("scan aborted: {0}")]
    Aborted(#[from] ScanError),
}

impl ReadDirError {
    /// Builds an `Os` variant from an [`io::Error`], classifying the raw code.
    #[must_use]
    pub fn from_io(operation: Operation, error: &io::Error) -> Self {
        let os_code = error.raw_os_error().unwrap_or(0);
        Self::Os {
            operation,
            class: classify_os_error(error),
            os_code,
        }
    }

    /// The stable class, or `None` for cancellation and builder aborts.
    #[must_use]
    pub const fn class(&self) -> Option<ErrorClass> {
        match self {
            Self::Os { class, .. } => Some(*class),
            Self::Cancelled | Self::Aborted(_) => None,
        }
    }
}

/// Maps an [`io::Error`] onto the stable [`ErrorClass`] taxonomy.
///
/// Classes are stable across macOS releases so error counts stay comparable
/// between scans, and so the UI keys Full Disk Access guidance off
/// [`ErrorClass::PermissionDenied`] rather than off a number.
#[must_use]
pub fn classify_os_error(error: &io::Error) -> ErrorClass {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => ErrorClass::PermissionDenied,
        Some(libc::ENOENT) => ErrorClass::NotFound,
        Some(libc::ENOTDIR) => ErrorClass::NotADirectory,
        Some(libc::ELOOP) => ErrorClass::SymlinkLoop,
        Some(libc::EMFILE | libc::ENFILE) => ErrorClass::TooManyOpenFiles,
        Some(libc::ENAMETOOLONG) => ErrorClass::NameTooLong,
        Some(libc::EIO) => ErrorClass::InputOutput,
        Some(libc::ETIMEDOUT | libc::EHOSTDOWN | libc::EHOSTUNREACH | libc::ENETDOWN | libc::ESTALE) => {
            ErrorClass::RemoteUnavailable
        }
        _ => ErrorClass::Other,
    }
}

/// Reads one directory.
///
/// `Send + Sync` is on the trait, not on the concrete reader, so an
/// implementation may keep worker-local scratch buffers
/// (docs/08-RUST-PRACTICES.md#concurrency-ownership-not-locks).
///
/// A reader must:
///
/// 1. never follow a symlink — a symlink is a leaf;
/// 2. recheck kind and device before enumerating, so a directory replaced
///    between enumeration and descent is not walked;
/// 3. emit bounded batches, both in entry count and in total name bytes;
/// 4. check `cancel` at least every
///    [`CANCEL_CHECK_ENTRIES`](crate::CANCEL_CHECK_ENTRIES) entries;
/// 5. preserve name bytes exactly.
pub trait DirReader: Send + Sync {
    /// Enumerates `dir`, handing bounded batches to `sink`.
    ///
    /// # Errors
    ///
    /// [`ReadDirError`] for a per-directory failure, cancellation, or a sink
    /// that asked to stop.
    fn read_batches(
        &self,
        dir: &DirHandle,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(RawEntryBatch<'_>) -> ControlFlow<ScanError>,
    ) -> Result<(), ReadDirError>;

    /// A short, stable name for benchmark and differential-test output.
    fn name(&self) -> &'static str;
}

impl fmt::Debug for dyn DirReader + '_ {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirReader").field("name", &self.name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_errors_are_the_only_full_disk_access_class() {
        assert_eq!(
            classify_os_error(&io::Error::from_raw_os_error(libc::EACCES)),
            ErrorClass::PermissionDenied
        );
        assert_eq!(
            classify_os_error(&io::Error::from_raw_os_error(libc::EPERM)),
            ErrorClass::PermissionDenied
        );
        assert_eq!(
            classify_os_error(&io::Error::from_raw_os_error(libc::ENOENT)),
            ErrorClass::NotFound
        );
        assert_eq!(
            classify_os_error(&io::Error::from_raw_os_error(libc::ELOOP)),
            ErrorClass::SymlinkLoop
        );
        assert_eq!(
            classify_os_error(&io::Error::other("no errno")),
            ErrorClass::Other,
            "an error without an errno still gets a stable class"
        );
    }

    #[test]
    fn read_errors_keep_their_raw_code() {
        let error = ReadDirError::from_io(Operation::OpenDir, &io::Error::from_raw_os_error(libc::EACCES));
        assert_eq!(error.class(), Some(ErrorClass::PermissionDenied));
        assert!(matches!(error, ReadDirError::Os { os_code, .. } if os_code == libc::EACCES));
        assert_eq!(ReadDirError::Cancelled.class(), None);
    }
}
