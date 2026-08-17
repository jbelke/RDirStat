//! The IPC data-transfer types.
//!
//! Everything here is small, `Serialize + Deserialize + specta::Type`, and
//! bounded by construction. **Node count never appears in an IPC payload**:
//! every command is `O(1)`, `O(page)`, or `O(drawn tiles)`.
//!
//! These types live in `rdirstat-core` rather than in `src-tauri` so that all
//! six crates, the CLI, and the generated TypeScript agree on one definition.
//! `src-tauri` writes command signatures against them and never declares a
//! parallel DTO.

use serde::{Deserialize, Serialize};

use crate::dirs::DirTotals;
use crate::error::ActionError;
use crate::id::{CatalogScanId, CategoryId, NodeId, TreeGeneration};
use crate::node::Kind;

/// A path rendered for display, never for a filesystem operation.
///
/// Filesystem names are bytes. This is the escaped, human-readable projection
/// of those bytes and it is **not action authority**: Reveal and Trash
/// reconstruct the path from the recorded scan root plus stored components and
/// then revalidate identity. A path that arrived from JavaScript is never used
/// to touch the disk (docs/03-MACOS.md#names-case-and-paths).
///
/// # Encoding
///
/// Total and reversible:
///
/// - a literal `%` becomes `%25`;
/// - any byte that is not part of a valid UTF-8 sequence becomes `%XX`, upper
///   hex;
/// - everything else is copied verbatim, in the filesystem's own byte order and
///   its own Unicode normalization. NFC normalization happens later, for
///   matching and rendering only.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct DisplayPath(String);

impl DisplayPath {
    /// Escapes filesystem bytes for display.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut out = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match core::str::from_utf8(rest) {
                Ok(text) => {
                    push_escaped(&mut out, text);
                    return Self(out);
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if let Some(text) = rest.get(..valid).and_then(|slice| core::str::from_utf8(slice).ok()) {
                        push_escaped(&mut out, text);
                    }
                    let skip = error.error_len().unwrap_or(rest.len() - valid).max(1);
                    for byte in rest.get(valid..valid + skip).unwrap_or_default() {
                        push_hex_escape(&mut out, *byte);
                    }
                    rest = rest.get(valid + skip..).unwrap_or_default();
                    if rest.is_empty() {
                        return Self(out);
                    }
                }
            }
        }
    }

    /// Wraps text that is already escaped.
    #[must_use]
    pub const fn from_escaped(text: String) -> Self {
        Self(text)
    }

    /// The escaped text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the encoding had to escape anything, i.e. the original bytes are
    /// not reproducible by a naive UTF-8 encode. The UI marks these.
    #[must_use]
    pub fn is_escaped(&self) -> bool {
        self.0.contains('%')
    }
}

fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        if ch == '%' {
            out.push_str("%25");
        } else {
            out.push(ch);
        }
    }
}

fn push_hex_escape(out: &mut String, byte: u8) {
    const HEX: [u8; 16] = *b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

impl core::fmt::Display for DisplayPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which column the backend sorts a child page by.
///
/// Rust owns ordering, because a client-side sort would need the whole tree in
/// JavaScript and the IPC contract forbids that. `TanStack Table` therefore runs
/// in `manualSorting` mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SortKey {
    /// Byte-wise name order. Not locale collation, which is not stable across
    /// OS releases.
    Name,
    /// Logical bytes.
    #[default]
    Logical,
    /// Allocated bytes.
    Allocated,
    /// Modification time.
    Mtime,
    /// Content category, then name.
    Category,
    /// Directories before files, then name.
    Kind,
}

/// Ascending or descending.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SortDirection {
    /// Smallest or earliest first.
    Ascending,
    /// Largest or latest first. The default, because the product question is
    /// "what is biggest".
    #[default]
    Descending,
}

/// Sort order for a child page. Ties always break on [`NodeId`], so the order
/// is total and paging cannot repeat or skip a row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
pub struct Sort {
    /// The column.
    pub key: SortKey,
    /// The direction.
    pub direction: SortDirection,
}

/// An opaque paging cursor.
///
/// Offset pagination is not used: at 500 rows a page and millions of children,
/// an offset scan is quadratic and a concurrent generation swap makes it wrong.
/// The encoded [`CursorPayload`] binds every parameter that could change the
/// meaning of "the next page".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    /// Wraps an encoded cursor.
    #[must_use]
    pub const fn from_encoded(text: String) -> Self {
        Self(text)
    }

    /// The encoded text. Opaque to the frontend, which only ever echoes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What an opaque [`Cursor`] decodes to.
