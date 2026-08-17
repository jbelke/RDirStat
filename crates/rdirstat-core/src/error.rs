//! Typed errors.
//!
//! Every enum here is `#[non_exhaustive]` and every one that crosses IPC
//! serializes as an adjacently tagged discriminated union
//! (`{"kind": "...", "detail": {...}}`), so the generated TypeScript can
//! distinguish "permission denied, offer Full Disk Access guidance" from "that
//! path is gone". A command returning `Result<T, String>` throws away exactly
//! the structure the frontend needs.
//!
//! The split that matters: **a partial-failure scan is a success with a
//! payload, not an error.** `EACCES` on one directory is a [`ScanError`] stored
//! in [`CompletedScan::errors`](crate::CompletedScan), never a `Result::Err`
//! out of the scan.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{NodeId, ScanId, TreeGeneration};
use crate::wire::DisplayPath;

/// What the scanner or an action was doing when something failed.
///
/// Recorded per error because macOS privacy guidance depends on it: a failure
/// to `open` a protected directory means something different from a failure to
/// `stat` an entry that vanished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Operation {
    /// Opening a directory handle.
    OpenDir,
    /// Enumerating directory entries.
    ReadDir,
    /// Reading entry metadata.
    Metadata,
    /// Reading a symlink target.
    ReadLink,
    /// Reading volume capacity or identity.
    StatFs,
    /// Reading file contents, for hashing only.
    ReadFile,
    /// Revealing an item in Finder.
    Reveal,
    /// Moving an item to Trash.
    Trash,
    /// Writing or reading a snapshot or catalog artifact.
    Persist,
}

/// A stable, coarse classification of an OS error.
///
/// Stable across macOS releases and across the raw `errno` values, so error
/// counts remain comparable between scans and the UI can key guidance off the
/// class rather than off a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorClass {
    /// `EACCES` or `EPERM`. The only class that may prompt Full Disk Access
    /// guidance.
    PermissionDenied,
    /// `ENOENT`. Usually a race, not a fault.
    NotFound,
    /// `ENOTDIR`, or a kind recheck that disagreed with enumeration.
    NotADirectory,
    /// `ELOOP`, or a symlink cycle the traversal refused to follow.
    SymlinkLoop,
    /// `EMFILE` or `ENFILE`.
    TooManyOpenFiles,
    /// `ENAMETOOLONG`, or a name that exceeded the arena's 16-bit length field.
    NameTooLong,
    /// The name is not valid UTF-8, or contains a byte the display layer must
    /// escape. The bytes are still retained losslessly.
    InvalidName,
    /// `EIO` and other hardware or filesystem failures.
    InputOutput,
    /// `ETIMEDOUT`, `EHOSTDOWN`, and other network-filesystem failures.
    RemoteUnavailable,
    /// Anything else. The raw OS code travels alongside.
    Other,
}

impl ErrorClass {
    /// Every class, in a fixed order.
    ///
    /// A running scan counts failures into an array of atomics — one slot per
    /// class — because that is all the scan path is allowed to write. This
    /// array is that mapping, and [`index`](Self::index) is its inverse, so the
    /// order here is part of nothing persistent: it is re-read through `ALL`
    /// every time and may be reordered freely.
    pub const ALL: [Self; 10] = [
        Self::PermissionDenied,
        Self::NotFound,
        Self::NotADirectory,
        Self::SymlinkLoop,
        Self::TooManyOpenFiles,
        Self::NameTooLong,
        Self::InvalidName,
        Self::InputOutput,
        Self::RemoteUnavailable,
        Self::Other,
    ];

    /// This class's slot in [`ALL`](Self::ALL).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::PermissionDenied => 0,
            Self::NotFound => 1,
            Self::NotADirectory => 2,
            Self::SymlinkLoop => 3,
            Self::TooManyOpenFiles => 4,
            Self::NameTooLong => 5,
            Self::InvalidName => 6,
            Self::InputOutput => 7,
            Self::RemoteUnavailable => 8,
            Self::Other => 9,
        }
    }
}

