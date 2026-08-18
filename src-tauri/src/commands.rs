//! The Tauri command layer.
//!
//! Thin by construction: every command validates, clones an `Arc`, hands the
//! work to [`spawn_blocking`](tauri::async_runtime::spawn_blocking) or a
//! dedicated thread, and maps the result onto a typed error. **No scanner,
//! layout, or filesystem logic lives here** — it lives in the modules this file
//! calls, and those modules exist to be replaced by `crates/*` at integration.
//!
//! Two rules bind every function below:
//!
//! - Blocking work never runs on the async command executor.
//! - A stale [`TreeGeneration`] is **rejected**, never applied to the current
//!   tree. That is what stops an old selection from acting on a new scan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rdirstat_core::{
    ActionError, AgeBucketEntry, AgeBucketRow, CancelState, CatalogScanId, CategoryEntry, CategoryId, CategoryRow,
    ChildPage, CommandError, CompletedScan, ConfirmationToken, Cursor, Details, DiffMetric, DiffReport, DisplayPath,
    DuplicateCandidateReport, LayoutKind, NodeId, QueryError, ReportName, ReportParams, ScanErrorReport, ScanId,
    ScanOptions, ScanStatus, SizeBandEntry, SizeBandRow, SnapshotOffer, Sort, StartError, TrashPreview, TrashReport,
    TreeGeneration, VolumeInfo,
};

use crate::engine::{self, ScanOutcome, ScanRequest};
use crate::query::Ancestor;
use crate::relocate::{RelocateError, RelocateMode, RelocatePlan, RelocateReport, SourceDisposal};
use crate::remote::{self, RemoteConfigError};
use crate::state::AppState;
use crate::storage::{self, StorageReport};
use crate::sync::{self, CompareMode, OnDiffer, SyncDiff, SyncError, SyncPlan, SyncReport};
use crate::transfers::{self, JobState, TransferId, TransferJob, TransferManager};
use crate::{actions, progress, query, relocate, volumes};
use rdirstat_remote::RemotePlan;
use rdirstat_remote::plan::RemoteCompare;

/// The ceiling on `scan_errors`'s sample list.
///
/// The frontend asks for what it will draw; this is the number it cannot
/// exceed, so a caller asking for a million samples gets a bounded payload
/// rather than the whole error log. The completed scan keeps
/// [`MAX_DETAILED_ERRORS`](rdirstat_core::MAX_DETAILED_ERRORS) of them and a
/// running one keeps far fewer, so this only ever truncates the finished case.
const MAX_ERROR_SAMPLES: usize = 200;

/// Ceiling on any report's breakdown list.
///
/// The "under 5 MiB" band on a boot volume holds ten million files. Expanding
/// it must return the heaviest few hundred, not attempt the rest — the row
/// already states the true count, and a ten-million-row payload is a file dump
/// rather than a breakdown.
///
/// Re-exported from core rather than redefined: this number had started to
/// exist separately in three places, which is three chances for one of them to
/// move alone.
use rdirstat_core::MAX_REPORT_ENTRIES as MAX_BAND_ENTRIES;

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// Validates a scan root before any slot is claimed.
fn validate_root(raw: &Path) -> Result<PathBuf, StartError> {
    let display = DisplayPath::from_bytes(raw.as_os_str().as_encoded_bytes());
    let metadata = std::fs::metadata(raw).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StartError::RootNotFound { path: display.clone() },
        std::io::ErrorKind::PermissionDenied => StartError::PermissionDenied {
            path: display.clone(),
            os_code: error.raw_os_error().unwrap_or(0),
        },
        _ => StartError::Internal(error.to_string()),
    })?;
    if !metadata.is_dir() {
        return Err(StartError::RootNotADirectory { path: display });
    }
    // Canonicalize so the recorded root is stable authority for every later
    // action, and so `..` in a frontend-supplied string cannot survive.
    std::fs::canonicalize(raw).map_err(|error| StartError::Internal(error.to_string()))
}

/// The most completions [`complete_path`] will return.
///
/// A directory with forty thousand children must not push forty thousand
/// strings across IPC to draw a menu that shows eight of them. This is a
/// *menu*, not a listing, and nothing tells the user how many were left out —
/// typing one more character is the disclosure, and a count would only invite
/// them to widen a prefix that is already too wide.
const MAX_PATH_COMPLETIONS: usize = 12;

/// Directory completions for a partially typed path.
///
/// Backs the scan bar's autocomplete. Deliberately **infallible**: a path that
/// does not exist, an unreadable directory, and a prefix naming a file all
/// yield an empty list rather than an error. A completion request is a
/// keystroke, not an action — raising `EACCES` for every character typed
/// through `/Library` would be noise, and the permission error that *means*
/// something still arrives from [`scan_start`].
///
/// Only directories are offered, because only a directory can be a scan root.
#[tauri::command]
#[specta::specta]
pub(crate) async fn complete_path(prefix: String) -> Vec<String> {
    tauri::async_runtime::spawn_blocking(move || complete_path_blocking(&prefix))
        .await
        .unwrap_or_default()
}

fn complete_path_blocking(prefix: &str) -> Vec<String> {
    let expanded = expand_tilde(prefix);

    // Split into the directory to read and the fragment to match against its
    // children. A trailing separator means "show me what is inside this", so
    // `/Users/` offers the children of `/Users` while `/Users` offers `/Users`
    // itself from the children of `/`.
    let Some(cut) = expanded.rfind('/') else {
        // No separator at all. Completing a bare word would mean guessing a
        // parent directory the user never named, and the likeliest guess —
        // the process's working directory — is meaningless in a GUI.
        return Vec::new();
    };
    let (dir, fragment) = (&expanded[..=cut], &expanded[cut + 1..]);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let wanted = fragment.to_lowercase();
    let mut matches: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            // `metadata` follows the link, which is what the user means here:
            // a symlink to a directory is a directory you can name as a root.
            // The scan still refuses to *descend* through symlinks — offering
            // one as a starting point is not the same as following it.
            entry.metadata().is_ok_and(|meta| meta.is_dir())
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Dot-directories appear only once the user has typed a dot, the
            // same bargain a shell makes: `/Volumes/` should not open with
            // `.Spotlight-V100` sitting above the volume the user wants.
            if name.starts_with('.') && !fragment.starts_with('.') {
                return None;
            }
            name.to_lowercase().starts_with(&wanted).then(|| format!("{dir}{name}"))
        })
        .collect();

    // Case-insensitively, so `/Volumes/a` does not sort `Archive` away from
    // `archive-scratch`. macOS is case-insensitive by default and the ordering
    // should not contradict the matching.
    matches.sort_by_key(|path| path.to_lowercase());
    matches.truncate(MAX_PATH_COMPLETIONS);
    matches
}

/// The most child directories one listing will return.
///
/// A destination browser is for steering, not for reading a directory out.
/// `/usr/share` has thousands of children and nobody picks a move target by
/// scrolling past two thousand rows — they type. The listing says when it has
/// been cut rather than pretending it is complete.
const MAX_BROWSE_ENTRIES: usize = 500;

/// One child directory offered by [`browse_directories`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub(crate) struct BrowseEntry {
    /// The leaf name, for display.
    pub name: String,
    /// The full path, which is what a later action is given.
    pub path: String,
}

/// What is inside a directory, for choosing a destination.
///
/// Deliberately NOT a scan: choosing where to put something needs the shape of
/// the filesystem, not the size of it. A destination on an 8 TB disk would cost
/// minutes to measure and the answer is not used — `relocate_plan` takes a
/// path, checks the destination's own properties, and never asks how big its
/// subtree is.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub(crate) struct BrowseListing {
    /// The directory actually read, after `~` expansion and canonicalisation.
    /// Echoed back because it may not be the string that was asked for.
    pub path: String,
    /// The parent, or `None` at the filesystem root, where "up" is not a move.
    pub parent: Option<String>,
    pub directories: Vec<BrowseEntry>,
    /// The listing was cut at [`MAX_BROWSE_ENTRIES`]. Said out loud: a browser
    /// that silently truncates is one that hides the folder you were looking
    /// for and lets you conclude it does not exist.
    pub truncated: bool,
    /// Why nothing could be listed. `Some` means the directory was not read at
    /// all, which is different from a directory that is genuinely empty, and
    /// the UI must be able to tell those apart.
    pub unreadable: Option<String>,
}

/// Lists the child directories of `path`, for the destination pane.
///
/// Unlike [`complete_path`] this reports failure, because the two answer
/// different questions. A completion is a guess offered while someone types and
/// an error there is noise; a browse is a deliberate "show me what is in here",
/// and "you cannot read this" is the answer rather than an interruption.
///
/// Files are omitted. A move target is a directory, and listing files would
/// offer choices that can only be refused later.
#[tauri::command]
#[specta::specta]
pub(crate) async fn browse_directories(path: String) -> BrowseListing {
    // Kept for the failure arm: the closure takes ownership, and a join error
    // still has to report which path it was asked about.
    let asked = path.clone();
    tauri::async_runtime::spawn_blocking(move || browse_blocking(&path))
        .await
        .unwrap_or_else(|error| BrowseListing {
            path: asked,
            parent: None,
            directories: Vec::new(),
            truncated: false,
            unreadable: Some(error.to_string()),
        })
}