///
/// `src-tauri` owns the encoding; this type fixes the contents, so a cursor
/// minted under one generation, parent, or sort can be rejected with
/// [`QueryError::InvalidCursor`](crate::QueryError::InvalidCursor) rather than
/// silently paging through the wrong tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
pub struct CursorPayload {
    /// The tree this cursor was minted against.
    pub generation: TreeGeneration,
    /// The parent whose children are being paged.
    pub parent: NodeId,
    /// The sort this cursor continues.
    pub sort: Sort,
    /// The sort key value of the last row returned.
    pub last_key: i64,
    /// The [`NodeId`] of the last row returned — the total tie-breaker.
    pub last_node: NodeId,
}

/// One row in a child page.
///
/// Carries the rendered name rather than a [`NameRef`](crate::NameRef): a
/// frontend has no blob to resolve against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ChildRow {
    /// The node, or a tagged virtual `<Files>` group.
    pub node: NodeId,
    /// The escaped display name — this component only, not a path.
    pub name: String,
    /// Filesystem object kind.
    pub kind: Kind,
    /// Primary content category. The frontend resolves the colour from a CSS
    /// variable; Rust never sends RGB.
    pub category: CategoryId,
    /// Logical bytes: for a file its own size, for a directory its subtree
    /// total.
    pub logical: u64,
    /// Allocated bytes, on the same basis.
    pub allocated: u64,
    /// Modification time in Unix seconds.
    pub mtime: i64,
    /// Bits from [`flags`](crate::flags). Drives the row annotations for
    /// unreadable, excluded, aggregated, hard-linked, and sparse entries.
    pub flags: u16,
    /// Direct children, so the tree can render a disclosure triangle without a
    /// second round trip. Zero for leaves and for virtual groups.
    pub children: u32,
    /// Whether this row is the synthesized `<Files>` group. Such a row cannot
    /// be revealed, trashed, or expanded.
    pub is_virtual_group: bool,
}

/// One bounded page of children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ChildPage {
    /// The tree these rows came from. The frontend rejects a page whose
    /// generation is not the one it asked for.
    pub generation: TreeGeneration,
    /// The parent that was paged.
    pub parent: NodeId,
    /// At most [`MAX_CHILD_PAGE`](crate::MAX_CHILD_PAGE) rows.
    pub rows: Vec<ChildRow>,
    /// Cursor for the next page, or `None` at the end.
    pub next: Option<Cursor>,
    /// Total children under `parent`, for the virtualizer's scrollbar. This is
    /// one directory's fan-out, not a node count.
    pub total_children: u32,
    /// The effective limit after clamping.
    pub limit: u32,
}

/// Everything the details panel shows for one selected item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct Details {
    /// The tree this came from.
    pub generation: TreeGeneration,
    /// The node.
    pub node: NodeId,
    /// Full path, escaped, for display and Copy Path only.
    pub path: DisplayPath,
    /// The escaped display name.
    pub name: String,
    /// Filesystem object kind.
    pub kind: Kind,
    /// Primary content category.
    pub category: CategoryId,
    /// Logical bytes. Shown beside `allocated`, never merged with it.
    pub logical: u64,
    /// Allocated bytes. On APFS this is not a reclaimable-space promise.
    pub allocated: u64,
    /// Modification time in Unix seconds.
    pub mtime: i64,
    /// Bits from [`flags`](crate::flags).
    pub flags: u16,
    /// Subtree aggregates, present only for a directory or a virtual group.
    pub subtree: Option<DirTotals>,
    /// For a repeated hard link, the path whose traversal counted the bytes.
    /// The UI says "counted at <path>" rather than implying either path owns
    /// them physically.
    pub counted_at: Option<DisplayPath>,
    /// Whether macOS presents this directory as a package.
    pub is_package: bool,
}

/// Which hierarchy layout `layout` should compute.
///
/// All three come from one traversal in `rdirstat-treemap` and share one Arrow
/// response, but each still owns its geometry, hit testing, labels, and
/// acceptance gate. A shared buffer does not make the sunburst free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LayoutKind {
    /// Squarified rectangles. Best for "what is the single biggest thing".
    #[default]
    Treemap,
    /// Stacked horizontal bars with depth on the y-axis. Best for "which deep
    /// path is heavy".
    Icicle,
    /// The icicle in polar coordinates. The best overview and the worst
    /// drill-down: outer rings lose angular resolution.
    Sunburst,
}

/// The canvas region a layout is computed for.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct Viewport {
    /// CSS pixel width.
    pub width: f32,
    /// CSS pixel height.
    pub height: f32,
    /// Device pixel ratio, so the sub-pixel cutoff is applied in real pixels.
    pub device_pixel_ratio: f32,
}