/// One recorded, non-fatal failure during a scan.
///
/// The first [`MAX_DETAILED_ERRORS`](crate::MAX_DETAILED_ERRORS) of these are
/// stored in full; beyond that only [`ErrorClassCount`] rows grow, so a volume
/// with ten million unreadable paths cannot exhaust memory through its own
/// error log.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScanError {
    /// macOS refused access. The scan continues and marks the node
    /// [`flags::UNREADABLE`](crate::flags::UNREADABLE).
    #[error("permission denied during {operation:?}: {path}")]
    PermissionDenied {
        /// The path that was refused.
        path: DisplayPath,
        /// What was being attempted.
        operation: Operation,
        /// Raw `errno`, for the diagnostic report.
        os_code: i32,
    },

    /// The entry disappeared between enumeration and metadata. Counted as a
    /// mutation; the namespace changed under the scan.
    #[error("vanished during {operation:?}: {path}")]
    Vanished {
        /// The path that went away.
        path: DisplayPath,
        /// What was being attempted.
        operation: Operation,
    },

    /// The entry's name is not valid UTF-8 or contains a path separator. The
    /// bytes are retained; only display and search are affected.
    #[error("undecodable name: {path}")]
    InvalidName {
        /// The escaped rendering of the offending path.
        path: DisplayPath,
    },

    /// Any other OS failure on one path.
    #[error("{class:?} during {operation:?}: {path}")]
    Io {
        /// The path that failed.
        path: DisplayPath,
        /// What was being attempted.
        operation: Operation,
        /// The stable class.
        class: ErrorClass,
        /// Raw `errno`.
        os_code: i32,
    },

    /// The projected peak resident set crossed the configured limit. The scan
    /// ends here rather than at allocation failure, and the UI offers a retry
    /// with aggregation.
    #[error("projected {projected_bytes} B exceeds the {limit_bytes} B limit")]
    MemoryLimit {
        /// Projected peak RSS at the current growth rate.
        projected_bytes: u64,
        /// The configured ceiling.
        limit_bytes: u64,
    },

    /// The arena could not accept another node or name. Fatal for the scan.
    #[error("arena exhausted: {0}")]
    Arena(#[from] ArenaError),

    /// The scan root vanished or changed volume identity. Unlike a descendant
    /// failure, this fails the whole scan: the result would not describe the
    /// thing the user asked about.
    #[error("scan root became unusable: {path} ({reason})")]
    RootUnavailable {
        /// The requested root.
        path: DisplayPath,
        /// Human-readable cause, already classified in `class`.
        reason: String,
        /// The stable class.
        class: ErrorClass,
    },
}

impl ScanError {
    /// The stable class, for the uncapped per-class counters.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::PermissionDenied { .. } => ErrorClass::PermissionDenied,
            Self::Vanished { .. } => ErrorClass::NotFound,
            Self::InvalidName { .. } => ErrorClass::InvalidName,
            Self::Io { class, .. } | Self::RootUnavailable { class, .. } => *class,
            Self::MemoryLimit { .. } | Self::Arena(_) => ErrorClass::Other,
        }
    }

    /// The operation, when one applies.
    #[must_use]
    pub const fn operation(&self) -> Option<Operation> {
        match self {
            Self::PermissionDenied { operation, .. }
            | Self::Vanished { operation, .. }
            | Self::Io { operation, .. } => Some(*operation),
            _ => None,
        }
    }

    /// Whether this error ends the scan rather than being recorded and skipped.
    ///
    /// Only three things are fatal: a root that stopped being the root, the
    /// memory ceiling, and an exhausted arena. Everything else is a row in the
    /// completion alert.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::RootUnavailable { .. } | Self::MemoryLimit { .. } | Self::Arena(_)
        )
    }
}