fn browse_blocking(requested: &str) -> BrowseListing {
    let expanded = expand_tilde(requested);
    let raw = if expanded.is_empty() { "/".to_owned() } else { expanded };

    // Canonicalise so the echoed path and every child path are absolute and
    // symlink-free. A destination reached through a symlink is exactly how
    // `check_disjoint` used to be fooled into copying a tree into itself.
    let resolved = std::fs::canonicalize(&raw).unwrap_or_else(|_| PathBuf::from(&raw));
    let display = resolved.to_string_lossy().into_owned();
    let parent = resolved.parent().map(|parent| parent.to_string_lossy().into_owned());

    let entries = match std::fs::read_dir(&resolved) {
        Ok(entries) => entries,
        Err(error) => {
            return BrowseListing {
                path: display,
                parent,
                directories: Vec::new(),
                truncated: false,
                unreadable: Some(error.to_string()),
            };
        }
    };

    let mut directories: Vec<BrowseEntry> = entries
        .flatten()
        .filter(|entry| entry.metadata().is_ok_and(|meta| meta.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Dot-directories are hidden here, unlike in completion, because
            // there is no typed prefix to ask for them. `.Trashes` and
            // `.Spotlight-V100` are never a destination anyone means.
            if name.starts_with('.') {
                return None;
            }
            let path = entry.path().to_string_lossy().into_owned();
            Some(BrowseEntry { name, path })
        })
        .collect();

    directories.sort_by_key(|entry| entry.name.to_lowercase());
    let truncated = directories.len() > MAX_BROWSE_ENTRIES;
    directories.truncate(MAX_BROWSE_ENTRIES);

    BrowseListing {
        path: display,
        parent,
        directories,
        truncated,
        unreadable: None,
    }
}

/// Expands a leading `~`, which users type and no syscall accepts.
fn expand_tilde(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix('~') else {
        return raw.to_owned();
    };
    // `~other` names another user's home and resolving it needs the password
    // database. Leaving it alone yields no completions, which is honest;
    // guessing `/Users/other` would be wrong on any machine with a network
    // directory service.
    if !rest.is_empty() && !rest.starts_with('/') {
        return raw.to_owned();
    }
    match std::env::var_os("HOME") {
        Some(home) => format!("{}{rest}", home.to_string_lossy()),
        None => raw.to_owned(),
    }
}

/// Starts a scan and returns immediately. Results arrive as events plus a
/// `scan_status` transition.
///
/// # Errors
///
/// [`StartError::AlreadyScanning`] (v1 runs exactly one),
/// [`StartError::RootNotFound`], [`StartError::RootNotADirectory`],
/// [`StartError::PermissionDenied`], or [`StartError::InvalidOptions`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn scan_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    root: String,
    options: ScanOptions,
) -> Result<ScanId, StartError> {
    // Options and exclusion patterns are validated *before* a scan slot is
    // claimed, so a bad pattern is a typed refusal rather than a scan the user
    // watches start and then fail.
    engine::validate(&options)?;

    let requested = PathBuf::from(root);
    let root_path = tauri::async_runtime::spawn_blocking(move || validate_root(&requested))
        .await
        .map_err(|error| StartError::Internal(error.to_string()))??;

    let active = state.begin_scan()?;
    let generation = state.next_generation();
    let scan_id = active.scan_id;
    let counters = Arc::clone(&active.counters);
    let cancel = Arc::clone(&active.cancel);

    // A dedicated OS thread, not the blocking pool: a scan runs for minutes and
    // must not occupy a pool slot the rest of the app needs.
    let spawned = std::thread::Builder::new()
        .name("rdirstat-scan".to_owned())
        .spawn(move || {
            let emitter = progress::spawn_emitter(app.clone(), scan_id, Arc::clone(&counters));
            let outcome = engine::run(ScanRequest {
                root: root_path,
                options,
                scan_id,
                generation,
                cancel,
                counters,
            });
            emitter.stop();

            let state = tauri::Manager::state::<AppState>(&app);
            match outcome {
                ScanOutcome::Completed(scan) => {
                    let scan = Arc::from(scan);
                    // Published first, saved second. The tree is what the user
                    // asked for and it is ready now; writing gigabytes of arena
                    // must not stand between them and it. `publish` swaps an
                    // `Arc`, so the save below reads the same immutable tree the
                    // UI is already querying.
                    state.publish(Arc::clone(&scan));
                    save_snapshot(&app, &scan);
                }
                ScanOutcome::Cancelled => state.release_unpublished(false),
                ScanOutcome::Failed(error) => {
                    tracing::error!(%error, "scan failed");
                    state.release_unpublished(true);
                }
            }
        });

    if spawned.is_err() {
        state.release_unpublished(true);
        return Err(StartError::Internal("could not spawn the scan thread".to_owned()));
    }
    Ok(scan_id)
}

/// Writes the completed scan to the snapshot store so the next launch is a file
/// read instead of another full traversal.
///
/// Every failure is logged and swallowed. The scan itself succeeded and is
/// already published; a snapshot that could not be written costs the *next*
/// launch a rescan, which is exactly what happened before snapshots existed. A
/// full disk is the likely cause, and refusing to complete a scan over it would
/// turn a missing optimisation into a broken app.
///
/// Only complete scans reach here. A cancelled or failed scan is never
/// published and is therefore never saved — a partial arena must not come back
/// on the next launch wearing the totals of a whole volume.
fn save_snapshot(app: &tauri::AppHandle, scan: &CompletedScan) {
    let store = match crate::snapshot_store::SnapshotStore::new(app) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(%error, "no snapshot store; this scan will not survive a restart");
            return;
        }
    };
    match store.save(scan) {
        Ok(path) => tracing::info!(
            path = %path.display(),
            nodes = scan.tree.len(),
            "saved a snapshot of the completed scan"
        ),
        Err(error) => tracing::warn!(%error, "could not save a snapshot of the completed scan"),
    }
}

/// Requests cancellation of `scan_id`.
///
/// # Errors
///
/// Never fails today; the signature keeps [`ScanError`](rdirstat_core::ScanError)
/// available for a supervisor that can report one.
#[tauri::command]
#[specta::specta]
pub(crate) async fn scan_cancel(
    state: tauri::State<'_, AppState>,
    scan_id: ScanId,
) -> Result<CancelState, rdirstat_core::ScanError> {
    Ok(state.cancel(scan_id))
}

/// The `O(1)` state-machine answer for the launch screen and the status bar.
///
/// # Errors
///
/// Never fails; the umbrella type keeps the signature uniform.
#[tauri::command]
#[specta::specta]
pub(crate) async fn scan_status(state: tauri::State<'_, AppState>) -> Result<ScanStatus, CommandError> {
    Ok(state.status())
}

/// What the scan's recorded failures actually were.
///
/// Answers from the **running** scan's live counters when one is active, and
/// from the published tree otherwise, so the same affordance works while the
/// error count is still climbing and after it has stopped. `limit` bounds the
/// sample list; the per-class counts are uncapped, and
/// [`ScanErrorReport::truncated`] says when there were more.
///
/// # Errors
///
/// Never fails. No scan and no published tree is an empty report, not an
/// error: "nothing has gone wrong yet" is an answer.
#[tauri::command]
#[specta::specta]
pub(crate) async fn scan_errors(
    state: tauri::State<'_, AppState>,
    limit: u32,
) -> Result<ScanErrorReport, CommandError> {
    let limit = (limit as usize).min(MAX_ERROR_SAMPLES);

    if let Some(counters) = state.active_counters() {
        let counts = counters.error_counts();
        let total = counts.iter().map(|entry| entry.count).sum();
        let samples = counters.error_samples(limit);
        return Ok(ScanErrorReport {
            live: true,
            // A running scan has not published a tree, and the previously
            // published one is a different scan's. NONE is the honest answer.
            generation: TreeGeneration::NONE,
            total,
            counts,
            truncated: total > samples.len() as u64,
            samples,
        });
    }

    let Some(scan) = state.published() else {
        return Ok(ScanErrorReport::default());
    };
    let total = scan.error_counts.iter().map(|entry| entry.count).sum();
    let samples: Vec<_> = scan.errors.iter().take(limit).cloned().collect();
    Ok(ScanErrorReport {
        live: false,
        generation: scan.generation,
        total,
        counts: scan.error_counts.clone(),
        truncated: total > samples.len() as u64,
        samples,
    })
}

/// One bounded page of children. `limit` is clamped to
/// [`MAX_CHILD_PAGE`](rdirstat_core::MAX_CHILD_PAGE).
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`],
/// [`QueryError::UnknownNode`], [`QueryError::VirtualGroup`], or
/// [`QueryError::InvalidCursor`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn children(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    item: NodeId,
    sort: Sort,
    cursor: Option<Cursor>,
    limit: u32,
) -> Result<ChildPage, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let keys = state.token_keys().clone();
    tauri::async_runtime::spawn_blocking(move || query::children(&scan, &keys, item, sort, cursor.as_ref(), limit))
        .await
        .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// Details for one node, including one on-demand `lstat`.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`],
