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
    ActionError, CancelState, CatalogScanId, ChildPage, CommandError, CompletedScan, ConfirmationToken, Cursor,
    Details, DisplayPath, LayoutKind, NodeId, QueryError, ReportName, ReportParams, ScanErrorReport, ScanId,
    ScanOptions, ScanStatus, SizeBandRow, Sort, StartError, TrashPreview, TrashReport, TreeGeneration, VolumeInfo,
};

use crate::engine::{self, ScanOutcome, ScanRequest};
use crate::query::Ancestor;
use crate::relocate::{RelocateError, RelocateMode, RelocatePlan, RelocateReport, SourceDisposal};
use crate::state::AppState;
use crate::{actions, progress, query, relocate, volumes};

/// The ceiling on `scan_errors`'s sample list.
///
/// The frontend asks for what it will draw; this is the number it cannot
/// exceed, so a caller asking for a million samples gets a bounded payload
/// rather than the whole error log. The completed scan keeps
/// [`MAX_DETAILED_ERRORS`](rdirstat_core::MAX_DETAILED_ERRORS) of them and a
/// running one keeps far fewer, so this only ever truncates the finished case.
const MAX_ERROR_SAMPLES: usize = 200;

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
pub(crate) async fn layout(
    state: tauri::State<'_, AppState>,
    generation: TreeGeneration,
    root: NodeId,
    kind: LayoutKind,
    viewport: rdirstat_core::Viewport,
    min_px: f32,
) -> Result<tauri::ipc::Response, QueryError> {
    let scan = state.tree_for_query(generation)?;
    let response = tauri::async_runtime::spawn_blocking(move || {
        rdirstat_treemap::layout(&scan.tree, generation, root, kind, viewport, min_px)
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
