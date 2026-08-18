//! The completed-scan record, the scan options that produced it, and the
//! progress event that reports on one in flight.
//!
//! Every completed scan records its root, volume identity, times, options,
//! exclusion hash, aggregation threshold, unreadable count, observed entry
//! count, retained node count, and logical/allocated totals. UI totals are
//! labelled with that context; a number without it is not a claim this product
//! makes (docs/00-OVERVIEW.md#accuracy-contract).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorClassCount, ScanError};
use crate::id::{NodeId, ScanId, TreeGeneration};
use crate::tree::Tree;
use crate::wire::DisplayPath;

/// Where a scan is in its lifecycle.
///
/// ```text
/// Idle -> Scanning -> Cancelling -> Idle
///                  -> Finalizing -> Ready
///                  -> Failed     -> Idle
/// Ready -> Scanning   (the old completed scan stays visible until the
///                      replacement is ready)
/// ```
///
/// "Cancel acknowledged" and "all I/O resources closed" are distinct states,
/// because no design can promise a hard deadline for an uninterruptible remote
/// filesystem syscall. The frontend adds its own `Empty` and `Loaded` states;
/// they are presentation, not scan lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScanState {
    /// Nothing is running.
    #[default]
    Idle,
    /// Workers are reading and the builder is linking.
    Scanning,
    /// Cancellation was requested; producers are stopping.
    Cancelling,
    /// Traversal finished; rollup and validation are running. A partial scan
    /// never reaches this state.
    Finalizing,
    /// A tree was published and has a [`TreeGeneration`].
    Ready,
    /// The scan ended on a fatal error. Nothing is published.
    Failed,
}

/// How far cancellation has progressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancelState {
    /// The request was received. The UI acknowledges here, within 100 ms.
    Acknowledged,
    /// Producers stopped, handles closed, queued batches dropped, and the
    /// builder discarded its partial state. Measured separately from the
    /// acknowledgement, because it depends on the kernel.
    ResourcesClosed,
    /// The scan had already finished; there was nothing to cancel.
    AlreadyFinished,
    /// No scan with that [`ScanId`] is running.
    NotFound,
}

/// Include or exclude.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuleAction {
    /// Descend into, and retain, matches. Overrides a later exclude.
    Include,
    /// Skip matches without opening them.
    Exclude,
}

/// What part of a path a rule matches against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuleScope {
    /// One directory name component, so a directory literally named `Caches`
    /// matches without `MyCachesBackup` matching too.
    DirectoryName,
    /// The whole path relative to the scan root.
    RootRelativePath,
}

/// The pattern language a rule is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuleSyntax {
    /// Shell-style glob.
    Glob,
    /// Regular expression. Rejected at configuration load if it is invalid or
    /// pathologically expensive; no regex ever compiles inside the scan loop.
    Regex,
}

/// One exclusion or inclusion rule.
///
/// Rules are compiled once, ordered, and **first match wins**.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
pub struct ExclusionRule {
    /// Whether a match is kept or skipped.
    pub action: RuleAction,
    /// What the pattern is matched against.
    pub scope: RuleScope,
    /// The pattern language.
    pub syntax: RuleSyntax,
    /// The pattern itself.
    pub pattern: String,
    /// Whether matching is case sensitive. APFS can be configured either way,
    /// so this is never assumed.
    pub case_sensitive: bool,
}