/// An Arrow IPC payload.
///
/// `src-tauri` returns this as `tauri::ipc::Response::new(bytes)`; the frontend
/// reads it with `tableFromIPC`. The Arrow schema is the contract, and
/// generation plus protocol version travel as Arrow **schema metadata** under
/// the `ARROW_META_*` keys, so a batch from the wrong generation is rejected by
/// the reader rather than drawn.
///
/// Offsets, lengths, and buffer alignment are validated by the Arrow reader
/// before any typed array is constructed, which is what replaced the earlier
/// hand-rolled little-endian header.
#[derive(Clone, PartialEq, Eq)]
pub struct BinaryResponse {
    /// IPC contract version, mirrored into the schema metadata.
    pub protocol_version: u16,
    /// The tree or catalog scan the payload describes.
    pub generation: TreeGeneration,
    /// Version of this particular named schema.
    pub schema_version: u16,
    /// Serialized Arrow IPC stream.
    pub bytes: Vec<u8>,
}

impl BinaryResponse {
    /// Wraps Arrow IPC bytes with the current protocol version.
    #[must_use]
    pub fn new(generation: TreeGeneration, schema_version: u16, bytes: Vec<u8>) -> Self {
        Self {
            protocol_version: crate::PROTOCOL_VERSION,
            generation,
            schema_version,
            bytes,
        }
    }

    /// Consumes the wrapper, yielding the bytes for `tauri::ipc::Response`.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl core::fmt::Debug for BinaryResponse {
    /// Summary only: a tile batch is megabytes of Arrow buffers.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinaryResponse")
            .field("protocol_version", &self.protocol_version)
            .field("generation", &self.generation)
            .field("schema_version", &self.schema_version)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// The closed set of named reports.
///
/// The frontend cannot submit SQL, paths, globs, identifiers, or pragmas. Each
/// name maps to one reviewed parameterized query with a pinned Arrow schema
/// version and a row ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReportName {
    /// Bytes and counts per content category.
    CategoryTotals,
    /// Bytes and counts per context tag.
    ContextTagTotals,
    /// Log-scale size buckets with p50/p90/p99/p99.9.
    SizeBuckets,
    /// Year and month totals for the age heatmap.
    AgeMonths,
    /// The largest entries.
    LargestEntries,
    /// Old, large, compressible entries.
    ArchiveCandidates,
    /// Exact added/removed/grown/shrunk between two catalog scans.
    ScanDiff,
    /// The largest logical and allocated deltas in a diff.
    BiggestMovers,
    /// Duplicate candidate clusters ranked by potential recovery.
    DuplicateClusters,
}

/// Bound, typed parameters for a named report.
///
/// `#[non_exhaustive]` because phase 6 will add parameters; a sibling
/// constructs it with `..ReportParams::default()`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[non_exhaustive]
pub struct ReportParams {
    /// Row ceiling for this call. Clamped to the report's own maximum.
    pub limit: u32,
    /// Continuation for a cursor-paged report.
    pub cursor: Option<Cursor>,
    /// Restrict to one content category.
    pub category: Option<CategoryId>,
    /// Inclusive lower bound on modification time, Unix seconds.
    pub since_unix: Option<i64>,
    /// Exclusive upper bound on modification time, Unix seconds.
    pub until_unix: Option<i64>,
    /// The second scan, for a diff report.
    pub compare_to: Option<CatalogScanId>,
}

/// A confirmation token for a destructive action.
///
/// Minted per selection and bound to the generation and the normalized node
/// set, so a token cannot be replayed against a different tree or a selection
/// the user did not see counted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct ConfirmationToken(String);

impl ConfirmationToken {
    /// Wraps a minted token.
    #[must_use]
    pub const fn from_encoded(text: String) -> Self {
        Self(text)
    }

    /// The token text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What happened to one item in a Trash request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct TrashItemResult {
    /// The selected node.
    pub node: NodeId,
    /// Where it was, for the undo affordance and the log.
    pub original: DisplayPath,
    /// Where it landed, when Foundation reported a resulting URL.
    pub trashed_to: Option<DisplayPath>,
    /// Bytes as recorded at scan time. Not re-measured: the point of the
    /// confirmation is that the user approved *these* numbers.
    pub scan_time_logical: u64,
    /// Allocated bytes as recorded at scan time.
    pub scan_time_allocated: u64,
    /// Why it stayed put, or `None` on success.
    pub error: Option<ActionError>,
}

