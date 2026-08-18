//! RDirStat — the Tauri desktop shell.
//!
//! This crate is the composition root and the native macOS boundary. It owns
//! application state, typed commands and events, and the permission-facing
//! actions. It owns **no** scan algorithm and **no** arena structure: every DTO
//! and every error crossing IPC is defined in `rdirstat-core`, and this crate
//! declares zero parallel types.
//!
//! ## Module map
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`state`] | `RwLock<Option<Arc<CompletedScan>>>`, the one-active-scan state machine, monotonic ids |
//! | [`commands`] | the twelve `#[tauri::command]` entry points, and nothing else |
//! | [`progress`] | scan atomics and the 10 Hz emitter thread |
//! | [`events`] | the `tauri_specta::Event` wrapper `rdirstat-core` cannot derive |
//! | [`cursor`] / [`token`] | the two opaque wire strings core leaves to the shell |
//! | [`query`] | child paging and node details against a frozen tree |
//! | [`actions`] | Reveal and Trash, including identity revalidation |
//! | [`fsident`] | path reconstruction, `lstat`, and error classification |
//! | [`snapshot_store`] | where `*.rdstat` files live, atomic writes, retention, restore-at-launch |
//! | [`volumes`] | the launch screen's volume list |
//! | [`engine`] | **integration seam** — the temporary home of `rdirstat-scan` |
//! | [`layout`] | **integration seam** — the temporary home of `rdirstat-treemap` |
//!
//! ## Binary responses
//!
//! `layout` and `report` return `tauri::ipc::Response` carrying an Arrow IPC
//! stream. `tauri::ipc::Response` does not implement `specta::Type`, so those
//! two commands are dispatched by a plain `tauri::generate_handler!` that
//! [`invoke_handler`] routes to by name; the other fourteen go through
//! `tauri-specta`. Their argument and error types are still exported to
//! TypeScript, so the frontend calls
//! `invoke<ArrayBuffer>("layout", { generation, root, kind, viewport, minPx })`
//! with typed arguments and reads the batch with `tableFromIPC`.

mod actions;
mod commands;
mod cursor;
mod digest;
mod engine;
mod events;
mod fsident;
mod progress;
mod query;
mod relocate;
mod remote;
mod schedules;
mod settings;
mod snapshot_store;
mod state;
mod storage;
mod sync;
mod token;
mod transfers;
mod tray;
mod updates;
mod verify;
mod volumes;

pub use crate::events::{ScanProgressEvent, TransferProgressEvent};
pub use crate::state::AppState;

/// Commands that answer with raw bytes instead of JSON. Exported as a constant
/// so the generated TypeScript names them rather than leaving them folklore.
const BINARY_COMMANDS: [&str; 2] = ["layout", "report"];

/// Loads the saved transfer queue and wires its change events to the frontend.
///
/// The listener is installed here, at the one place that has an `AppHandle`,
/// so that `TransferManager` itself never holds one — which is what keeps every
/// test in `transfers.rs` runnable without a window.
fn install_transfer_queue(app: &tauri::AppHandle) -> std::sync::Arc<transfers::TransferManager> {
    use tauri_specta::Event as _;

    let app_data = snapshot_store::app_data_dir(app).unwrap_or_else(|error| {
        tracing::warn!(%error, "no app data directory; transfers will not survive a restart");
        std::env::temp_dir().join("rdirstat-transfers")
    });
    let emitter = app.clone();
    std::sync::Arc::new(
        transfers::TransferManager::load(&app_data).with_listener(Box::new(move |job| {
            // A failed emit means the window is gone. The job carries on, and
            // its state is on disk, so there is nothing to recover here.
            drop(events::TransferProgressEvent(job.clone()).emit(&emitter));
        })),
    )
}