/// Everything that changes what a scan observes.
///
/// Persisted with the result and hashed into
/// [`CompletedScan::exclusion_hash`], so two results are only comparable when
/// their options are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[non_exhaustive]
pub struct ScanOptions {
    /// Descend past a device boundary. Off by default; a mount marker is
    /// retained either way.
    pub cross_filesystems: bool,
    /// Count hard-linked content once per volume scan. On by default. Later
    /// entries stay visible and carry
    /// [`flags::HARD_LINK_REPEAT`](crate::flags::HARD_LINK_REPEAT).
    pub count_hard_links_once: bool,
    /// Prepend the shipped conservative root-scan exclusions.
    pub apply_default_exclusions: bool,
    /// Omit nodes smaller than this, folding their counts and sizes into the
    /// parent. `None` (the default) retains full detail. An aggregated result
    /// is visibly marked, and omitted paths cannot be searched, trashed,
    /// catalogued, or used in exact per-suffix statistics.
    pub aggregate_below_bytes: Option<u64>,
    /// Reader worker count. `None` selects the measured default, which is not
    /// `num_cpus` by reflex: too much concurrency turns a sequential disk
    /// workload into seek contention.
    pub workers: Option<u16>,
    /// Peak-RSS ceiling. When the published projection crosses it the scan ends
    /// with [`ScanError::MemoryLimit`] rather than at allocation failure.
    pub memory_limit_bytes: Option<u64>,
    /// Ordered rules, first match wins.
    pub exclusions: Vec<ExclusionRule>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            cross_filesystems: false,
            count_hard_links_once: true,
            apply_default_exclusions: true,
            aggregate_below_bytes: None,
            workers: None,
            memory_limit_bytes: None,
            exclusions: Vec::new(),
        }
    }
}

/// A hex-encoded configuration digest.
///
/// Travels with every snapshot and Parquet partition so old category ids stay
/// interpretable after the settings change that produced them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct ConfigHash(String);

impl ConfigHash {
    /// Renders a 32-byte digest as lowercase hex.
    #[must_use]
    pub fn from_digest(digest: &[u8; 32]) -> Self {
        const HEX: [u8; 16] = *b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(out)
    }

    /// Wraps an already-rendered digest.
    #[must_use]
    pub const fn from_hex(hex: String) -> Self {
        Self(hex)
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which volume a scan ran against.
///
/// Recorded so a diff can refuse two scans of different volumes, and so a
/// re-`stat` before Trash can tell "same path" from "same object".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct VolumeId {
    /// `st_dev` of the scan root at start.
    pub device: u64,
    /// Filesystem type as reported by `statfs`, e.g. `apfs`.
    pub fs_type: String,
    /// The volume UUID when macOS supplies one.
    pub volume_uuid: Option<String>,
    /// The mount point, escaped for display.
    pub mount_point: DisplayPath,
    /// Whether the volume preserves case in names.
    pub case_preserving: bool,
    /// Whether the volume distinguishes case in lookups. Never assumed: APFS
    /// can be configured either way.
    pub case_sensitive: bool,
}

/// Entry and node counts for one scan.
///
/// `observed_entries` and `retained_nodes` are tracked separately so an
/// aggregated scan cannot look like a complete inventory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ScanCounts {
    /// Entries the scanner saw, including ones aggregation omitted.
    pub observed_entries: u64,
    /// Nodes actually retained in the arena.
    pub retained_nodes: u64,
    /// Directories retained.
    pub directories: u64,
    /// Regular files retained.
    pub files: u64,
    /// Symlinks retained. Always leaves.
    pub symlinks: u64,
    /// Sockets, FIFOs, and devices retained.
    pub special: u64,
    /// Directories that could not be read.
    pub unreadable_dirs: u64,
    /// Paths skipped by an exclusion rule, counted separately from unreadable
    /// ones because they mean something completely different.
    pub excluded_paths: u64,
    /// Nodes whose children were folded in by `--aggregate-below`.
    pub aggregated_nodes: u64,
    /// Entries that were a later observation of an already-counted
    /// `(dev, ino)`.
    pub hard_link_repeats: u64,
}

/// Tree totals for one scan.
///
/// The two numbers are never summed or reconciled. Volume capacity from macOS
/// is shown *beside* them, never folded in: clones, snapshots, purgeable space,
/// exclusions, and concurrent mutation all create legitimate deltas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ScanTotals {
    /// Logical bytes over the scan root, after hard-link policy.
    pub logical: u64,
    /// Allocated bytes over the scan root, after hard-link policy. On APFS this
    /// is not a promise of uniquely reclaimable space.
    pub allocated: u64,
}