/// An uncapped count of errors sharing a class and operation.
///
/// Kept alongside the capped detail list so a summary stays exact even when
/// the detail list is truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ErrorClassCount {
    /// The stable class.
    pub class: ErrorClass,
    /// The operation, or `None` for classes that have none.
    pub operation: Option<Operation>,
    /// How many errors of this class and operation occurred, uncapped.
    pub count: u64,
}

/// A structural failure while building or loading an arena.
///
/// These are bugs or corrupt inputs, not filesystem facts, which is why they
/// are separate from [`ScanError`]. Loading a `*.rdstat` file turns every one
/// of them into a closed failure before a single node reaches a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArenaError {
    /// The arena is full. The ceiling is
    /// [`MAX_NODE_INDEX`](crate::MAX_NODE_INDEX) because bit 31 is reserved for
    /// virtual groups.
    #[error("arena is full at {ceiling} nodes")]
    NodeCeiling {
        /// The ceiling that was hit.
        ceiling: u32,
    },

    /// A single name exceeded the 16-bit length field.
    #[error("name of {len} bytes exceeds the 16-bit length field")]
    NameTooLong {
        /// The offending length.
        len: usize,
    },

    /// The name blob grew past the 48-bit offset field.
    #[error("name blob offset {offset} exceeds the 48-bit field")]
    NameOffsetOverflow {
        /// The offending offset.
        offset: u64,
    },

    /// A directory was pushed out of order, or is not a real arena slot. The
    /// sorted index would stop being searchable.
    #[error("directory index entry {node} is out of order")]
    DirIndexOutOfOrder {
        /// The offending id.
        node: NodeId,
    },

    /// A link named a node that does not exist.
    #[error("node {node} does not exist")]
    UnknownNode {
        /// The dangling id.
        node: NodeId,
    },

    /// A parent link chain did not terminate at the root within
    /// [`MAX_TREE_DEPTH`](crate::MAX_TREE_DEPTH) steps, i.e. the arena contains
    /// a cycle.
    #[error("parent chain from {node} exceeded {limit} levels")]
    CycleOrDepth {
        /// Where the walk started.
        node: NodeId,
        /// The depth ceiling.
        limit: u32,
    },

    /// A node was linked under a parent that is not a directory.
    #[error("{parent} is not a directory")]
    NotADirectory {
        /// The offending parent.
        parent: NodeId,
    },
}

/// A read-only query against a published tree failed.
///
/// Every variant is the frontend's cue to do something specific: refetch under
/// the current generation, clear a selection, or surface a corruption alert.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueryError {
    /// No completed scan is loaded.
    #[error("no scan is loaded")]
    NoScan,

    /// The request named a generation that is no longer current. The frontend
    /// refetches with `current` rather than mixing two trees.
    #[error("generation {requested} is stale; current is {current}")]
    StaleGeneration {
        /// What the caller asked for.
        requested: TreeGeneration,
        /// What is actually published.
        current: TreeGeneration,
    },

    /// The id does not name a live node in this tree.
    #[error("unknown node {node}")]
    UnknownNode {
        /// The offending id.
        node: NodeId,
    },

    /// The operation is not defined on a virtual `<Files>` group. A group has
    /// no path, no children, and no identity to revalidate.
    #[error("{node} is a virtual group")]
    VirtualGroup {
        /// The offending id.
        node: NodeId,
    },

    /// The cursor is malformed, or was minted for a different generation,
    /// parent, sort key, or direction.
    #[error("cursor is not valid for this query")]
    InvalidCursor,

    /// The caller asked for more rows than the hard ceiling
    /// ([`MAX_CHILD_PAGE`](crate::MAX_CHILD_PAGE)).
    #[error("requested {requested} rows; the limit is {max}")]
    LimitExceeded {
        /// What the caller asked for.
        requested: u32,
        /// The ceiling.
        max: u32,
    },

    /// A parent chain exceeded [`MAX_TREE_DEPTH`](crate::MAX_TREE_DEPTH) while
    /// reconstructing a path. Fails closed rather than looping.
    #[error("path from {node} exceeded {limit} levels")]
    PathTooDeep {
        /// Where the walk started.
        node: NodeId,
        /// The depth ceiling.
        limit: u32,
    },

    /// The tree failed a structural invariant — a name range outside the blob,
    /// a dangling link. Only reachable from a corrupt snapshot.
    #[error("tree is inconsistent: {0}")]
    Corrupt(#[from] ArenaError),

    /// The requested report needs a completed catalog scan and there is none.
    #[error("report requires a saved catalog scan")]
    NoCatalogScan,

    /// Anything unexpected, already logged with its full source chain.
    #[error("{0}")]
    Internal(String),
}

