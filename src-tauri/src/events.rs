//! The one event this backend emits.
//!
//! `tauri_specta::Event` cannot be derived in `rdirstat-core`, because that
//! would make a crate under `crates/*` depend on `tauri` and end the ability to
//! benchmark the scanner without a webview. The derive therefore lives here, on
//! a newtype wrapper, exactly as the contract specifies.

use rdirstat_core::ScanProgress;

/// [`ScanProgress`] on the wire, under
/// [`SCAN_PROGRESS_EVENT`](rdirstat_core::SCAN_PROGRESS_EVENT).
///
/// The explicit `event_name` matters: the derive would otherwise kebab-case the
/// Rust identifier into `scan-progress-event`, and the contract pins the wire
/// name to `scan:progress`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type, tauri_specta::Event)]
#[tauri_specta(event_name = "scan:progress")]
pub struct ScanProgressEvent(pub ScanProgress);

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_specta::Event as _;

    #[test]
    fn the_event_name_matches_the_contract_constant() {
        assert_eq!(ScanProgressEvent::NAME, rdirstat_core::SCAN_PROGRESS_EVENT);
    }

    #[test]
    fn the_payload_is_the_core_progress_type_verbatim() {
        let event = ScanProgressEvent(ScanProgress {
            sequence: 3,
            ..ScanProgress::default()
        });
        let json = serde_json::to_string(&event).expect("serializes");
        assert!(json.contains(r#""sequence":3"#), "{json}");
    }
}