/// One immutable, published scan result.
///
/// Held as `Arc<CompletedScan>` and swapped atomically after validation and
/// rollup. Readers clone the `Arc`; nothing holds an application lock while
/// computing.
///
/// Deliberately **not** `Serialize`: it owns the whole [`Tree`], and putting
/// 69M nodes through serde would be neither fast nor bounded. The `*.rdstat`
/// container writes it, and [`CompletedScan::summary`] is the small projection
/// that crosses IPC.
///
/// A cancelled or partial scan never becomes one of these, is never saved, and
/// is never catalogued.
#[derive(Debug)]
pub struct CompletedScan {
    /// The attempt that produced this tree.
    pub scan_id: ScanId,
    /// The published generation. Commands quote it and reject stale ones.
    pub generation: TreeGeneration,
    /// The scan root, as filesystem bytes. Actions resolve from here plus
    /// stored components — never from a path supplied by the frontend.
    pub root_path: PathBuf,
    /// The arena node for the scan root.
    pub root: NodeId,
    /// Which volume this is.
    pub volume: VolumeId,
    /// Start time, Unix milliseconds.
    pub started_unix_ms: i64,
    /// End time, Unix milliseconds.
    pub finished_unix_ms: i64,
    /// The options in force.
    pub options: ScanOptions,
    /// Digest of the effective exclusion rule set.
    pub exclusion_hash: ConfigHash,
    /// Digest of the compiled category configuration.
    pub category_config_hash: ConfigHash,
    /// The build that produced this result.
    pub tool_version: String,
    /// Entry and node counts.
    pub counts: ScanCounts,
    /// Logical and allocated totals.
    pub totals: ScanTotals,
    /// Entries that vanished or changed while being read. A non-zero value is
    /// why the UI says "changed during scan" instead of implying precision it
    /// does not have.
    pub mutations: u64,
    /// The first [`MAX_DETAILED_ERRORS`](crate::MAX_DETAILED_ERRORS) failures,
    /// in full.
    pub errors: Vec<ScanError>,
    /// Uncapped counts by class, so a truncated detail list never makes a
    /// summary wrong.
    pub error_counts: Vec<ErrorClassCount>,
    /// Roots that an exclusion rule skipped, reported separately from
    /// unreadable paths.
    pub excluded_roots: Vec<DisplayPath>,
    /// The frozen arena.
    pub tree: Tree,
}

impl CompletedScan {
    /// Whether `--aggregate-below` omitted any node.
    ///
    /// An aggregated arena cannot produce a full-detail catalog, and its
    /// missing-detail contract travels with every partition it writes.
    #[must_use]
    pub const fn is_aggregated(&self) -> bool {
        self.options.aggregate_below_bytes.is_some()
    }

    /// Whether anything makes the totals a floor rather than a total.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        self.is_aggregated() || self.counts.unreadable_dirs > 0 || self.counts.excluded_paths > 0 || self.mutations > 0
    }

    /// Wall-clock duration in milliseconds.
    #[must_use]
    pub const fn duration_ms(&self) -> i64 {
        self.finished_unix_ms.saturating_sub(self.started_unix_ms)
    }

    /// The small, serializable projection that crosses IPC.
    #[must_use]
    pub fn summary(&self) -> ScanSummary {
        ScanSummary {
            scan_id: self.scan_id,
            generation: self.generation,
            root: self.root,
            root_path: DisplayPath::from_bytes(self.root_path.as_os_str().as_encoded_bytes()),
            volume: self.volume.clone(),
            started_unix_ms: self.started_unix_ms,
            finished_unix_ms: self.finished_unix_ms,
            options: self.options.clone(),
            exclusion_hash: self.exclusion_hash.clone(),
            category_config_hash: self.category_config_hash.clone(),
            tool_version: self.tool_version.clone(),
            counts: self.counts,
            totals: self.totals,
            mutations: self.mutations,
            error_counts: self.error_counts.clone(),
            excluded_roots: self.excluded_roots.clone(),
            aggregated: self.is_aggregated(),
            partial: self.is_partial(),
        }
    }
}