/// [`QueryError::UnknownNode`], or [`QueryError::PathTooDeep`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn node_details(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
) -> Result<Details, QueryError> {
    let scan = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || query::details(&scan, node))
        .await
        .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// The size-band histogram for a subtree.
///
/// Always returns every band, including empty ones — "there is nothing over
/// 50 GiB here" is an answer, and a table whose rows appear and vanish as the
/// user drills is harder to read than one that does not move.
///
/// `O(subtree)`, which is why it runs on the blocking pool: the arena is already
/// in memory, but a whole-volume subtree is millions of nodes and that is not
/// work for the async executor.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn size_bands(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
) -> Result<Vec<SizeBandRow>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::bands::size_bands(&scan.tree, node).ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// The largest files inside one size band, for the breakdown accordion.
///
/// A leaderboard, not an enumeration: the smallest band on a boot volume holds
/// ten million files, so `limit` is a hard ceiling and the caller already knows
/// the true count from `size_bands`.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn size_band_entries(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    band: u8,
    limit: u32,
) -> Result<Vec<SizeBandEntry>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let capped = (limit as usize).min(MAX_BAND_ENTRIES);
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::bands::size_band_entries(&scan.tree, node, usize::from(band), capped)
            .ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// What is on disk for each mounted volume, so a switcher can offer a restore.
///
/// Cheap by construction: a directory listing plus a header-and-metadata read
/// per volume, never an arena decode. This is called every time a menu opens.
///
/// A volume with no snapshot and a volume whose snapshot this build cannot read
/// both report `has_snapshot: false` — from the caller's side they are the same
/// answer, "restoring is not on offer here", and distinguishing them would put a
/// failure in a menu that the user can do nothing about.
///
/// # Errors
///
/// Never fails: a store that cannot be resolved is reported as "no snapshots"
/// rather than as an error, because the switcher still works without them.
#[tauri::command]
#[specta::specta]
pub(crate) async fn snapshot_offers(app: tauri::AppHandle) -> Result<Vec<SnapshotOffer>, CommandError> {
    let offers = tauri::async_runtime::spawn_blocking(move || {
        let Ok(store) = crate::snapshot_store::SnapshotStore::new(&app) else {
            return Vec::new();
        };
        volumes::list()
            .into_iter()
            .map(|volume| {
                let root = PathBuf::from(volume.mount_point.as_str());
                let info = store.snapshot_info(&root, volume.device);
                SnapshotOffer {
                    mount_point: volume.mount_point,
                    device: volume.device,
                    has_snapshot: info.is_some(),
                    // Stated, never implied. A snapshot can be stale by any
                    // amount, and an interface that silently restores a
                    // two-week-old tree while the user believes they are looking
                    // at their disk is worse than one that costs a rescan.
                    taken_unix_ms: info.map(|found| found.taken_unix_ms),
                    nodes: info.map(|found| found.nodes),
                    bytes: info.map(|found| found.bytes),
                }
            })
            .collect()
    })
    .await
    .map_err(|error| CommandError::Internal(error.to_string()))?;
    Ok(offers)
}

/// Restores a previously saved scan for one volume, instead of rescanning it.
///
/// This is what makes switching drives cheap: a root that has been scanned
/// before comes back as a file read rather than as minutes of traversal.
///
/// **A restore is not a rescan, and the caller must not present it as one.**
/// The tree it publishes is as old as its snapshot, so anything created since
/// is missing from it. `snapshot_offers` reports when each was taken precisely
/// so the offer can say so.
///
/// # Errors
///
/// [`CommandError::Internal`] when there is no readable snapshot for that root,
/// or when a scan is running — a scan about to publish its own tree must not
/// have one replaced underneath it.
#[tauri::command]
#[specta::specta]
pub(crate) async fn restore_snapshot(
    app: tauri::AppHandle,
    root: String,
    device: u64,
) -> Result<TreeGeneration, CommandError> {
    let requested = PathBuf::from(root);
    let restored = tauri::async_runtime::spawn_blocking(move || {
        let store = crate::snapshot_store::SnapshotStore::new(&app)
            .map_err(|error| CommandError::Internal(error.to_string()))?;
        let found = store
            .load_for_root(&requested, device)
            .ok_or_else(|| CommandError::Internal("no readable snapshot for that volume".to_owned()))?;
        let state = tauri::Manager::state::<AppState>(&app);
        state
            .publish_restored(*found.scan)
            .ok_or_else(|| CommandError::Internal("a scan is running; cancel it first".to_owned()))
    })
    .await
    .map_err(|error| CommandError::Internal(error.to_string()))??;
    Ok(restored)
}

/// Content-category totals for a subtree — the Types report.
///
/// `O(subtree)`, so it runs on the blocking pool and the caller fetches it only
/// while the route is showing.
///
/// Categories with no files are omitted. `rdirstat-core` cannot enumerate the
/// category table — that lives in `rdirstat-classify`, which depends on core,
/// not the other way round — so "every category" would mean 256 rows of which
/// most name nothing.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn category_totals(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
) -> Result<Vec<CategoryRow>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::by_category::category_totals(&scan.tree, node).ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// The largest files in one content category.
///
/// A leaderboard, not an enumeration; see [`MAX_BAND_ENTRIES`].
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn category_entries(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    category: CategoryId,
    limit: u32,
) -> Result<Vec<CategoryEntry>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let capped = usize::try_from(limit).unwrap_or(MAX_BAND_ENTRIES).min(MAX_BAND_ENTRIES);
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::by_category::category_entries(&scan.tree, node, category, capped)
            .ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// Age buckets for a subtree — the Ages report.
///
/// `now_unix_seconds` is supplied by the caller rather than read from the clock
/// here, because a function whose answer depends on the wall clock cannot be
/// tested and because the *same* value has to reach both this command and
/// [`age_bucket_entries`]. A mismatch is not detectable by the backend and would
/// silently produce a file list that disagrees with the count above it.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn age_buckets(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    now_unix_seconds: i64,
) -> Result<Vec<AgeBucketRow>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::by_age::age_buckets(&scan.tree, node, now_unix_seconds).ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// The largest files in one age bucket.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn age_bucket_entries(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    now_unix_seconds: i64,
    bucket: u8,
    limit: u32,
) -> Result<Vec<AgeBucketEntry>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let capped = usize::try_from(limit).unwrap_or(MAX_BAND_ENTRIES).min(MAX_BAND_ENTRIES);
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::by_age::age_bucket_entries(&scan.tree, node, now_unix_seconds, usize::from(bucket), capped)
            .ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// Same-size file groups — the Dupes report.
///
/// **Candidates, not duplicates.** Nothing is opened and no content is hashed,
/// so this reports files that share a logical size and says so in the payload:
/// `content_verified` is always false at this stage, and the recovery figure is
/// an upper bound rather than a promise. Same-size is not same-content, and on
/// APFS two copies may already be clones sharing their storage.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::UnknownNode`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn duplicate_candidates(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    max_clusters: u32,
    max_members: u32,
) -> Result<DuplicateCandidateReport, QueryError> {
    let scan = state.tree_for_query(generation)?;
    // Zero means "use the backend ceiling", which is what the frontend sends.
    let clusters = usize::try_from(max_clusters).unwrap_or(0);
    let members = usize::try_from(max_members).unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || {
        rdirstat_core::dupes::duplicate_candidates(&scan.tree, node, clusters, members)
            .ok_or(QueryError::UnknownNode { node })
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// Reads candidate duplicates and groups them by SHA-256 of their contents.
///
/// The duplicates panel groups by size, which finds *candidates*: two files of
/// the same length are not the same file. This is the step that reads the
/// bytes, and it is deliberately a separate command rather than part of
/// [`duplicate_candidates`] — grouping by size is instant and this is not, so
/// the expensive half happens when the user asks for it and on the set they
/// asked about.
///
/// # Errors
///
/// [`QueryError::NoScan`] or [`QueryError::StaleGeneration`]. Per-file
/// problems are reported inside the result rather than failing the batch: one
/// unreadable file must not cost the answer for the other ninety-nine.
#[tauri::command]
#[specta::specta]
pub(crate) async fn verify_duplicates(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    nodes: Vec<NodeId>,
) -> Result<crate::verify::VerifyReport, QueryError> {
    let scan = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || crate::verify::verify(&scan, &nodes))
        .await
        .map_err(|error| QueryError::Internal(error.to_string()))
}

/// Compares the published scan against the previous snapshot of the same root.
///
/// **Both halves of the comparison are chosen here, not by the caller.** The
/// frontend picks the metric and the row cap; it does not get to say which two
/// scans are being compared, because a diff whose labels can disagree with its
/// data is worse than no diff.
///
/// ## Why it can refuse
///
/// docs/06-DATA.md requires compatible detail thresholds. Two scans taken with
/// different aggregation or different exclusion rules are not comparable: every
/// entry the stricter scan dropped shows up as thousands of spurious
/// "removed" rows, and the result looks like a catastrophe rather than a
/// configuration difference. Refusing with a reason is the honest answer; the
/// UI shows it instead of a wrong diff.
///
/// Memory: this holds TWO arenas at once, which at the design profile is twice
/// the node array against a 5.0 GiB ceiling. The previous tree is dropped as
/// soon as the report is built and is deliberately never cached.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`], or
/// [`QueryError::Internal`] carrying the reason no comparison is possible.
#[tauri::command]
#[specta::specta]
pub(crate) async fn scan_diff(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    metric: DiffMetric,
    limit: u32,
) -> Result<DiffReport, QueryError> {
    let live = state.tree_for_query(generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        let store =
            crate::snapshot_store::SnapshotStore::new(&app).map_err(|error| QueryError::Internal(error.to_string()))?;
        let previous = store
            .load_previous_for_root(&live.root_path, live.volume.device)
            .ok_or_else(|| QueryError::Internal("only one saved scan for this volume".to_owned()))?;

        // The check that stops a configuration difference from reading as a
        // catastrophe. Compared before any work is done, because the walk
        // itself would succeed and produce a confidently wrong answer.
        let before = &previous.scan;
        if before.options.aggregate_below_bytes != live.options.aggregate_below_bytes {
            return Err(QueryError::Internal(
                "the two scans used different aggregation thresholds, so entries one of them omitted \
                 would appear as changes"
                    .to_owned(),
            ));
        }
        if before.exclusion_hash != live.exclusion_hash {
            return Err(QueryError::Internal(
                "the two scans used different exclusion rules, so paths one of them skipped would \
                 appear as changes"
                    .to_owned(),
            ));
        }

        let describe = |scan: &CompletedScan| rdirstat_core::diff::DiffScanInfo {
            root: DisplayPath::from_bytes(path_as_bytes(&scan.root_path)),
            taken_unix_ms: Some(scan.finished_unix_ms),
            nodes: u64::try_from(scan.tree.len()).unwrap_or(u64::MAX),
        };
        let options = rdirstat_core::diff::DiffOptions::new(describe(before), describe(&live))
            .with_metric(metric)
            .with_limit(usize::try_from(limit).unwrap_or(rdirstat_core::diff::MAX_DIFF_ENTRIES));

        let report = rdirstat_core::diff::diff_trees(&before.tree, before.root, &live.tree, live.root, options);
        // Explicit: the previous arena is the largest thing this command holds,
        // and it must not outlive the report it was needed for.
        drop(previous);
        report
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))?
}