/// Starting a scan failed before any traversal happened.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StartError {
    /// There is already an active scan. v1 supports exactly one.
    #[error("{scan_id} is already running")]
    AlreadyScanning {
        /// The scan that holds the slot.
        scan_id: ScanId,
    },

    /// The requested root does not exist.
    #[error("root not found: {path}")]
    RootNotFound {
        /// The requested root.
        path: DisplayPath,
    },

    /// The requested root is not a directory.
    #[error("root is not a directory: {path}")]
    RootNotADirectory {
        /// The requested root.
        path: DisplayPath,
    },

    /// macOS refused to open the root. This is the case that may justify Full
    /// Disk Access guidance.
    #[error("permission denied for root: {path}")]
    PermissionDenied {
        /// The requested root.
        path: DisplayPath,
        /// Raw `errno`.
        os_code: i32,
    },

    /// An exclusion rule failed to compile, a worker count was zero, or another
    /// option was rejected at load. No regex compiles inside the scan loop, so
    /// this is the only place a bad pattern can surface.
    #[error("invalid scan options: {detail}")]
    InvalidOptions {
        /// What was wrong.
        detail: String,
    },

    /// Anything unexpected, already logged with its full source chain.
    #[error("{0}")]
    Internal(String),
}

/// A Reveal or Trash action failed.
///
/// Every variant that mentions identity exists because a path recorded at scan
/// time is not authority at action time: the tree is a past observation and the
/// filesystem has moved on.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionError {
    /// No completed scan is loaded.
    #[error("no scan is loaded")]
    NoScan,

    /// The request named a generation that is no longer current.
    #[error("generation {requested} is stale; current is {current}")]
    StaleGeneration {
        /// What the caller asked for.
        requested: TreeGeneration,
        /// What is actually published.
        current: TreeGeneration,
    },

    /// The id does not name a live node in this tree.
    #[error("unknown node {node}")]
    UnknownNode {
        /// The offending id.
        node: NodeId,
    },

    /// A virtual `<Files>` group, or the scan root, was selected. Neither can
    /// be revealed or trashed as a path.
    #[error("{node} cannot be acted on")]
    NotActionable {
        /// The offending id.
        node: NodeId,
    },

    /// The re-`stat` disagreed with the scan observation on device, inode, or
    /// kind. The item at that path is not the item the user selected.
    #[error("changed since the scan: {path}")]
    ChangedSinceScan {
        /// The reconstructed path.
        path: DisplayPath,
    },

    /// The reconstructed path resolved outside the scan root, e.g. through a
    /// component that became a symlink after the scan.
    #[error("outside the scan root: {path}")]
    OutsideScanRoot {
        /// The reconstructed path.
        path: DisplayPath,
    },

    /// The confirmation token is missing, expired, or bound to a different
    /// generation or selection. Trash never proceeds without a fresh one.
    #[error("confirmation token is not valid for this selection")]
    InvalidConfirmation,

    /// macOS refused the operation.
    #[error("permission denied during {operation:?}: {path}")]
    PermissionDenied {
        /// The path that was refused.
        path: DisplayPath,
        /// What was being attempted.
        operation: Operation,
        /// Raw `errno`.
        os_code: i32,
    },

    /// `NSFileManager.trashItem` failed for a reason of its own.
    #[error("could not move to Trash: {path} ({reason})")]
    TrashFailed {
        /// The item that stayed put.
        path: DisplayPath,
        /// What Foundation reported.
        reason: String,
    },

    /// Anything unexpected, already logged with its full source chain.
    #[error("{0}")]
    Internal(String),
}