/// The IPC projection of a [`CompletedScan`]: everything except the tree.
///
/// `O(1)` in node count, which is the rule every command obeys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ScanSummary {
    /// The attempt that produced this tree.
    pub scan_id: ScanId,
    /// The published generation.
    pub generation: TreeGeneration,
    /// The arena node for the scan root.
    pub root: NodeId,
    /// The scan root, escaped for display only.
    pub root_path: DisplayPath,
    /// Which volume this is.
    pub volume: VolumeId,
    /// Start time, Unix milliseconds.
    pub started_unix_ms: i64,
    /// End time, Unix milliseconds.
    pub finished_unix_ms: i64,
    /// The options in force.
    pub options: ScanOptions,
    /// Digest of the effective exclusion rule set.
    pub exclusion_hash: ConfigHash,
    /// Digest of the compiled category configuration.
    pub category_config_hash: ConfigHash,
    /// The build that produced this result.
    pub tool_version: String,
    /// Entry and node counts.
    pub counts: ScanCounts,
    /// Logical and allocated totals.
    pub totals: ScanTotals,
    /// Entries that changed under the scan.
    pub mutations: u64,
    /// Uncapped error counts by class. The detail list stays in the backend.
    pub error_counts: Vec<ErrorClassCount>,
    /// Roots skipped by an exclusion rule.
    pub excluded_roots: Vec<DisplayPath>,
    /// Whether aggregation omitted nodes.
    pub aggregated: bool,
    /// Whether anything makes these totals a floor.
    pub partial: bool,
}

/// The application state machine, as one `O(1)` answer.
///
/// There is at most one active scan in v1. An old completed scan stays visible
/// while a replacement runs, which is why `generation` and `summary` can be
/// populated at the same time as `state == ScanState::Scanning`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ScanStatus {
    /// Where the lifecycle is, collapsed to one answer.
    ///
    /// The busiest state among the running scans, or `Ready`/`Idle` when none
    /// are. Kept alongside [`running`](Self::running) rather than replaced by
    /// it because "is this app busy" is a real question with a scalar answer,
    /// and every surface that only needs that — the tray, the window title —
    /// should not have to fold a list to get it.
    pub state: ScanState,
    /// The newest running scan, if any.
    ///
    /// Scalar for the same reason as [`state`](Self::state). A caller that
    /// cares which scans are running reads [`running`](Self::running).
    pub active_scan: Option<ScanId>,
    /// The most recently published tree, or [`TreeGeneration::NONE`].
    pub generation: TreeGeneration,
    /// That tree's summary, if there is one.
    pub summary: Option<ScanSummary>,
    /// The most recent progress snapshot from any scan, so a client that just
    /// connected does not have to wait up to 100 ms for the next event.
    pub last_progress: Option<ScanProgress>,
    /// Every scan that is running or waiting to run, oldest first.
    ///
    /// Empty when nothing is scanning. A scan appears here from the moment it
    /// is accepted — including while it is *waiting* for a device or for
    /// memory — because a user who clicked Scan and sees nothing concludes the
    /// click was lost.
    pub running: Vec<RunningScanRow>,
    /// Every completed tree still resident, so the user can switch between
    /// them. Bounded; the least-recently-read is evicted first and remains
    /// restorable from the snapshot store.
    pub ready: Vec<ReadyScanRow>,
}

/// Why a scan has been accepted but has not started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WaitReason {
    /// Another scan is already reading this filesystem. Running both would
    /// finish *later* than running them one after the other, so this one waits.
    SameDevice {
        /// The scan it is waiting behind.
        scan_id: ScanId,
    },
    /// Starting now could push the process past its memory budget.
    Memory,
}

/// One scan that is running, or waiting to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RunningScanRow {
    pub scan_id: ScanId,
    /// The generation this scan *will* publish under.
    ///
    /// Assigned at admission rather than at completion, so the UI can name the
    /// tree a progress bar is building before that tree exists — and so a
    /// user's click on a still-running scan has something to address.
    pub generation: TreeGeneration,
    /// The root, for a list showing several at once.
    pub root: DisplayPath,
    pub state: ScanState,
    /// `Some` while this scan is waiting rather than running. The UI shows the
    /// reason, because a progress bar at 0% with no explanation reads as broken.
    pub waiting: Option<WaitReason>,
    pub last_progress: Option<ScanProgress>,
}

