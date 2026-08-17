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
//! | [`volumes`] | the launch screen's volume list |
//! | [`engine`] | **integration seam** — the temporary home of `rdirstat-scan` |
//! | [`layout`] | **integration seam** — the temporary home of `rdirstat-treemap` |
//!
//! ## Binary responses
//!
//! `layout` and `report` return `tauri::ipc::Response` carrying an Arrow IPC
//! stream. `tauri::ipc::Response` does not implement `specta::Type`, so those
//! two commands are dispatched by a plain `tauri::generate_handler!` that
//! [`invoke_handler`] routes to by name; the other twelve go through
//! `tauri-specta`. Their argument and error types are still exported to
//! TypeScript, so the frontend calls
//! `invoke<ArrayBuffer>("layout", { generation, root, kind, viewport, minPx })`
//! with typed arguments and reads the batch with `tableFromIPC`.

mod actions;
mod commands;
mod cursor;
mod engine;
mod events;
mod fsident;
mod layout;
mod progress;
mod query;
mod state;
mod token;
mod volumes;

pub use crate::events::ScanProgressEvent;
pub use crate::state::AppState;

/// Commands that answer with raw bytes instead of JSON. Exported as a constant
/// so the generated TypeScript names them rather than leaving them folklore.
const BINARY_COMMANDS: [&str; 2] = ["layout", "report"];

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
            commands::children,
            commands::node_details,
            commands::path_of,
            commands::volumes,
            commands::trash_preview,
            commands::move_to_trash,
            commands::reveal_in_finder,
        ])
        .events(tauri_specta::collect_events![ScanProgressEvent])
        // Argument types for the two binary commands, so the frontend still has
        // generated types for a hand-written `invoke<ArrayBuffer>` call.
        .typ::<rdirstat_core::LayoutKind>()
        .typ::<rdirstat_core::Viewport>()
        .typ::<rdirstat_core::ReportName>()
        .typ::<rdirstat_core::ReportParams>()
        .typ::<rdirstat_core::CatalogScanId>()
        .typ::<rdirstat_core::QueryError>()
        .constant("PROTOCOL_VERSION", rdirstat_core::PROTOCOL_VERSION)
        .constant("MAX_CHILD_PAGE", rdirstat_core::MAX_CHILD_PAGE)
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

/// Starts the desktop application and blocks until its event loop exits.
///
/// # Errors
///
/// Returns a Tauri runtime error when the application cannot initialize or the
/// event loop terminates abnormally.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
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
        .manage(AppState::new())
        .setup(move |app| {
            builder.mount_events(app);
            Ok(())
        })
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
}