/// The umbrella error for a Tauri command that has no narrower type of its own.
///
/// `src-tauri` converts `anyhow::Error` into this at the command boundary; the
/// library crates never construct it. Every variant carries the protocol
/// version so a frontend built against an older contract can say so instead of
/// rendering a mystery.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandError {
    /// A scan could not be started.
    #[error(transparent)]
    Start(StartError),
    /// A query against a published tree failed.
    #[error(transparent)]
    Query(QueryError),
    /// A Reveal or Trash action failed.
    #[error(transparent)]
    Action(ActionError),
    /// A scan-level failure surfaced through a command.
    #[error(transparent)]
    Scan(ScanError),
    /// No completed scan is loaded.
    #[error("no scan is loaded")]
    NoScan,
    /// The frontend and backend disagree about the IPC contract version.
    #[error("protocol version {requested} is not supported; this build speaks {supported}")]
    ProtocolMismatch {
        /// What the caller claimed.
        requested: u16,
        /// What this build implements.
        supported: u16,
    },
    /// Anything unexpected, already logged with its full source chain.
    #[error("{0}")]
    Internal(String),
}

impl From<StartError> for CommandError {
    fn from(value: StartError) -> Self {
        Self::Start(value)
    }
}

impl From<QueryError> for CommandError {
    fn from(value: QueryError) -> Self {
        Self::Query(value)
    }
}

impl From<ActionError> for CommandError {
    fn from(value: ActionError) -> Self {
        Self::Action(value)
    }
}

impl From<ScanError> for CommandError {
    fn from(value: ScanError) -> Self {
        Self::Scan(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_scan_failures_end_a_scan() {
        let denied = ScanError::PermissionDenied {
            path: DisplayPath::from_bytes(b"/System/Library"),
            operation: Operation::OpenDir,
            os_code: 13,
        };
        assert!(!denied.is_fatal());
        assert_eq!(denied.class(), ErrorClass::PermissionDenied);
        assert_eq!(denied.operation(), Some(Operation::OpenDir));

        assert!(
            ScanError::MemoryLimit {
                projected_bytes: 6 << 30,
                limit_bytes: 5 << 30
            }
            .is_fatal()
        );
        assert!(ScanError::Arena(ArenaError::NodeCeiling { ceiling: 1 }).is_fatal());
        assert!(
            ScanError::RootUnavailable {
                path: DisplayPath::from_bytes(b"/Volumes/gone"),
                reason: "unmounted".to_owned(),
                class: ErrorClass::NotFound,
            }
            .is_fatal()
        );
    }

    #[test]
    fn errors_serialize_as_a_discriminated_union() {
        let error = QueryError::StaleGeneration {
            requested: TreeGeneration::from_raw(3),
            current: TreeGeneration::from_raw(4),
        };
        let json = serde_json::to_string(&error).expect("serializes");
        assert!(json.contains(r#""kind":"stale_generation""#), "{json}");
        assert!(json.contains(r#""requested":3"#), "{json}");
        let round_tripped: QueryError = serde_json::from_str(&json).expect("round trips");
        assert_eq!(round_tripped, error);
    }

    #[test]
    fn unit_variants_still_carry_a_kind_tag() {
        let json = serde_json::to_string(&QueryError::NoScan).expect("serializes");
        assert_eq!(json, r#"{"kind":"no_scan"}"#);
    }

    #[test]
    fn narrower_errors_widen_into_the_umbrella() {
        let wide: CommandError = QueryError::NoScan.into();
        assert!(matches!(wide, CommandError::Query(QueryError::NoScan)));
    }
}