/// One completed tree the user can switch to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ReadyScanRow {
    pub generation: TreeGeneration,
    pub scan_id: ScanId,
    pub root: DisplayPath,
    pub summary: ScanSummary,
}

/// One throttled progress snapshot.
///
/// Emitted at most [`PROGRESS_MAX_HZ`](crate::PROGRESS_MAX_HZ) times a second,
/// carrying **absolute counters** plus a sequence number, so a dropped event
/// costs nothing and events cannot be applied out of order. Completion is a
/// state transition returned by the supervisor, never inferred from a missing
/// progress event.
///
/// `src-tauri` emits this under
/// [`SCAN_PROGRESS_EVENT`](crate::SCAN_PROGRESS_EVENT). Because no crate under
/// `crates/*` may depend on `tauri`, the `tauri_specta::Event` derive lives in
/// `src-tauri` on a newtype wrapper around this type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ScanProgress {
    /// Which scan this is about. The frontend drops events from older ids.
    pub scan_id: ScanId,
    /// Monotonic per-scan sequence number.
    pub sequence: u64,
    /// Lifecycle state at the moment of the snapshot.
    pub state: ScanState,
    /// Entries observed so far.
    pub observed_entries: u64,
    /// Nodes retained so far.
    pub retained_nodes: u64,
    /// Directories completed so far.
    pub directories: u64,
    /// Logical bytes accumulated so far.
    pub logical_bytes: u64,
    /// Allocated bytes accumulated so far.
    pub allocated_bytes: u64,
    /// Errors recorded so far, all classes.
    pub errors: u64,
    /// Entries that changed under the scan so far.
    pub mutations: u64,
    /// Directories queued but not yet read. Exact, not an approximation:
    /// termination is decided by this counter, not by a heuristic.
    pub pending_dirs: u64,
    /// Occupancy of the bounded result channel, for spotting backpressure.
    pub result_queue_depth: u32,
    /// Milliseconds since the scan started.
    pub elapsed_ms: u64,
    /// Best-effort display text for the directory being read. Set through a
    /// non-blocking lock and skipped under contention, because a `format!` per
    /// entry in a scan worker is a bug.
    pub current_dir: Option<DisplayPath>,
    /// Current resident set size in bytes.
    pub rss_bytes: u64,
    /// Projected peak RSS at the current growth rate. Crossing the configured
    /// limit ends the scan with [`ScanError::MemoryLimit`] and offers a retry.
    pub projected_peak_rss_bytes: u64,
    /// Mean retained name length in bytes, for the memory profile.
    pub mean_name_bytes: u32,
    /// Retained directories as a fraction of retained nodes, in basis points.
    /// An integer, so nothing in the progress path touches a float.
    pub directory_ratio_bp: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_the_conservative_ones() {
        let options = ScanOptions::default();
        assert!(
            !options.cross_filesystems,
            "a volume boundary is not crossed by accident"
        );
        assert!(options.count_hard_links_once);
        assert!(options.apply_default_exclusions);
        assert_eq!(options.aggregate_below_bytes, None, "full detail is the default");
    }

    #[test]
    fn config_hash_renders_lowercase_hex() {
        let mut digest = [0_u8; 32];
        digest[0] = 0xde;
        digest[1] = 0xad;
        let hash = ConfigHash::from_digest(&digest);
        assert!(hash.as_str().starts_with("dead"));
        assert_eq!(hash.as_str().len(), 64);
    }

    #[test]
    fn progress_is_serializable_for_the_event_payload() {
        let progress = ScanProgress {
            scan_id: ScanId::FIRST,
            sequence: 7,
            ..ScanProgress::default()
        };
        let json = serde_json::to_string(&progress).expect("serializes");
        assert!(json.contains(r#""sequence":7"#), "{json}");
        assert!(json.contains(r#""state":"idle""#), "{json}");
    }
}