/// A scan root as filesystem bytes, for display only.
#[cfg(unix)]
fn path_as_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes()
}

/// The escaped full path of a node, for display and Copy Path only.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`],
/// [`QueryError::UnknownNode`], [`QueryError::VirtualGroup`], or
/// [`QueryError::PathTooDeep`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn path_of(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    item: NodeId,
) -> Result<DisplayPath, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let mut bytes = Vec::with_capacity(128);
    scan.tree.path_bytes(item, &mut bytes)?;
    Ok(DisplayPath::from_bytes(&bytes))
}

/// The chain from the scan root down to a node, for the breadcrumb.
///
/// Cheap — `O(depth)` with no `stat` — so the shell can call it on every
/// navigation instead of trying to remember where the user has been.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`],
/// [`QueryError::UnknownNode`], or [`QueryError::PathTooDeep`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn ancestors(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
) -> Result<Vec<Ancestor>, QueryError> {
    let scan = state.tree_for_query(generation)?;
    query::ancestors(&scan, node)
}

/// What a relocation would do, plus the token that authorizes it.
///
/// Returns a plan even when the relocation cannot proceed: the reasons are the
/// point, and [`RelocatePlan::token`] is `None` for anything unactionable. The
/// frontend must key the confirm button on the token, not on the call
/// succeeding.
///
/// # Errors
///
/// [`RelocateError`] only for a request that cannot be described at all — an
/// unknown node, a path outside the scan root, or a destination that is not an
/// absolute, `..`-free path.
#[tauri::command]
#[specta::specta]
pub(crate) async fn relocate_plan(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    destination: String,
    mode: RelocateMode,
    disposal: SourceDisposal,
) -> Result<RelocatePlan, RelocateError> {
    let scan = state.tree_for_action(generation).map_err(RelocateError::Action)?;
    let destination = PathBuf::from(destination);
    // The destination is the one path in the whole app that comes from the
    // frontend rather than from the arena, so it is validated here rather than
    // trusted. Everything else is reconstructed from stored components.
    if !relocate::is_acceptable_destination(&destination) {
        return Err(RelocateError::Destination {
            path: DisplayPath::from_bytes(destination.as_os_str().as_encoded_bytes()),
            reason: "the destination must be an absolute path with no `..` segments".to_owned(),
        });
    }
    let keys = state.token_keys().clone();
    tauri::async_runtime::spawn_blocking(move || {
        relocate::plan(
            &scan,
            &keys,
            now_unix_ms(),
            relocate::RelocateRequest {
                node,
                destination_parent: &destination,
                mode,
                disposal,
            },
        )
    })
    .await
    .map_err(|error| RelocateError::Internal(error.to_string()))?
}

/// Executes a planned relocation: copy, verify, dispose, symlink.
///
/// **Long-running and blocking.** A multi-gigabyte subtree is copied and then
/// read back in full for verification, so this can run for minutes. It is on
/// `spawn_blocking` for that reason.
///
/// # Errors
///
/// [`RelocateError`]. Every failure before the disposal step leaves the source
/// untouched; [`RelocateError::SymlinkFailed`] is the sole exception and says
/// so in its message.
#[tauri::command]
#[specta::specta]
pub(crate) async fn relocate_apply(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
    destination: String,
    mode: RelocateMode,
    disposal: SourceDisposal,
    confirmation: ConfirmationToken,
) -> Result<RelocateReport, RelocateError> {
    let scan = state.tree_for_action(generation).map_err(RelocateError::Action)?;
    let destination = PathBuf::from(destination);
    if !relocate::is_acceptable_destination(&destination) {
        return Err(RelocateError::Destination {
            path: DisplayPath::from_bytes(destination.as_os_str().as_encoded_bytes()),
            reason: "the destination must be an absolute path with no `..` segments".to_owned(),
        });
    }
    let keys = state.token_keys().clone();
    tauri::async_runtime::spawn_blocking(move || {
        relocate::apply(
            &scan,
            &keys,
            now_unix_ms(),
            relocate::RelocateRequest {
                node,
                destination_parent: &destination,
                mode,
                disposal,
            },
            &confirmation,
        )
    })
    .await
    .map_err(|error| RelocateError::Internal(error.to_string()))?
}

/// What the app has stored on disk.
///
/// Reads the snapshot store's directory and peeks each file's header — never
/// decodes an arena, so this stays kilobytes per file rather than the hundreds
/// of megabytes one holds. Safe to call whenever the panel opens.
///
/// # Errors
///
/// Never. A store that does not exist yet is an empty report, not a failure:
/// that is the ordinary state of a fresh install and the panel still has to
/// render.
#[tauri::command]
#[specta::specta]
pub(crate) async fn storage_report(app: tauri::AppHandle) -> Result<StorageReport, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        // Resolved, not created. A configured store on an unmounted disk must
        // be *described* — that is exactly when the user opens this panel —
        // and creating the directory would write a decoy store onto the mount
        // point instead.
        crate::snapshot_store::resolve(&app).map_or_else(
            |error| storage::empty_report(&error.to_string()),
            |resolved| storage::describe(&resolved),
        )
    })
    .await
    .map_err(|error| CommandError::Internal(error.to_string()))
}

/// Points the snapshot store at `directory`, or back at the default.
///
/// `None` clears the setting. The path is validated before it is saved, not
/// after: a stored location the app cannot write to would fail at the end of
/// the next scan, which is the most expensive possible moment to find out.
///
/// This does **not** move existing snapshots. They are a cache, keyed by a
/// digest of the scanned root, so a store that starts empty refills itself on
/// the next scan of each volume — and moving gigabytes as a side effect of
/// changing a preference is not something a settings control should do
/// silently. The panel says where the old files are so they can be moved or
/// deleted deliberately.
///
/// # Errors
///
/// [`CommandError::Internal`] with a reason the user can act on: a relative
/// path, a path that is not a directory and cannot be created, or one that
/// exists but rejects a write.
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_snapshot_dir(
    app: tauri::AppHandle,
    directory: Option<String>,
) -> Result<StorageReport, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let app_data =
            crate::snapshot_store::app_data_dir(&app).map_err(|error| CommandError::Internal(error.to_string()))?;

        let chosen = match directory.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(validate_store_dir(Path::new(raw))?),
        };

        let mut settings = crate::settings::load(&app_data);
        settings.snapshot_dir = chosen;
        crate::settings::save(&app_data, &settings)
            .map_err(|error| CommandError::Internal(format!("could not save settings: {error}")))?;

        crate::snapshot_store::resolve(&app).map_or_else(
            |error| Ok(storage::empty_report(&error.to_string())),
            |resolved| Ok(storage::describe(&resolved)),
        )
    })
    .await
    .map_err(|error| CommandError::Internal(error.to_string()))?
}

