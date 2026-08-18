//! # `rdirstat-core` — the shared contract
//!
//! The immutable arena tree, its packed identifiers, directory totals, byte
//! formatting, and every type that crosses the IPC boundary. This crate knows
//! nothing about I/O, Tauri, or the filesystem: `scan`, `classify`, and
//! `treemap` depend on it, the CLI and the desktop shell compose them, and
//! **nothing under `crates/*` depends on `tauri`**. That is what keeps the
//! benchmarks honest and the scanner testable without a webview.
//!
//! Everything here is sized against one number: **69 million entries on one
//! volume**. A [`Node`] is 48 bytes because 69M × 48 is 3.08 GiB and the peak
//! RSS gate is 5.0 GiB. A design that is comfortable at 2M entries and
//! unbounded at 69M has not cleared the bar.
//!
//! ## The IPC contract
//!
//! This is the binding list from `docs/01-ARCHITECTURE.md`, reproduced here so
//! every crate reads one copy of it. `AppState` is owned by `src-tauri`; every
//! other type in these signatures is defined in this crate and must not be
//! redeclared anywhere else.
//!
//! **Node count never appears in a payload.** Every command is `O(1)`,
//! `O(page)`, or `O(drawn tiles)`.
//!
//! ```rust,ignore
//! // ---- the nine binding commands (docs/01-ARCHITECTURE.md#ipc-contract) ----
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn scan_start(
//!     state: tauri::State<'_, AppState>,
//!     root: String,
//!     options: ScanOptions,
//! ) -> Result<ScanId, StartError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn scan_cancel(
//!     state: tauri::State<'_, AppState>,
//!     scan_id: ScanId,
//! ) -> Result<CancelState, ScanError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn children(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     item: NodeId,
//!     sort: Sort,
//!     cursor: Option<Cursor>,
//!     limit: u32,
//! ) -> Result<ChildPage, QueryError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn layout(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     root: NodeId,
//!     kind: LayoutKind,
//!     viewport: Viewport,
//!     min_px: f32,
//! ) -> Result<BinaryResponse, QueryError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn node_details(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     node: NodeId,
//! ) -> Result<Details, QueryError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn path_of(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     item: NodeId,
//! ) -> Result<DisplayPath, QueryError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn report(
//!     state: tauri::State<'_, AppState>,
//!     catalog_scan: CatalogScanId,
//!     name: ReportName,
//!     params: ReportParams,
//! ) -> Result<BinaryResponse, QueryError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn reveal_in_finder(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     node: NodeId,
//! ) -> Result<(), ActionError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn move_to_trash(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     nodes: Vec<NodeId>,
//!     confirmation: ConfirmationToken,
//! ) -> Result<TrashReport, ActionError>;
//!
//! // ---- additive commands the UI needs; docs/01 does not enumerate them, ----
//! // ---- but the launch screen, the state machine, and the confirmation  ----
//! // ---- sheet in docs/03 and docs/05 all require one.                   ----
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn scan_status(
//!     state: tauri::State<'_, AppState>,
//! ) -> Result<ScanStatus, CommandError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn volumes(
//!     state: tauri::State<'_, AppState>,
//! ) -> Result<Vec<VolumeInfo>, CommandError>;
//!
//! #[tauri::command]
//! #[specta::specta]
//! async fn trash_preview(
//!     state: tauri::State<'_, AppState>,
//!     generation: TreeGeneration,
//!     nodes: Vec<NodeId>,
//! ) -> Result<TrashPreview, ActionError>;
//! ```
//!
//! ### Events
//!
//! One event, emitted at most [`PROGRESS_MAX_HZ`] times a second under the name
//! [`SCAN_PROGRESS_EVENT`], carrying [`ScanProgress`] — absolute counters plus a
//! sequence number, so a dropped event costs nothing. The frontend ignores
//! events from an older [`ScanId`]. **Completion is a state transition returned
//! by the supervisor, not inferred from a missing progress event.**
//!
//! `tauri_specta::Event` cannot be derived on [`ScanProgress`] here, because
//! that would make this crate depend on `tauri`. `src-tauri` therefore wraps it:
//!
//! ```rust,ignore
//! #[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, tauri_specta::Event)]
//! pub struct ScanProgressEvent(pub rdirstat_core::ScanProgress);
//! ```
//!
//! ### Errors
//!
//! Errors cross IPC as discriminated variants, never as free-form strings. Every
//! error enum in this crate serializes adjacently tagged —
//! `{"kind": "...", "detail": {...}}` — so the generated TypeScript is a
//! discriminated union and the frontend can tell "permission denied, offer Full
//! Disk Access guidance" from "that path is gone".
//!
//! The five error types, by boundary: [`StartError`] (scan could not start),
//! [`ScanError`] (one recorded, usually non-fatal, scan failure), [`QueryError`]
//! (a read against a published tree), [`ActionError`] (Reveal or Trash), and
//! [`CommandError`] (the umbrella for commands with no narrower type; carries a
//! [`CommandError::ProtocolMismatch`] variant quoting [`PROTOCOL_VERSION`]).
//! [`ArenaError`] is structural and never leaves the backend on its own.
//!
//! ### Binary responses
//!
//! `layout` and `report` return [`BinaryResponse`], which `src-tauri` hands to
//! `tauri::ipc::Response::new(bytes)`. The payload is **Arrow IPC**; the Arrow
//! schema is the contract. Generation and protocol version travel as Arrow
//! schema metadata under [`ARROW_META_GENERATION`] and
//! [`ARROW_META_PROTOCOL_VERSION`], and the frontend rejects a batch whose
//! generation is not the one it asked for. `layout` emits exactly the columns in
//! [`LAYOUT_COLUMNS`]. Adding an SQL column does not by itself update the UI.
//!
//! ## Working with this crate
//!
//! - Build with [`TreeBuilder`], freeze with [`TreeBuilder::finish`], publish as
//!   `Arc<CompletedScan>`. There is no `Arc<Mutex<Tree>>` and there will not be
//!   one.
//! - Read names through [`NameBlob::bytes`] and nodes through [`Tree::node`].
//!   Both return `Option`. Nothing in this crate slices a blob unchecked, and a
//!   `u32` arriving from IPC is never trusted as an index.
//! - Format sizes with [`format_si`]. Decimal SI is the default because Finder
//!   is decimal SI. Logical and allocated are shown side by side and never
//!   summed.

#![forbid(unsafe_code)]

pub mod bands;
pub mod by_age;
pub mod by_category;
pub mod diff;
mod dirs;
pub mod dupes;
mod error;
mod fmt_bytes;
mod id;
mod name;
mod node;
mod scan;
pub mod snapshot;
mod tree;
mod wire;

pub use crate::bands::{SIZE_BAND_COUNT, SIZE_BAND_EDGES, SizeBandEntry, SizeBandRow};
pub use crate::by_age::{AGE_BUCKET_COUNT, AGE_BUCKET_EDGES, AgeBucketEntry, AgeBucketRow};
pub use crate::by_category::{CategoryEntry, CategoryRow};
pub use crate::diff::{DiffMetric, DiffReport, MAX_DIFF_ENTRIES};
pub use crate::dirs::{DirIndex, DirTotals};
pub use crate::dupes::{
    DuplicateCandidateCluster, DuplicateCandidateMember, DuplicateCandidateReport, MAX_CLUSTER_MEMBERS,
    MAX_DUPLICATE_CLUSTERS,
};
pub use crate::error::{
    ActionError, ArenaError, CommandError, ErrorClass, ErrorClassCount, Operation, QueryError, ScanError, StartError,
};
pub use crate::fmt_bytes::{format_iec, format_percent, format_si};
pub use crate::id::{
    Bytes, CatalogScanId, CategoryId, DirId, MAX_NODE_INDEX, NodeId, ScanId, TreeGeneration, VIRTUAL_GROUP_BIT,
};
pub use crate::name::{MAX_NAME_LEN, MAX_NAME_OFFSET, NameBlob, NameRef};
pub use crate::node::{Kind, Node, flags};
pub use crate::scan::{
    CancelState, CompletedScan, ConfigHash, ExclusionRule, ReadyScanRow, RuleAction, RuleScope, RuleSyntax,
    RunningScanRow, ScanCounts, ScanOptions, ScanProgress, ScanState, ScanStatus, ScanSummary, ScanTotals, VolumeId,
    WaitReason,
};
pub use crate::tree::{Children, MAX_TREE_DEPTH, Tree, TreeBuilder};
pub use crate::wire::{
    BinaryResponse, ChildPage, ChildRow, ConfirmationToken, Cursor, CursorPayload, Details, DisplayPath, LayoutKind,
    ReportName, ReportParams, ScanErrorReport, SnapshotOffer, Sort, SortDirection, SortKey, TrashItemResult,
    TrashPreview, TrashPreviewItem, TrashReport, Viewport, VolumeInfo,
};

/// Version of the IPC contract.
///
/// Bumped when a command signature, an error shape, or an Arrow schema changes
/// incompatibly. Travels in [`CommandError::ProtocolMismatch`] and in Arrow
/// schema metadata, so a stale frontend says so instead of rendering a mystery.
pub const PROTOCOL_VERSION: u16 = 1;

/// Hard ceiling on rows in one [`ChildPage`].
///
/// The backend clamps rather than erroring on a larger request, and **no paged
/// command or named report may ever return more rows than its declared limit.**
/// That is the invariant the whole virtualized UI rests on.
pub const MAX_CHILD_PAGE: u32 = 500;

/// How many [`ScanError`] values are stored in full.
///
/// Beyond this only [`ErrorClassCount`] rows grow, so a volume with ten million
/// unreadable paths cannot exhaust memory through its own error log.
pub const MAX_DETAILED_ERRORS: usize = 10_000;

/// Ceiling on the rows any one report's breakdown returns.
///
/// Every report here — size bands, categories, ages — answers a question about
/// a whole subtree and then offers to show *which files*. A band on a boot
/// volume holds ten million of them, so that list is a leaderboard, never an
/// enumeration.
///
/// Defined once, here, because it had begun to exist independently in the
/// command layer, in the query layer, and in each report: four copies of one
/// number, each free to drift, none of them wrong until one of them was.
pub const MAX_REPORT_ENTRIES: usize = 250;

/// Ceiling on progress event frequency, in hertz.
pub const PROGRESS_MAX_HZ: u32 = 10;

/// Smallest drawable tile edge, in device pixels.
///
/// The sub-pixel cutoff is what bounds the drawn tile count; a candidate node
/// count is not evidence that the rendered set stayed bounded.
///
/// **Eight, not three.** The cutoff is divided by the device pixel ratio to
/// reach CSS pixels, so on a 2x display a value of 3 admitted a tile 1.5 CSS
/// pixels on a side — smaller than the 1px border drawn around it, so most of
/// such a tile *is* border. Measured on a real 138 GB scan in an 832x287
/// canvas, that produced 26,472 tiles averaging 3x3 CSS pixels: a uniform
/// scatter in which the large blocks a treemap exists to show were not
/// visible at all. Eight device pixels is 4 CSS pixels at 2x, which is the
/// smallest tile that still reads as a rectangle rather than as a speck.
pub const MIN_TILE_PX: f32 = 8.0;

/// Event name for [`ScanProgress`].
pub const SCAN_PROGRESS_EVENT: &str = "scan:progress";

/// Arrow schema-metadata key carrying [`PROTOCOL_VERSION`].
pub const ARROW_META_PROTOCOL_VERSION: &str = "rdirstat.protocol_version";

/// Arrow schema-metadata key carrying the [`TreeGeneration`] as a decimal
/// string. The frontend rejects a batch whose value is not the generation it
/// asked for.
pub const ARROW_META_GENERATION: &str = "rdirstat.generation";

/// Arrow schema-metadata key naming the schema, e.g. [`LAYOUT_SCHEMA_NAME`].
pub const ARROW_META_SCHEMA_NAME: &str = "rdirstat.schema";

/// Arrow schema-metadata key carrying that schema's version.
pub const ARROW_META_SCHEMA_VERSION: &str = "rdirstat.schema_version";

/// Schema name for the hierarchy tile batch produced by `layout`.
pub const LAYOUT_SCHEMA_NAME: &str = "layout";

/// Version of the `layout` Arrow schema.
pub const LAYOUT_SCHEMA_VERSION: u16 = 1;

/// The exact columns of the `layout` Arrow batch, in order.
///
/// Types are `node: UInt32`, `depth: UInt32`, `x/y/w/h: Float32`,
/// `category: UInt8`; none is nullable. All three layouts in [`LayoutKind`]
/// share this schema — `x/y/w/h` are rectangle coordinates for the treemap and
/// icicle, and `(start_angle, inner_radius, sweep, thickness)` for the sunburst.
pub const LAYOUT_COLUMNS: [&str; 7] = ["node", "depth", "x", "y", "w", "h", "category"];

/// Clamps a requested page size to [`MAX_CHILD_PAGE`].
///
/// Clamping, not erroring: a UI that asks for one row too many should get a
/// page, not a failure. [`QueryError::LimitExceeded`] exists for callers that
/// want to be told instead.
#[must_use]
pub const fn clamp_page_limit(requested: u32) -> u32 {
    if requested == 0 || requested > MAX_CHILD_PAGE {
        MAX_CHILD_PAGE
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_limits_clamp_rather_than_fail() {
        assert_eq!(clamp_page_limit(0), MAX_CHILD_PAGE);
        assert_eq!(clamp_page_limit(1), 1);
        assert_eq!(clamp_page_limit(MAX_CHILD_PAGE), MAX_CHILD_PAGE);
        assert_eq!(clamp_page_limit(MAX_CHILD_PAGE + 1), MAX_CHILD_PAGE);
        assert_eq!(clamp_page_limit(u32::MAX), MAX_CHILD_PAGE);
    }

    #[test]
    fn the_layout_schema_is_pinned() {
        assert_eq!(LAYOUT_COLUMNS.len(), 7);
        assert_eq!(LAYOUT_COLUMNS[0], "node");
        assert_eq!(LAYOUT_COLUMNS[6], "category");
    }
}