/// Builds the `tauri-specta` command and event registry.
///
/// Separated from [`run`] so a test can export the bindings without starting a
/// window.
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(tauri_specta::collect_commands![
            commands::scan_start,
            commands::scan_cancel,
            commands::scan_status,
            commands::scan_errors,
            commands::children,
            commands::node_details,
            commands::structure_digest,
            commands::file_digest,
            commands::size_bands,
            commands::size_band_entries,
            commands::category_totals,
            commands::category_entries,
            commands::age_buckets,
            commands::age_bucket_entries,
            commands::duplicate_candidates,
            commands::scan_diff,
            commands::snapshot_offers,
            commands::restore_snapshot,
            commands::ancestors,
            commands::path_of,
            commands::complete_path,
            commands::browse_directories,
            commands::verify_duplicates,
            commands::volumes,
            commands::trash_preview,
            commands::move_to_trash,
            commands::reveal_in_finder,
            commands::preferences,
            commands::set_theme,
            commands::check_for_updates,
            commands::sync_schedules,
            commands::save_sync_schedule,
            commands::delete_sync_schedule,
            commands::run_sync_schedule_now,
            commands::sync_diff,
            commands::sync_plan,
            commands::sync_apply,
            commands::storage_report,
            commands::set_snapshot_dir,
            commands::export_snapshot,
            commands::relocate_plan,
            commands::relocate_apply,
            commands::remote_profiles,
            commands::remote_targets,
            commands::remote_save_target,
            commands::remote_delete_target,
            commands::remote_probe,
            commands::remote_plan,
            commands::transfers,
            commands::transfer_enqueue,
            commands::transfer_pause,
            commands::transfer_resume,
            commands::transfer_cancel,
            commands::transfers_clear,
        ])
        .events(tauri_specta::collect_events![ScanProgressEvent, TransferProgressEvent])
        // Argument types for the two binary commands, so the frontend still has
        // generated types for a hand-written `invoke<ArrayBuffer>` call.
        .typ::<rdirstat_core::LayoutKind>()
        .typ::<rdirstat_core::Viewport>()
        // Which byte count the layout's AREAS encode, so the toolbar's
        // logical/allocated choice is typed on the wire rather than a string.
        .typ::<rdirstat_treemap::SizeMetric>()
        .typ::<rdirstat_core::ReportName>()
        .typ::<rdirstat_core::ReportParams>()
        .typ::<rdirstat_core::CatalogScanId>()
        .typ::<rdirstat_core::QueryError>()
        .constant("PROTOCOL_VERSION", rdirstat_core::PROTOCOL_VERSION)
        .constant("MAX_CHILD_PAGE", rdirstat_core::MAX_CHILD_PAGE)
        .constant("MAX_REPORT_ENTRIES", rdirstat_core::MAX_REPORT_ENTRIES)
        .constant("MIN_TILE_PX", rdirstat_core::MIN_TILE_PX)
        .constant("PROGRESS_MAX_HZ", rdirstat_core::PROGRESS_MAX_HZ)
        .constant("SCAN_PROGRESS_EVENT", rdirstat_core::SCAN_PROGRESS_EVENT)
        .constant("LAYOUT_COLUMNS", rdirstat_core::LAYOUT_COLUMNS)
        .constant("LAYOUT_SCHEMA_VERSION", rdirstat_core::LAYOUT_SCHEMA_VERSION)
        .constant("BINARY_COMMANDS", BINARY_COMMANDS)
        // `specta-typescript` 0.0.12 refuses to export `u64`/`i64` at all
        // unless told which trade-off to take, and every byte count, id, and
        // mtime in this contract is one. Both settings below are asserted with
        // a reason, not to silence the error:
        //
        // - **Bigints as `number`.** Tauri's IPC is JSON, so a `u64` already
        //   arrives in JavaScript as a `number`; the alternative
        //   (`enable_lossless_bigints`) generates `BigInt(x)` wrappers that do
        //   not recover precision JSON already dropped, and in rc.25 it emits
        //   `BigInt(x[0])` for a `#[serde(transparent)]` newtype like `ScanId`,
        //   which throws at runtime. The precision this gives up is real but
        //   unreachable here: 2^53 bytes is 9 PB, and `NodeId` is a `u32`.
        // - **Lossless floats.** The only floats in the contract are `Viewport`
        //   and `min_px`. `layout::build` coerces every non-finite value before
        //   using it, so `number` is true and `number | null` would push a null
        //   check onto the caller for a case that cannot occur.
        .dangerously_cast_bigints_to_number()
        .semantic_types(specta_typescript::semantic::Configuration::default().enable_lossless_floats())
}