/// Accepts a directory only if the app can actually write there.
///
/// Creates it when it is missing, because "choose a folder that does not exist
/// yet" is a reasonable thing to want and refusing would send the user to
/// Finder to make an empty directory by hand. It does not create *parents*
/// beyond one level for the same reason `/Volumes/big/snapshots` must not be
/// silently manufactured while the disk is unplugged: one missing level is a
/// new folder, several is a wrong path.
fn validate_store_dir(path: &Path) -> Result<PathBuf, CommandError> {
    if !path.is_absolute() {
        return Err(CommandError::Internal(
            "the snapshot folder must be an absolute path".to_owned(),
        ));
    }
    if path.components().any(|part| part.as_os_str() == "..") {
        return Err(CommandError::Internal(
            "the snapshot folder must not contain `..`".to_owned(),
        ));
    }
    if !path.is_dir() {
        let parent = path
            .parent()
            .ok_or_else(|| CommandError::Internal("that path has no parent directory".to_owned()))?;
        if !parent.is_dir() {
            return Err(CommandError::Internal(format!(
                "{} does not exist, so {} cannot be created there",
                parent.display(),
                path.display()
            )));
        }
        std::fs::create_dir(path)
            .map_err(|error| CommandError::Internal(format!("could not create {}: {error}", path.display())))?;
    }

    let probe = path.join(format!(".rdirstat-write-probe.{}", std::process::id()));
    std::fs::File::create(&probe)
        .map_err(|error| CommandError::Internal(format!("{} is not writable: {error}", path.display())))?;
    let _ = std::fs::remove_file(&probe);

    Ok(path.to_path_buf())
}

/// Copies one stored snapshot to a path the user chose.
///
/// `destination_dir` is the folder to write into; empty means the user's
/// Downloads folder. The snapshot keeps its own filename, and the full path
/// written is returned so the UI can say where it went.
///
/// A byte-for-byte copy, so the original checksum still verifies when the file
/// is restored later. Both paths are validated against the store rather than
/// trusted: `source` must be inside it, and neither may be relative or contain
/// `..`. Without that this command is an arbitrary-file-read primitive.
///
/// # Errors
///
/// [`CommandError::Internal`] carrying the reason — a source outside the
/// store, a destination that already exists, or any I/O failure.
#[tauri::command]
#[specta::specta]
pub(crate) async fn export_snapshot(
    app: tauri::AppHandle,
    source: String,
    destination_dir: String,
) -> Result<String, CommandError> {
    // Resolved here rather than in the frontend: the webview has no business
    // knowing where the user's home is, and a path it invented would be one
    // more attacker-controlled string for `export_snapshot` to validate.
    let fallback = tauri::Manager::path(&app)
        .download_dir()
        .map_err(|error| CommandError::Internal(format!("no Downloads folder: {error}")))?;

    tauri::async_runtime::spawn_blocking(move || {
        let store = crate::snapshot_store::SnapshotStore::new(&app)
            .map_err(|error| CommandError::Internal(error.to_string()))?;
        let directory = if destination_dir.trim().is_empty() {
            fallback
        } else {
            PathBuf::from(destination_dir)
        };
        storage::export_snapshot(store.directory(), Path::new(&source), &directory)
            .map(|written| written.display().to_string())
            .map_err(CommandError::Internal)
    })
    .await
    .map_err(|error| CommandError::Internal(error.to_string()))?
}

/// What a folder sync would copy, and the token that authorizes it.
///
/// Walks the source and `stat`s each matching destination path. Under
/// `Verify` it also compares the contents of same-sized files, which reads
/// both sides in full — the frontend offers that as an explicit choice
/// because the cost is proportional to the overlap, not to the difference.
///
/// # Errors
///
/// [`SyncError`] for a path that is relative, missing, not a directory, or
/// that overlaps the other side. Everything else is reported inside the plan.
#[tauri::command]
#[specta::specta]
pub(crate) async fn sync_plan(
    state: tauri::State<'_, AppState>,
    source: String,
    destination: String,
    compare_mode: CompareMode,
    on_differ: OnDiffer,
) -> Result<SyncPlan, SyncError> {
    let keys = state.token_keys().clone();
    // A sync reads no scanned tree, so there is no generation for it to be
    // stale against — it works with no scan loaded at all. The token is scoped
    // to a fixed generation and does its real work by binding the plan's SHAPE
    // (file count, byte count), which `apply` recomputes and re-checks. The
    // TTL, not the generation, is what expires a forgotten confirmation.
    let generation = TreeGeneration::FIRST;
    tauri::async_runtime::spawn_blocking(move || {
        sync::plan(
            &keys,
            generation,
            now_unix_ms(),
            sync::SyncRequest {
                source: Path::new(&source),
                destination: Path::new(&destination),
                compare_mode,
                on_differ,
            },
        )
    })
    .await
    .map_err(|error| SyncError::Internal(error.to_string()))?
}

/// Runs a planned folder sync.
///
/// **Long-running and blocking.** Re-plans from scratch and verifies the token
/// against the fresh shape, so a source that changed since the review fails
/// closed rather than copying something the user never saw counted.
///
/// # Errors
///
/// [`SyncError::InvalidConfirmation`] when the plan no longer matches, or any
/// path error from planning. Per-file copy failures are reported inside the
/// report rather than aborting the run.
#[tauri::command]
#[specta::specta]
pub(crate) async fn sync_apply(
    state: tauri::State<'_, AppState>,
    source: String,
    destination: String,
    compare_mode: CompareMode,
    on_differ: OnDiffer,
    confirmation: ConfirmationToken,
) -> Result<SyncReport, SyncError> {
    let keys = state.token_keys().clone();
    let generation = TreeGeneration::FIRST;
    tauri::async_runtime::spawn_blocking(move || {
        sync::apply(
            &keys,
            generation,
            now_unix_ms(),
            sync::SyncRequest {
                source: Path::new(&source),
                destination: Path::new(&destination),
                compare_mode,
                on_differ,
            },
            &confirmation,
        )
    })
    .await
    .map_err(|error| SyncError::Internal(error.to_string()))?
}

/// What two folders each hold, for the side-by-side view.
///
/// Symmetric and read-only. It takes a left and a right rather than a source
/// and a destination because the direction is chosen *after* looking, and it
/// mints no token: copying still goes through [`sync_plan`] and [`sync_apply`],
/// which are the only things that may authorize a write.
///
/// `differences_only` drops rows the two sides agree about. The row listing is
/// capped, and in a real pair of folders the agreements outnumber the
/// differences by orders of magnitude, so leaving them in spends the cap on
/// rows nobody needs to read. The counts are complete either way.
///
/// # Errors
///
/// [`SyncError`] for a path that is relative, missing, not a directory, or that
/// overlaps the other side.
#[tauri::command]
#[specta::specta]
pub(crate) async fn sync_diff(
    _state: tauri::State<'_, AppState>,
    left: String,
    right: String,
    compare_mode: CompareMode,
    differences_only: bool,
) -> Result<SyncDiff, SyncError> {
    tauri::async_runtime::spawn_blocking(move || {
        sync::diff(
            sync::DiffRequest {
                left: Path::new(&left),
                right: Path::new(&right),
                compare_mode,
                differences_only,
            },
            sync::MAX_DIFF_ROWS,
        )
    })
    .await
    .map_err(|error| SyncError::Internal(error.to_string()))?
}

/// Mounted local volumes, for the launch screen.
///
/// # Errors
///
/// [`CommandError::Internal`] if the blocking task could not be joined.
#[tauri::command]
#[specta::specta]
pub(crate) async fn volumes(_state: tauri::State<'_, AppState>) -> Result<Vec<VolumeInfo>, CommandError> {
    tauri::async_runtime::spawn_blocking(volumes::list)
        .await
        .map_err(|error| CommandError::Internal(error.to_string()))
}

/// What a Trash request would do, plus the token that authorizes it.
///
/// # Errors
///
/// [`ActionError::NoScan`] or [`ActionError::StaleGeneration`]. Per-item
/// problems are reported inside [`TrashPreview::rejected`], not as an error.
#[tauri::command]
#[specta::specta]
pub(crate) async fn trash_preview(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    nodes: Vec<NodeId>,
) -> Result<TrashPreview, ActionError> {
    let scan = state.tree_for_action(generation)?;
    let keys = state.token_keys().clone();
    tauri::async_runtime::spawn_blocking(move || actions::preview(&scan, &keys, now_unix_ms(), &nodes))
        .await
        .map_err(|error| ActionError::Internal(error.to_string()))
}

/// Moves a confirmed selection to the Trash through `NSFileManager`.
///
/// # Errors
///
/// [`ActionError::NoScan`], [`ActionError::StaleGeneration`], or
/// [`ActionError::InvalidConfirmation`]. Per-item failures are reported inside
/// [`TrashReport`], which is honest about partial success rather than
/// pretending the batch was atomic.
#[tauri::command]
#[specta::specta]
pub(crate) async fn move_to_trash(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    nodes: Vec<NodeId>,
    confirmation: ConfirmationToken,
) -> Result<TrashReport, ActionError> {
    let scan = state.tree_for_action(generation)?;
    let keys = state.token_keys().clone();
    tauri::async_runtime::spawn_blocking(move || {
        actions::move_to_trash(&scan, &keys, now_unix_ms(), &nodes, &confirmation)
    })
    .await
    .map_err(|error| ActionError::Internal(error.to_string()))?
}

