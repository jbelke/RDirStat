//! The events this backend emits.
//!
//! `tauri_specta::Event` cannot be derived in `rdirstat-core`, because that
//! would make a crate under `crates/*` depend on `tauri` and end the ability to
//! benchmark the scanner without a webview. The derive therefore lives here, on
//! a newtype wrapper, exactly as the contract specifies.

use rdirstat_core::ScanProgress;

use crate::transfers::TransferJob;

/// [`ScanProgress`] on the wire, under
/// [`SCAN_PROGRESS_EVENT`](rdirstat_core::SCAN_PROGRESS_EVENT).
///
/// The explicit `event_name` matters: the derive would otherwise kebab-case the
/// Rust identifier into `scan-progress-event`, and the contract pins the wire
/// name to `scan:progress`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type, tauri_specta::Event)]
#[tauri_specta(event_name = "scan:progress")]
pub struct ScanProgressEvent(pub ScanProgress);

/// One transfer's state, whenever it changes.
///
/// The whole job rather than a delta, because a job is small — a dozen scalars
/// and a capped failure list — and a delta would need the frontend to hold a
/// reconstruction of state the backend already has correct on disk. It is also
/// what makes a late-arriving or dropped event harmless: every event is a
/// complete answer, so there is no sequence to fall behind.
///
/// Emitted after the queue file is written, never before, so an event never
/// describes a state a crash one instruction later would lose.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type, tauri_specta::Event)]
#[tauri_specta(event_name = "transfer:progress")]
pub struct TransferProgressEvent(pub TransferJob);

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_specta::Event as _;

    #[test]
    fn the_event_name_matches_the_contract_constant() {
        assert_eq!(ScanProgressEvent::NAME, rdirstat_core::SCAN_PROGRESS_EVENT);
    }

    #[test]
    fn the_transfer_event_name_is_namespaced_like_the_scan_one() {
        assert_eq!(TransferProgressEvent::NAME, "transfer:progress");
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