/// Writes `src/lib/bindings.ts` from the live command signatures.
///
/// The checked-in file is generated, never hand-written: a hand-written client
/// is exactly how the IPC contract drifts.
///
/// # Errors
///
/// Whatever the TypeScript exporter reports when the file cannot be written.
pub fn export_bindings(path: &std::path::Path) -> Result<(), specta_typescript::Error> {
    use specta_typescript::Typescript;
    specta_builder().export(
        Typescript::default().header(
            "// Generated by tauri-specta from src-tauri/src/commands.rs. Do not edit.\n\
             //\n\
             // `layout` and `report` are NOT here: they answer with an Arrow IPC\n\
             // ArrayBuffer, so call them as\n\
             //   invoke<ArrayBuffer>(\"layout\", { generation, root, kind, viewport, minPx })\n\
             // and read the batch with apache-arrow's `tableFromIPC`. Their argument\n\
             // and error types below are generated and are the contract.\n",
        ),
        path,
    )
}

/// Installs the `tracing` subscriber, once.
///
/// Until this existed the app emitted no diagnostics at all: every
/// `tracing::info!`/`warn!` in this crate went to a subscriber that was never
/// registered. That is tolerable for a scan, whose outcome the user can see,
/// and not tolerable for the snapshot store, whose whole contract is "fail
/// quietly and let the app carry on" — a failure nobody can observe is
/// indistinguishable from the feature not being wired up at all.
///
/// `RUST_LOG` overrides the default. Errors are ignored deliberately: a second
/// call, or a host that has already set a global subscriber, is not a reason to
/// refuse to start.
fn install_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rdirstat=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

/// Reads the newest `*.rdstat` snapshot and publishes it, off the UI thread.
///
/// This is what makes launch cheap. A full-volume scan of a working machine is
/// millions of entries and minutes of wall clock; the snapshot is the same
/// arena as a file, so the app can open on the last tree instead of on an empty
/// volume picker.
///
/// Deliberately fire-and-forget on its own thread:
///
/// - **It must not block `setup`.** The window has to appear immediately. A
///   multi-gigabyte arena takes a moment to read, and none of it is needed to
///   draw the first frame.
/// - **Every failure is survivable.** No store, no snapshots, an unreadable
///   file, or a scan the user started first all mean the same thing — the app
///   behaves exactly as it did before snapshots existed. Nothing here is on the
///   path to a working app; it only ever removes a wait.
///
/// The frontend needs no signal: it polls `scan_status` while nothing is
/// published, and this turns that poll into `Ready` with a generation, which is
/// the same transition a finished scan produces.
/// Starts the tick that runs unattended syncs.
///
/// One minute is the resolution, not the frequency: a tick loads the settings
/// file, asks each schedule whether it is due, and almost always does nothing.
/// The shortest interval a schedule may have is fifteen minutes, so the common
/// case is a cheap read of a small JSON file once a minute.
///
/// Everything happens on a blocking thread because a sync is `ditto` and file
/// I/O, and it is deliberately serial: two schedules that come due in the same
/// minute run one after the other rather than competing for the same disk.
///
/// A missing app data directory is not a reason to refuse to start. Schedules
/// simply do not run, which is the same state as having none.
fn start_schedule_timer(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Ok(app_data) = snapshot_store::app_data_dir(&app) else {
            tracing::warn!("no app data directory; scheduled syncs are off for this run");
            return;
        };
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        // The first tick fires immediately, which would run every overdue
        // schedule during launch — competing with the snapshot restore for the
        // disk at the exact moment the user is waiting for a window.
        tick.tick().await;
        loop {
            tick.tick().await;
            let app_data = app_data.clone();
            let ran =
                tauri::async_runtime::spawn_blocking(move || schedules::run_due(&app_data, commands::now_unix_ms()))
                    .await;
            match ran {
                Ok(0) | Err(_) => {}
                Ok(count) => tracing::info!(count, "scheduled syncs finished"),
            }
        }
    });
}