/// Reveals a node in Finder.
///
/// # Errors
///
/// [`ActionError::NoScan`], [`ActionError::StaleGeneration`],
/// [`ActionError::NotActionable`] (the scan root or a virtual group),
/// [`ActionError::ChangedSinceScan`], or [`ActionError::OutsideScanRoot`].
#[tauri::command]
#[specta::specta]
pub(crate) async fn reveal_in_finder(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    node: NodeId,
) -> Result<(), ActionError> {
    let scan = state.tree_for_action(generation)?;
    tauri::async_runtime::spawn_blocking(move || actions::reveal(&scan, node))
        .await
        .map_err(|error| ActionError::Internal(error.to_string()))?
}

// ---------------------------------------------------------------------------
// Binary commands.
//
// `layout` and `report` return `tauri::ipc::Response`, which carries the Arrow
// IPC stream as raw bytes rather than a JSON number array. `tauri::ipc::Response`
// does not implement `specta::Type`, so these two cannot be collected by
// `tauri_specta::collect_commands!`; `lib.rs` routes them to a plain
// `tauri::generate_handler!` by command name. Their argument types *are*
// exported (see `Builder::typ` in `lib.rs`), so the frontend still gets typed
// parameters and calls `invoke<ArrayBuffer>("layout", …)`.
// ---------------------------------------------------------------------------

/// The tile batch for one hierarchy view, as Arrow IPC.
///
/// Geometry and serialization belong to `rdirstat-treemap`; this command only
/// resolves the generation, moves the work off the async executor, and hands
/// back the bytes.
///
/// # Errors
///
/// [`QueryError::NoScan`], [`QueryError::StaleGeneration`],
/// [`QueryError::UnknownNode`], or [`QueryError::Internal`] naming the
/// offending viewport field. Note a `<Files>` group root is **not** an error:
/// `rdirstat-treemap` lays the group out as its owner's direct files, so
/// double-clicking a group row is not a dead end.
#[tauri::command]
#[allow(
    clippy::too_many_arguments,
    reason = "a Tauri command's parameters are its wire signature, not a function's design. \
              Grouping them into a struct would change the IPC shape the frontend sends \
              and buy nothing: each one is an independent scalar the caller supplies."
)]
pub(crate) async fn layout(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    root: NodeId,
    kind: LayoutKind,
    viewport: rdirstat_core::Viewport,
    min_px: f32,
    categories: Option<Vec<u8>>,
    metric: Option<rdirstat_treemap::SizeMetric>,
) -> Result<tauri::ipc::Response, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let response = tauri::async_runtime::spawn_blocking(move || {
        // The frontend sends CATEGORY IDS, never families. A family is a
        // presentation grouping that can change without the arena changing, so
        // it is expanded on the frontend and the backend never learns it exists.
        //
        // An empty list collapses to "no filter": filtering everything out is
        // never what a click meant, and a blank canvas is not a useful answer to
        // give back for one.
        let filter = categories
            .filter(|ids| !ids.is_empty())
            .map(|ids| rdirstat_treemap::CategorySet::from_ids(&ids));
        // Which byte count the AREAS encode. docs/05-UI.md makes logical vs
        // allocated an explicit user choice "so a screenshot is never ambiguous
        // about which number it is showing" — and a picture drawn from one
        // number while the toolbar names the other is exactly that ambiguity.
        // The toolbar sends its choice; `None` keeps the crate's default.
        let metric = metric.unwrap_or_default();

        if !scan.tree.contains(root) {
            return Err(QueryError::UnknownNode { node: root });
        }
        // A canvas that has not been measured yet asks for a 0-by-0 viewport,
        // which `LayoutOptions::new` rejects. `layout` answers that case with an
        // empty batch rather than an error, so it stays the one that handles it
        // — an empty batch has no geometry, so which metric it was drawn from
        // cannot matter.
        let unmeasured = viewport.width.is_finite()
            && viewport.height.is_finite()
            && (viewport.width <= 0.0 || viewport.height <= 0.0);
        if unmeasured {
            return rdirstat_treemap::layout(&scan.tree, generation, root, kind, viewport, min_px);
        }

        let options = rdirstat_treemap::LayoutOptions::new(kind, viewport, min_px)?.with_metric(metric);
        match filter {
            None => Ok(rdirstat_treemap::layout_with(&scan.tree, generation, root, &options)?),
            Some(set) => {
                let options = options.with_categories(Some(set));
                // Reused across a window resize: the weights do not depend on
                // the viewport, and rebuilding them per drag step is the
                // difference between paying 163 ms once and paying it
                // continuously while the edge is being dragged.
                let cache = tauri::Manager::state::<AppState>(&app);
                let weights = cache.filter_weights(&scan, root, options.metric, set);
                let tiles = rdirstat_treemap::layout_tiles_with(&scan.tree, root, &options, Some(&weights))?;
                Ok(rdirstat_treemap::tiles_to_response(&tiles, generation)?)
            }
        }
    })
    .await
    .map_err(|error| QueryError::Internal(error.to_string()))??;
    Ok(tauri::ipc::Response::new(response.into_bytes()))
}