/// One item in a Trash preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct TrashPreviewItem {
    /// The node that will be moved.
    pub node: NodeId,
    /// Its reconstructed path, escaped for display.
    pub path: DisplayPath,
    /// Logical bytes as recorded at scan time.
    pub logical: u64,
    /// Allocated bytes as recorded at scan time.
    pub allocated: u64,
}

/// What the confirmation sheet shows, plus the token that authorizes the move.
///
/// The selection is normalized first — a descendant whose ancestor is already
/// moving is dropped, and virtual groups and the scan root are rejected — so
/// the user confirms the count they will actually get.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct TrashPreview {
    /// The tree the selection was made against.
    pub generation: TreeGeneration,
    /// Fresh token, bound to this generation and this normalized set.
    pub token: ConfirmationToken,
    /// The normalized selection.
    pub items: Vec<TrashPreviewItem>,
    /// Sum of scan-time logical bytes.
    pub total_logical: u64,
    /// Sum of scan-time allocated bytes. APFS sharing can make physical
    /// recovery smaller than this; the sheet says so.
    pub total_allocated: u64,
    /// How many selected nodes were dropped because an ancestor is moving.
    pub dropped_descendants: u32,
    /// Nodes that cannot be trashed at all — virtual groups and the scan root.
    pub rejected: Vec<NodeId>,
}

/// Capacity and identity for one mounted volume, for the launch-screen picker.
///
/// Volume capacity is shown **beside** a scan's tree total, never silently
/// reconciled into it: clones, snapshots, purgeable space, exclusions,
/// unreadable data, and concurrent mutation all create legitimate deltas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct VolumeInfo {
    /// Display name, e.g. `Macintosh HD`.
    pub name: String,
    /// Mount point, escaped for display.
    pub mount_point: DisplayPath,
    /// `st_dev` of the mount point.
    pub device: u64,
    /// Filesystem type, e.g. `apfs`.
    pub fs_type: String,
    /// Total capacity in bytes.
    pub total_bytes: u64,
    /// Ordinary available capacity in bytes.
    pub available_bytes: u64,
    /// Foundation's "available for important usage", where macOS supplies it.
    /// It can include purgeable capacity and is labelled as such.
    pub important_available_bytes: Option<u64>,
    /// Whether this is the boot volume.
    pub is_root_volume: bool,
    /// Whether the volume is on removable media.
    pub is_removable: bool,
    /// Whether Time Machine local snapshots are present. v1 reports presence
    /// only: `tmutil` gives no authoritative byte total to subtract from
    /// `statfs`, so no acceptance gate depends on "snapshot bytes".
    pub has_local_snapshots: bool,
}

/// The outcome of a Trash request.
///
/// Items are processed independently. This reports partial success honestly
/// rather than pretending the batch was atomic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct TrashReport {
    /// The tree the selection was made against.
    pub generation: TreeGeneration,
    /// Items after normalization — descendants of a selected ancestor are
    /// dropped, and virtual groups and the scan root are rejected.
    pub requested: u32,
    /// How many reached the Trash.
    pub moved: u32,
    /// How many failed.
    pub failed: u32,
    /// Per-item outcomes, in the order they were attempted.
    pub items: Vec<TrashItemResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_survives_escaping_unchanged() {
        assert_eq!(
            DisplayPath::from_bytes("Björk – Homogenic.flac".as_bytes()).as_str(),
            "Björk – Homogenic.flac"
        );
        assert!(!DisplayPath::from_bytes(b"plain.txt").is_escaped());
    }

    #[test]
    fn undecodable_bytes_escape_reversibly() {
        let path = DisplayPath::from_bytes(&[b'a', 0xff, b'b', 0x80]);
        assert_eq!(path.as_str(), "a%FFb%80");
        assert!(path.is_escaped());
    }

    #[test]
    fn a_literal_percent_is_escaped_so_the_encoding_stays_reversible() {
        let path = DisplayPath::from_bytes(b"100%.txt");
        assert_eq!(path.as_str(), "100%25.txt");
    }

    #[test]
    fn a_truncated_multibyte_sequence_at_the_end_is_escaped_not_dropped() {
        // The leading byte of a three-byte sequence, with nothing after it.
        let path = DisplayPath::from_bytes(&[b'x', 0xe2, 0x82]);
        assert_eq!(path.as_str(), "x%E2%82");
    }

    #[test]
    fn empty_bytes_render_as_an_empty_path() {
        assert_eq!(DisplayPath::from_bytes(b"").as_str(), "");
    }

    #[test]
    fn sort_defaults_to_largest_logical_first() {
        let sort = Sort::default();
        assert_eq!(sort.key, SortKey::Logical);
        assert_eq!(sort.direction, SortDirection::Descending);
    }
}