fn restore_last_scan(app: tauri::AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("rdirstat-restore".to_owned())
        .spawn(move || {
            let store = match snapshot_store::SnapshotStore::new(&app) {
                Ok(store) => store,
                Err(error) => {
                    tracing::warn!(%error, "no snapshot store; the previous scan cannot be restored");
                    return;
                }
            };
            let Some(restored) = store.load_newest() else {
                // Logged, not silent: "no snapshot yet" and "the restore never
                // ran" look identical from the outside, and only one of them is
                // a bug.
                tracing::info!("no snapshot to restore; the first scan will create one");
                return;
            };

            let path = restored.path.clone();
            let bytes = restored.bytes;
            let nodes = restored.scan.tree.len();
            let root = restored.scan.root_path.clone();

            let state = tauri::Manager::state::<AppState>(&app);
            if let Some(generation) = state.publish_restored_if_idle(*restored.scan) {
                tracing::info!(
                    path = %path.display(),
                    bytes,
                    nodes,
                    root = %root.display(),
                    generation = generation.get(),
                    "restored the previous scan from a snapshot"
                );
            } else {
                // The user got there first. Their scan is live observation and
                // this is a cache; the cache loses.
                tracing::debug!("a scan was already published; discarding the restored snapshot");
            }
        });

    if let Err(error) = spawned {
        tracing::warn!(%error, "could not spawn the snapshot restore thread");
    }
}