/// A named report over a saved catalog scan.
///
/// # Errors
///
/// Always [`QueryError::NoCatalogScan`] in this build: `rdirstat-catalog`, its
/// Parquet writer, and DuckDB are phase 6 and are deliberately not compiled in.
/// The command exists so the contract is complete and the frontend can route on
/// the real variant instead of a missing command.
#[tauri::command]
pub(crate) async fn report(
    _state: tauri::State<'_, AppState>,
    _catalog_scan: CatalogScanId,
    _name: ReportName,
    _params: ReportParams,
) -> Result<tauri::ipc::Response, QueryError> {
    Err(QueryError::NoCatalogScan)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // complete_path
    // -----------------------------------------------------------------------

    /// Builds a fixture whose shape exercises every filter at once.
    fn completion_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["Movies", "movies-backup", "Music", "Documents", ".hidden"] {
            std::fs::create_dir(dir.path().join(name)).expect("mkdir");
        }
        std::fs::write(dir.path().join("Moviefile.txt"), b"x").expect("write");
        dir
    }

    fn complete(dir: &tempfile::TempDir, fragment: &str) -> Vec<String> {
        let prefix = format!("{}/{fragment}", dir.path().display());
        complete_path_blocking(&prefix)
            .into_iter()
            .filter_map(|path| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect()
    }

    #[test]
    fn completion_matches_a_prefix_case_insensitively() {
        let dir = completion_fixture();
        // Lowercase "mov" must still reach "Movies": macOS is case-insensitive
        // by default, so matching that is not would contradict the filesystem.
        assert_eq!(complete(&dir, "mov"), vec!["Movies", "movies-backup"]);
    }

    #[test]
    fn completion_offers_directories_only() {
        let dir = completion_fixture();
        // `Moviefile.txt` shares the prefix and is a file. Only a directory can
        // be a scan root, so offering it would be offering a dead end.
        assert!(!complete(&dir, "Movie").contains(&"Moviefile.txt".to_owned()));
    }

    #[test]
    fn completion_hides_dot_directories_until_a_dot_is_typed() {
        let dir = completion_fixture();
        assert!(!complete(&dir, "").contains(&".hidden".to_owned()));
        assert_eq!(complete(&dir, "."), vec![".hidden"]);
    }

    #[test]
    fn a_trailing_separator_lists_the_directory_itself() {
        let dir = completion_fixture();
        // "" after the separator is the "show me what is in here" case, and it
        // must not collapse to "match everything anywhere".
        let all = complete(&dir, "");
        assert!(all.contains(&"Music".to_owned()), "{all:?}");
        assert!(all.contains(&"Documents".to_owned()), "{all:?}");
    }

    #[test]
    fn completion_is_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..(MAX_PATH_COMPLETIONS * 3) {
            std::fs::create_dir(dir.path().join(format!("d{index:04}"))).expect("mkdir");
        }
        let prefix = format!("{}/d", dir.path().display());
        assert_eq!(complete_path_blocking(&prefix).len(), MAX_PATH_COMPLETIONS);
    }

    #[test]
    fn an_unreadable_or_absent_directory_yields_no_completions_rather_than_an_error() {
        // The whole point of the infallible signature: a keystroke mid-path
        // names a directory that does not exist yet, and that is not an error.
        assert!(complete_path_blocking("/nonexistent-abcxyz/foo").is_empty());
        assert!(complete_path_blocking("").is_empty());
        // No separator: nothing to anchor the listing against.
        assert!(complete_path_blocking("Movies").is_empty());
    }

    // -----------------------------------------------------------------------
    // browse_directories
    // -----------------------------------------------------------------------

    #[test]
    fn browsing_lists_child_directories_and_omits_files_and_dot_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["Movies", "Archive", ".Trashes"] {
            std::fs::create_dir(dir.path().join(name)).expect("mkdir");
        }
        std::fs::write(dir.path().join("note.txt"), b"x").expect("write");

        let listing = browse_blocking(&dir.path().to_string_lossy());
        let names: Vec<&str> = listing.directories.iter().map(|e| e.name.as_str()).collect();
        // Sorted case-insensitively, files dropped, dot-directories dropped:
        // none of the three is a destination anyone means to pick.
        assert_eq!(names, vec!["Archive", "Movies"], "{listing:?}");
        assert!(listing.unreadable.is_none());
        assert!(!listing.truncated);
    }

    #[test]
    fn a_child_path_is_absolute_so_an_action_can_use_it_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("Target")).expect("mkdir");
        let listing = browse_blocking(&dir.path().to_string_lossy());
        let entry = listing.directories.first().expect("one entry");
        assert!(std::path::Path::new(&entry.path).is_absolute(), "{entry:?}");
        assert!(entry.path.ends_with("/Target"), "{entry:?}");
    }

    #[test]
    fn an_unreadable_directory_says_so_rather_than_looking_empty() {
        // The distinction the UI depends on: "nothing in here" and "I could not
        // look" must not render the same, or a permission problem reads as an
        // empty destination and the user picks it.
        let listing = browse_blocking("/nonexistent-abcxyz-browse");
        assert!(listing.unreadable.is_some(), "{listing:?}");
        assert!(listing.directories.is_empty());
    }

    #[test]
    fn browsing_is_capped_and_admits_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..(MAX_BROWSE_ENTRIES + 5) {
            std::fs::create_dir(dir.path().join(format!("d{index:04}"))).expect("mkdir");
        }
        let listing = browse_blocking(&dir.path().to_string_lossy());
        assert_eq!(listing.directories.len(), MAX_BROWSE_ENTRIES);
        assert!(listing.truncated, "a cut listing must say it was cut");
    }

    #[test]
    fn the_root_has_no_parent_and_everything_else_does() {
        assert_eq!(browse_blocking("/").parent, None);
        let dir = tempfile::tempdir().expect("tempdir");
        let child = dir.path().join("child");
        std::fs::create_dir(&child).expect("mkdir");
        let listing = browse_blocking(&child.to_string_lossy());
        assert!(listing.parent.is_some(), "{listing:?}");
    }

    #[test]
    fn a_bare_tilde_expands_but_another_users_home_does_not() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = home.to_string_lossy().into_owned();
        assert_eq!(expand_tilde("~/Movies"), format!("{home}/Movies"));
        assert_eq!(expand_tilde("~"), home);
        // `~other` needs the password database; guessing /Users/other is wrong
        // on any machine with a directory service, so it is left verbatim.
        assert_eq!(expand_tilde("~other/x"), "~other/x");
        assert_eq!(expand_tilde("/already/absolute"), "/already/absolute");
    }

    #[test]
    fn a_missing_root_is_named_before_a_slot_is_claimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = validate_root(&dir.path().join("nope")).expect_err("this call must be rejected");
        assert!(matches!(error, StartError::RootNotFound { .. }), "{error:?}");
    }

    #[test]
    fn a_file_is_not_a_scan_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").expect("write");
        assert!(matches!(
            validate_root(&file).expect_err("this call must be rejected"),
            StartError::RootNotADirectory { .. }
        ));
    }

    #[test]
    fn a_valid_root_is_canonicalized() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("inner")).expect("mkdir");
        let messy = dir.path().join("inner").join(".");
        let root = validate_root(&messy).expect("valid");
        assert!(root.is_absolute());
        assert!(!root.to_string_lossy().contains("/./"));
    }

    #[test]
    fn regex_rules_are_refused_before_a_scan_starts() {
        // `ScanOptions` is `#[non_exhaustive]`; assign rather than literal.
        let mut options = ScanOptions::default();
        options.exclusions = vec![rdirstat_core::ExclusionRule {
            action: rdirstat_core::RuleAction::Exclude,
            scope: rdirstat_core::RuleScope::RootRelativePath,
            syntax: rdirstat_core::RuleSyntax::Regex,
            pattern: "^tmp".to_owned(),
            case_sensitive: true,
        }];
        assert!(engine::validate(&options).is_err());
    }
}

// ---------------------------------------------------------------------------
// Remote destinations and the transfer queue
//
// Same shape as the local sync above — check, review, confirm — with one extra
// step in front of it, because a remote destination has to be *configured*
// before it can be planned against and there is nowhere to type a bucket name
// into a folder picker.
//
// The division of labour: `rdirstat-remote` decides what to copy and how to
// reach the far side, `crate::remote` owns the saved list and the Keychain,
// `crate::transfers` owns the queue, and this file does none of those things.
// ---------------------------------------------------------------------------

/// What the caller wants uploaded, and how carefully.
///
/// One struct shared by [`remote_plan`] and [`transfer_enqueue`], so the two
/// cannot disagree: the token minted against a plan is only valid for a
/// transfer made from the *same* settings, and four loose arguments repeated at
/// two call sites is how they drift.
#[derive(Clone, Debug, serde::Deserialize, specta::Type)]
pub(crate) struct TransferRequest {
    /// The local folder to upload. Absolute.
    pub source: String,
    /// The saved destination's name.
    pub target: String,
    pub compare: RemoteCompare,
    pub on_differ: OnDiffer,
}

/// A remote plan plus the token that authorizes acting on it.
///
/// Two structs rather than a `token` field on `RemotePlan` itself, because the
/// plan is computed by a crate that knows nothing about this process's token
/// keys — and should not, since minting one is the act of authorizing a write.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
pub(crate) struct RemotePlanView {
    /// `None` when the transfer cannot proceed. The UI keys its button on this.
    pub token: Option<ConfirmationToken>,
    pub plan: RemotePlan,
}

/// The presets the destination editor offers.
///
/// A constant, so this needs no state and cannot fail.
#[tauri::command]
#[specta::specta]
pub(crate) fn remote_profiles() -> Vec<rdirstat_remote::Profile> {
    rdirstat_remote::PROFILES.to_vec()
}

/// The saved destinations, with whether each has a stored secret — never the
/// secret itself.
///
/// # Errors
///
/// [`RemoteConfigError::Internal`] if the app data directory is unavailable.
#[tauri::command]
#[specta::specta]
pub(crate) async fn remote_targets(app: tauri::AppHandle) -> Result<Vec<remote::TargetView>, RemoteConfigError> {
    let app_data = app_data(&app)?;
    // Reads a file and queries the Keychain once per target, so it is blocking
    // work and does not belong on the command executor.
    tauri::async_runtime::spawn_blocking(move || remote::list(&app_data))
        .await
        .map_err(|error| RemoteConfigError::Internal(error.to_string()))
}

/// Adds a destination, or updates the one named by `replacing`.
///
/// The secret fields follow the rule in [`remote::SecretInput`]: absent means
/// *leave what is stored alone*, and an empty string means *remove it*. The UI
/// cannot send back a secret it was never shown, so this is what lets a user
/// edit a bucket's folder without re-entering a key.
///
/// # Errors
///
/// [`RemoteConfigError`] for a bad field, a duplicate name, a full list, or a
/// keychain that refused.
#[tauri::command]
#[specta::specta]
pub(crate) async fn remote_save_target(
    app: tauri::AppHandle,
    target: rdirstat_remote::RemoteTarget,
    secret: remote::SecretInput,
    replacing: Option<String>,
) -> Result<rdirstat_remote::RemoteTarget, RemoteConfigError> {
    let app_data = app_data(&app)?;
    tauri::async_runtime::spawn_blocking(move || remote::upsert(&app_data, &target, secret, replacing.as_deref()))
        .await
        .map_err(|error| RemoteConfigError::Internal(error.to_string()))?
}

/// Forgets a destination and its stored secret.
///
/// Does **not** touch anything at the destination. Removing a bucket from this
/// list is removing a bookmark, and a delete button that quietly deleted 400 GB
/// of backups would be the worst button in the app.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`], or a keychain failure.
#[tauri::command]
#[specta::specta]
pub(crate) async fn remote_delete_target(app: tauri::AppHandle, name: String) -> Result<(), RemoteConfigError> {
    let app_data = app_data(&app)?;
    tauri::async_runtime::spawn_blocking(move || remote::remove(&app_data, &name))
        .await
        .map_err(|error| RemoteConfigError::Internal(error.to_string()))?
}

/// Confirms a destination is reachable and its credentials work.
///
/// Cheap and non-destructive: it does not list, because a bucket the user can
/// write but not list is an ordinary least-privilege setup and probing with a
/// list would call a working destination broken.
///
/// # Errors
///
/// [`RemoteConfigError::Invalid`] carrying what the endpoint said.
#[tauri::command]
#[specta::specta]
pub(crate) async fn remote_probe(app: tauri::AppHandle, name: String) -> Result<(), RemoteConfigError> {
    let remote = open(&app, &name).await?;
    remote
        .probe()
        .await
        .map_err(|error| RemoteConfigError::Invalid(error.to_string()))
}