/// Starts the desktop application and blocks until its event loop exits.
///
/// # Errors
///
/// Returns a Tauri runtime error when the application cannot initialize or the
/// event loop terminates abnormally.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    install_tracing();

    // Before anything reads `RDIRSTAT_DATA_DIR`. A development convenience
    // only: a bundled `.app` is launched with a working directory of `/` and
    // will never find a project `.env`, which is why the same setting is also
    // reachable from the storage panel.
    if let Some(path) = settings::load_dotenv() {
        tracing::info!(path = %path.display(), "loaded .env");
    }

    let builder = specta_builder();

    #[cfg(debug_assertions)]
    if let Err(error) = export_bindings(std::path::Path::new("../src/lib/bindings.ts")) {
        // A missing frontend directory must not stop the backend from running.
        eprintln!("could not export bindings: {error}");
    }

    let typed = builder.invoke_handler();
    let binary =
        tauri::generate_handler![commands::layout, commands::report] as fn(tauri::ipc::Invoke<tauri::Wry>) -> bool;

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(move |app| {
            builder.mount_events(app);
            // The transfer queue needs the app data directory, which needs the
            // handle — so it is managed here rather than beside `AppState`. A
            // queue that cannot find its directory is not a reason to refuse to
            // start: the app is a disk-usage tool first, and an in-memory queue
            // still works for the session.
            {
                use tauri::Manager as _;
                app.manage(install_transfer_queue(app.handle()));
            }
            // The menu-bar presence. A tray that cannot be created is not a
            // reason to refuse to start: the main window is the product, and
            // the tray is how you watch it without one.
            if let Err(error) = tray::build(app.handle()) {
                tracing::error!(%error, "could not create the tray icon; continuing without a menu-bar presence");
            }
            restore_last_scan(app.handle().clone());
            start_schedule_timer(app.handle().clone());
            Ok(())
        })
        // Closing the main window hides it — the app stays in the menu bar and
        // Quit stays explicit (docs/05-UI.md, "Menu bar").
        .on_window_event(tray::hide_instead_of_closing)
        .invoke_handler(move |invoke| {
            // Route by name *before* consuming the invoke: the two binary
            // commands answer with an ArrayBuffer, everything else with JSON.
            if BINARY_COMMANDS.contains(&invoke.message.command()) {
                binary(invoke)
            } else {
                typed(invoke)
            }
        })
        .run(tauri::generate_context!())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_commands_are_not_also_registered_with_specta() {
        // A command in both handlers would be dispatched twice; the router in
        // `run` relies on the two sets being disjoint.
        for name in BINARY_COMMANDS {
            assert!(
                ![
                    "scan_start",
                    "scan_cancel",
                    "scan_status",
                    "children",
                    "node_details",
                    "path_of",
                    "volumes",
                    "trash_preview",
                    "move_to_trash",
                    "reveal_in_finder",
                ]
                .contains(&name),
                "{name} is registered twice"
            );
        }
    }

    /// Regenerates the checked-in `src/lib/bindings.ts`.
    ///
    /// `#[ignore]`d because it is the one test that writes outside a
    /// `TempDir`. `cargo tauri dev` does the same thing on every debug run;
    /// this exists so the file can be refreshed without opening a window:
    ///
    /// ```text
    /// CARGO_TARGET_DIR=target/agent-4 cargo test -p rdirstat -- --ignored emit_the_checked_in_bindings
    /// ```
    #[test]
    #[ignore = "writes ../src/lib/bindings.ts; run explicitly"]
    fn emit_the_checked_in_bindings() {
        let path = std::path::Path::new("../src/lib/bindings.ts");
        export_bindings(path).expect("the command signatures must be exportable");
        assert!(path.exists());
    }

    #[test]
    fn bindings_export_to_a_temporary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bindings.ts");
        export_bindings(&path).expect("the command signatures must be exportable");
        let text = std::fs::read_to_string(&path).expect("written");
        for command in [
            "scanStart",
            "scanCancel",
            "scanStatus",
            "children",
            "nodeDetails",
            "pathOf",
            "volumes",
            "trashPreview",
            "moveToTrash",
            "revealInFinder",
        ] {
            assert!(text.contains(command), "{command} is missing from the bindings");
        }
        assert!(text.contains("scan:progress"), "the event name must be exported");
        assert!(
            text.contains("BINARY_COMMANDS"),
            "the binary command names must be exported"
        );
        assert!(text.contains("StaleGeneration") || text.contains("stale_generation"));
    }

    /// The checked-in `src/lib/bindings.ts` must match what the current
    /// command signatures generate.
    ///
    /// Without this, the file is refreshed only by a debug run of the app or
    /// by the `#[ignore]`d test above, so a command whose signature changed
    /// reaches `main` alongside a stale client that still typechecks — against
    /// the old shape. The drift is invisible until it is a runtime error.
    #[test]
    fn the_checked_in_bindings_have_not_drifted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generated_path = dir.path().join("bindings.ts");
        export_bindings(&generated_path).expect("the command signatures must be exportable");
        let generated = std::fs::read_to_string(&generated_path).expect("written");

        let committed = std::fs::read_to_string("../src/lib/bindings.ts")
            .expect("src/lib/bindings.ts is checked in and must be readable");

        if generated == committed {
            return;
        }

        // A 3,500-line `assert_eq!` is unreadable. Name the first divergence.
        let divergence = generated
            .lines()
            .zip(committed.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map_or_else(
                || {
                    format!(
                        "identical line for line, but the lengths differ: \
                         generated {}, committed {}",
                        generated.lines().count(),
                        committed.lines().count()
                    )
                },
                |(index, (generated, committed))| {
                    format!(
                        "first divergence at line {}:\n  generated: {generated}\n  committed: {committed}",
                        index + 1
                    )
                },
            );

        panic!(
            "src/lib/bindings.ts is stale: the command signatures have changed \
             since it was generated.\n{divergence}\nRegenerate with:\n    \
             cargo test -p rdirstat -- --ignored emit_the_checked_in_bindings"
        );
    }
}