/// Describes what uploading `source` to a destination would do.
///
/// **Long-running.** One recursive listing of the destination, then a local
/// walk against it; under `Verify` it also reads every same-sized local file in
/// full to hash it.
///
/// # Errors
///
/// [`RemoteConfigError`] when the destination cannot be opened or listed.
#[tauri::command]
#[specta::specta]
pub(crate) async fn remote_plan(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: TransferRequest,
) -> Result<RemotePlanView, RemoteConfigError> {
    let TransferRequest {
        source,
        target,
        compare,
        on_differ,
    } = request;
    let source = PathBuf::from(&source);
    if !source.is_absolute() {
        return Err(RemoteConfigError::Invalid(
            "the folder to upload must be an absolute path".to_owned(),
        ));
    }
    let remote = open(&app, &target).await?;
    let (listing, listing_truncated) = remote
        .list()
        .await
        .map_err(|error| RemoteConfigError::Invalid(error.to_string()))?;

    let available_comparison = remote.comparison();
    let destination = remote.display().to_owned();
    let walked = source.clone();
    let plan = tauri::async_runtime::spawn_blocking(move || {
        rdirstat_remote::plan::plan(rdirstat_remote::plan::RemotePlanRequest {
            source: &walked,
            destination: &destination,
            listing: &listing,
            listing_truncated,
            available_comparison,
            compare,
            on_differ,
            // The display cap. The transfer itself re-plans uncapped.
            max_entries: rdirstat_remote::plan::MAX_PLANNED_ENTRIES,
        })
    })
    .await
    .map_err(|error| RemoteConfigError::Internal(error.to_string()))?;

    // Bound to the plan's SHAPE, exactly as the local sync's token is: what
    // must not drift between the review and the confirm is the set of files and
    // the counts the user actually looked at. There is no scanned tree behind a
    // transfer, so the generation is fixed and the TTL is what expires a
    // forgotten confirmation.
    let token = (plan.total_to_copy > 0).then(|| {
        crate::token::mint(
            state.token_keys(),
            TreeGeneration::FIRST,
            now_unix_ms(),
            &[plan_identity(&plan)],
        )
    });
    Ok(RemotePlanView { token, plan })
}

/// The shape of a plan, as the confirmation token binds it.
fn plan_identity(plan: &RemotePlan) -> crate::token::ItemIdentity {
    crate::token::ItemIdentity {
        node: NodeId::from_raw(0),
        device: plan.total_to_copy,
        inode: plan.bytes_to_copy,
    }
}

/// Every transfer, newest first.
///
/// # Errors
///
/// [`RemoteConfigError::Internal`] if the app data directory is unavailable.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfers(
    queue: tauri::State<'_, Arc<TransferManager>>,
) -> Result<Vec<TransferJob>, RemoteConfigError> {
    Ok(queue.list().await)
}

/// Queues an upload and starts it.
///
/// Re-plans before copying and checks the token against the fresh shape, so a
/// source that changed between the review and the confirm fails closed — the
/// same rule as [`sync_apply`].
///
/// # Errors
///
/// [`RemoteConfigError::Invalid`] when the confirmation no longer matches the
/// plan, or the destination cannot be opened.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfer_enqueue(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    queue: tauri::State<'_, Arc<TransferManager>>,
    request: TransferRequest,
    confirmation: ConfirmationToken,
) -> Result<TransferJob, RemoteConfigError> {
    let TransferRequest {
        source,
        target,
        compare,
        on_differ,
    } = request.clone();
    let source_path = PathBuf::from(&source);
    if !source_path.is_absolute() {
        return Err(RemoteConfigError::Invalid(
            "the folder to upload must be an absolute path".to_owned(),
        ));
    }
    let saved = {
        let app_data = app_data(&app)?;
        let name = target.clone();
        tauri::async_runtime::spawn_blocking(move || remote::find(&app_data, &name))
            .await
            .map_err(|error| RemoteConfigError::Internal(error.to_string()))??
    };

    // The token was minted against a plan the user reviewed. It is verified
    // here rather than only at upload time so that a stale confirmation is
    // refused before a job appears in the queue looking legitimate.
    let plan = remote_plan(app.clone(), state.clone(), request).await?;
    crate::token::verify(
        state.token_keys(),
        &confirmation,
        TreeGeneration::FIRST,
        now_unix_ms(),
        &[plan_identity(&plan.plan)],
    )
    .map_err(|_| RemoteConfigError::Invalid("this plan is no longer valid; check the folder again".to_owned()))?;

    let job = queue
        .enqueue(
            &source_path,
            &target,
            &saved.display(),
            compare,
            on_differ,
            now_unix_ms(),
        )
        .await;
    start_worker(app, Arc::clone(&queue), job.id);
    Ok(job)
}

/// Asks a transfer to stop where it is. Resumable.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`] for an unknown transfer.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfer_pause(
    queue: tauri::State<'_, Arc<TransferManager>>,
    id: TransferId,
) -> Result<TransferJob, RemoteConfigError> {
    stop(&queue, id, false).await
}

/// Stops a transfer for good. What already arrived stays where it is.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`] for an unknown transfer.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfer_cancel(
    queue: tauri::State<'_, Arc<TransferManager>>,
    id: TransferId,
) -> Result<TransferJob, RemoteConfigError> {
    stop(&queue, id, true).await
}

/// Restarts a paused or failed transfer.
///
/// Re-plans from scratch, which is the whole resume mechanism: files that made
/// it are found at the destination and skipped. See `transfers`' module docs.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`], or [`RemoteConfigError::Invalid`] when the
/// transfer is not in a state that can be resumed.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfer_resume(
    app: tauri::AppHandle,
    queue: tauri::State<'_, Arc<TransferManager>>,
    id: TransferId,
) -> Result<TransferJob, RemoteConfigError> {
    let job = queue
        .get(id)
        .await
        .ok_or_else(|| RemoteConfigError::NotFound(format!("transfer {}", id.get())))?;
    if !job.state.is_resumable() {
        return Err(RemoteConfigError::Invalid(
            "this transfer is not waiting to be resumed".to_owned(),
        ));
    }
    let job = queue
        .set_state(id, JobState::Queued, now_unix_ms())
        .await
        .ok_or_else(|| RemoteConfigError::NotFound(format!("transfer {}", id.get())))?;
    start_worker(app, Arc::clone(&queue), id);
    Ok(job)
}

/// Removes finished transfers from the list. Running ones are left alone.
///
/// # Errors
///
/// Infallible today; typed for symmetry with the rest of the queue commands.
#[tauri::command]
#[specta::specta]
pub(crate) async fn transfers_clear(queue: tauri::State<'_, Arc<TransferManager>>) -> Result<u32, RemoteConfigError> {
    let removed = queue.clear_finished(now_unix_ms()).await;
    Ok(u32::try_from(removed).unwrap_or(u32::MAX))
}

/// Stops a running transfer, or moves a waiting one straight to its end state.
async fn stop(queue: &TransferManager, id: TransferId, cancel: bool) -> Result<TransferJob, RemoteConfigError> {
    let missing = || RemoteConfigError::NotFound(format!("transfer {}", id.get()));
    let job = queue.get(id).await.ok_or_else(missing)?;

    // A running job is asked to stop and writes its own final state, so that
    // the state on disk is always one a worker actually reached. A job that is
    // merely queued has no worker to ask, and is moved directly.
    if queue.request_stop(id, cancel).await {
        return Ok(job);
    }
    let state = if cancel { JobState::Cancelled } else { JobState::Paused };
    queue.set_state(id, state, now_unix_ms()).await.ok_or_else(missing)
}

/// Opens a saved destination.
async fn open(app: &tauri::AppHandle, name: &str) -> Result<rdirstat_remote::Remote, RemoteConfigError> {
    let app_data = app_data(app)?;
    let name = name.to_owned();
    // Reads a file and the Keychain, both blocking.
    tauri::async_runtime::spawn_blocking(move || {
        let target = remote::find(&app_data, &name)?;
        let credentials = remote::credentials(&target)?;
        rdirstat_remote::connect(&target, &credentials).map_err(|error| RemoteConfigError::Invalid(error.to_string()))
    })
    .await
    .map_err(|error| RemoteConfigError::Internal(error.to_string()))?
}

/// The application data directory, or a typed error.
fn app_data(app: &tauri::AppHandle) -> Result<PathBuf, RemoteConfigError> {
    crate::snapshot_store::app_data_dir(app).map_err(|error| RemoteConfigError::Internal(error.to_string()))
}

/// Spawns the task that runs one job.
///
/// Detached on purpose. The command that started it returns as soon as the job
/// is in the queue, because the queue *is* the handle — the UI watches job
/// state, and a command that awaited a four-hour upload would be a command that
/// times out.
fn start_worker(app: tauri::AppHandle, queue: Arc<TransferManager>, id: TransferId) {
    tauri::async_runtime::spawn(async move {
        let Some(job) = queue.get(id).await else {
            return;
        };
        let remote = match open(&app, &job.target_name).await {
            Ok(remote) => remote,
            Err(error) => {
                queue.update_failed(id, error.to_string(), now_unix_ms()).await;
                return;
            }
        };
        if let Err(error) = transfers::run_job(&queue, &remote, id, now_unix_ms).await {
            tracing::warn!(id = id.get(), %error, "a transfer ended badly");
        }
    });
}
