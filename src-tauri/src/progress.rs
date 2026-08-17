//! Scan progress: lock-free counters plus the 10 Hz emitter thread.
//!
//! The scan path writes **atomics only** — no `format!`, no allocation, no
//! event emission from a reader worker. A separate timer thread wakes at
//! [`PROGRESS_MAX_HZ`](rdirstat_core::PROGRESS_MAX_HZ), reads the atomics into
//! one [`ScanProgress`], and emits it with a monotonic sequence number. Because
//! every counter is absolute, a dropped event costs nothing and events cannot
//! be applied out of order.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdirstat_core::{
    DisplayPath, ErrorClass, ErrorClassCount, PROGRESS_MAX_HZ, ScanError, ScanId, ScanProgress, ScanState,
};

use crate::events::ScanProgressEvent;
use crate::state::{AppState, CancelToken};

/// Bytes of resident arena per retained node, before names.
const NODE_BYTES: u64 = 48;
/// Bytes of `DirIndex` entry per retained directory: 56 (`DirTotals`) + 4 (`NodeId`).
const DIR_ENTRY_BYTES: u64 = 60;
/// How often real process RSS is sampled, in emitter ticks. Ten ticks is one
/// second; sampling every tick would fork ten times a second for a number that
/// does not move that fast.
const RSS_SAMPLE_EVERY: u64 = 10;

/// The atomics a running scan publishes and the emitter thread reads.
#[derive(Debug)]
pub struct ProgressCounters {
    /// Entries the scanner has seen, including any aggregation omitted.
    pub observed_entries: AtomicU64,
    /// Nodes actually pushed into the arena.
    pub retained_nodes: AtomicU64,
    /// Directories whose batch the builder has consumed.
    pub directories: AtomicU64,
    /// Logical bytes accumulated, after hard-link policy.
    pub logical_bytes: AtomicU64,
    /// Allocated bytes accumulated, after hard-link policy.
    pub allocated_bytes: AtomicU64,
    /// Recorded [`ScanError`](rdirstat_core::ScanError) count, all classes.
    pub errors: AtomicU64,
    /// Entries that vanished or changed under the scan.
    pub mutations: AtomicU64,
    /// Directories queued but not yet folded in. **Exact**: termination is
    /// decided by this counter reaching zero, not by a heuristic.
    pub pending_dirs: AtomicU64,
    /// Total interned name bytes, for the mean-name-length memory profile.
    pub name_bytes: AtomicU64,
    /// Occupancy of the bounded result channel.
    pub result_queue_depth: AtomicU32,
    /// [`ScanState`] as a small code; see [`state_code`].
    state: AtomicU8,
    /// Last sampled process RSS.
    rss_bytes: AtomicU64,
    /// Best-effort text for the directory being read. Written through
    /// `try_lock` and skipped under contention — a blocking lock in the scan
    /// loop would be a bug.
    current_dir: Mutex<Option<DisplayPath>>,
    /// What the recorded failures actually were, while they are being recorded.
    errors_by_class: ErrorLog,
    started: Instant,
}

/// The per-class counts and the first few failures of a running scan.
///
/// `ScanProgress` carries one number: `errors`. A number is not an answer to
/// "what failed" — it is the question — and until a scan finishes, its detailed
/// error list belongs to the scanner and has not crossed any boundary yet. This
/// is the shell's own copy, written as failures happen and read by the
/// `scan_errors` command.
///
/// Two shapes for two jobs, both bounded:
///
/// - **Counts are an array of atomics**, one slot per [`ErrorClass`]. Exact,
///   allocation-free, and safe to write from any thread.
/// - **Samples are a `Vec` behind a mutex**, capped at [`LIVE_ERROR_SAMPLES`]
///   and never re-allocated after the cap. It is the *first* N rather than the
///   most recent: the same rule `CompletedScan::errors` follows, so the live
///   list and the final list agree about their first page instead of quietly
///   disagreeing. Once the cap is reached the lock is not even taken.
#[derive(Debug, Default)]
struct ErrorLog {
    classes: [AtomicU64; ErrorClass::ALL.len()],
    samples: Mutex<Vec<ScanError>>,
}

/// How many failures a *running* scan keeps in full.
///
/// Small on purpose. This is the "what is going wrong right now" affordance,
/// not the report: the completed scan keeps
/// [`MAX_DETAILED_ERRORS`](rdirstat_core::MAX_DETAILED_ERRORS) of them, and
/// that is what the completion alert reads.
pub(crate) const LIVE_ERROR_SAMPLES: usize = 64;

impl Default for ProgressCounters {
    fn default() -> Self {
        Self::new()
    }
}

const STATE_IDLE: u8 = 0;
const STATE_SCANNING: u8 = 1;
const STATE_CANCELLING: u8 = 2;
const STATE_FINALIZING: u8 = 3;
const STATE_READY: u8 = 4;
const STATE_FAILED: u8 = 5;

const fn state_code(state: ScanState) -> u8 {
    match state {
        ScanState::Scanning => STATE_SCANNING,
        ScanState::Cancelling => STATE_CANCELLING,
        ScanState::Finalizing => STATE_FINALIZING,
        ScanState::Ready => STATE_READY,
        ScanState::Failed => STATE_FAILED,
        _ => STATE_IDLE,
    }
}

const fn state_from_code(code: u8) -> ScanState {
    match code {
        STATE_SCANNING => ScanState::Scanning,
        STATE_CANCELLING => ScanState::Cancelling,
        STATE_FINALIZING => ScanState::Finalizing,
        STATE_READY => ScanState::Ready,
        STATE_FAILED => ScanState::Failed,
        _ => ScanState::Idle,
    }
}

impl ProgressCounters {
    /// Zeroed counters, clock started.
    #[must_use]
    pub fn new() -> Self {
        Self {
            observed_entries: AtomicU64::new(0),
            retained_nodes: AtomicU64::new(0),
            directories: AtomicU64::new(0),
            logical_bytes: AtomicU64::new(0),
            allocated_bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            mutations: AtomicU64::new(0),
            pending_dirs: AtomicU64::new(0),
            name_bytes: AtomicU64::new(0),
            result_queue_depth: AtomicU32::new(0),
            state: AtomicU8::new(STATE_SCANNING),
            rss_bytes: AtomicU64::new(0),
            current_dir: Mutex::new(None),
            errors_by_class: ErrorLog::default(),
            started: Instant::now(),
        }
    }

    /// Records one failure for the live "what are these errors" view.
    ///
    /// Called once per recorded [`ScanError`], from whichever thread recorded
    /// it. The class counter is always incremented; the full error is kept only
    /// while there is room, so a volume with a million unreadable paths costs
    /// one atomic add each after the first [`LIVE_ERROR_SAMPLES`].
    ///
    /// A poisoned samples lock is recovered rather than propagated: a panic
    /// somewhere else must not turn the error log into a second failure.
    pub fn record_error(&self, error: &ScanError) {
        let slot = error.class().index();
        if let Some(counter) = self.errors_by_class.classes.get(slot) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        let mut samples = self
            .errors_by_class
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() < LIVE_ERROR_SAMPLES {
            samples.push(error.clone());
        }
    }

    /// The live per-class counts, in [`ErrorClass::ALL`] order, omitting zeros.
    ///
    /// `operation` is `None`: the running counters are per class only. The
    /// completed scan reports the (class, operation) pairs.
    #[must_use]
    pub fn error_counts(&self) -> Vec<ErrorClassCount> {
        ErrorClass::ALL
            .iter()
            .enumerate()
            .filter_map(|(slot, class)| {
                let count = self.errors_by_class.classes.get(slot)?.load(Ordering::Relaxed);
                (count > 0).then_some(ErrorClassCount {
                    class: *class,
                    operation: None,
                    count,
                })
            })
            .collect()
    }

    /// The retained failures of the running scan, oldest first, capped at
    /// `limit` as well as at [`LIVE_ERROR_SAMPLES`].
    #[must_use]
    pub fn error_samples(&self, limit: usize) -> Vec<ScanError> {
        let samples = self
            .errors_by_class
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        samples.iter().take(limit).cloned().collect()
    }

    /// Publishes the lifecycle state the next snapshot will carry.
    pub fn set_state(&self, state: ScanState) {
        self.state.store(state_code(state), Ordering::Relaxed);
    }

    /// Best-effort "currently reading" text. Skipped when the emitter holds the
    /// lock, which is exactly the contention the docs call for.
    pub fn offer_current_dir(&self, path: &[u8]) {
        if let Ok(mut slot) = self.current_dir.try_lock() {
            *slot = Some(DisplayPath::from_bytes(path));
        }
    }

    /// As [`offer_current_dir`](Self::offer_current_dir), for a path the
    /// producer has **already** escaped.
    ///
    /// `DisplayPath::from_bytes` is not idempotent — it escapes `%` as `%25` —
    /// so re-encoding a `DisplayPath` that arrived on a `ScanProgress` snapshot
    /// would double-escape every path containing a percent sign.
    pub fn offer_current_display(&self, path: Option<DisplayPath>) {
        if let Ok(mut slot) = self.current_dir.try_lock() {
            *slot = path;
        }
    }

    /// Milliseconds since the scan started.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Bytes of scanner-owned resident state implied by the current counters.
    ///
    /// This is the memory model from docs/01: `48 B` per node, the interned
    /// name bytes, and `60 B` per directory index entry. It is the number the
    /// projection extrapolates, not a substitute for process RSS.
    #[must_use]
    pub fn arena_bytes(&self) -> u64 {
        let nodes = self.retained_nodes.load(Ordering::Relaxed);
        let dirs = self.directories.load(Ordering::Relaxed);
        nodes
            .saturating_mul(NODE_BYTES)
            .saturating_add(self.name_bytes.load(Ordering::Relaxed))
            .saturating_add(dirs.saturating_mul(DIR_ENTRY_BYTES))
    }

    /// Projected peak resident bytes at the observed growth rate.
    ///
    /// `pending_dirs` is exact and the mean fan-out is observed, so the
    /// remaining entry count is an estimate with a real basis rather than a
    /// guess. Crossing [`ScanOptions::memory_limit_bytes`] ends the scan with
    /// [`ScanError::MemoryLimit`](rdirstat_core::ScanError::MemoryLimit).
    ///
    /// [`ScanOptions::memory_limit_bytes`]: rdirstat_core::ScanOptions::memory_limit_bytes
    #[must_use]
    pub fn projected_peak_bytes(&self) -> u64 {
        let observed = self.observed_entries.load(Ordering::Relaxed);
        let directories = self.directories.load(Ordering::Relaxed).max(1);
        let pending = self.pending_dirs.load(Ordering::Relaxed);
        let mean_fan_out = observed / directories;
        let remaining = pending.saturating_mul(mean_fan_out);
        let per_entry = NODE_BYTES.saturating_add(u64::from(self.mean_name_bytes()));
        let base = self.rss_bytes.load(Ordering::Relaxed).max(self.arena_bytes());
        base.saturating_add(remaining.saturating_mul(per_entry))
    }

    /// Mean retained name length in bytes.
    #[must_use]
    pub fn mean_name_bytes(&self) -> u32 {
        let nodes = self.retained_nodes.load(Ordering::Relaxed);
        if nodes == 0 {
            return 0;
        }
        u32::try_from(self.name_bytes.load(Ordering::Relaxed) / nodes).unwrap_or(u32::MAX)
    }

    /// Retained directories as a fraction of retained nodes, in basis points.
    /// Integer arithmetic: nothing in the progress path touches a float.
    #[must_use]
    pub fn directory_ratio_bp(&self) -> u32 {
        let nodes = self.retained_nodes.load(Ordering::Relaxed);
        if nodes == 0 {
            return 0;
        }
        let dirs = self.directories.load(Ordering::Relaxed);
        u32::try_from(dirs.saturating_mul(10_000) / nodes).unwrap_or(u32::MAX)
    }

    /// One absolute snapshot.
    #[must_use]
    pub fn snapshot(&self, scan_id: ScanId, sequence: u64) -> ScanProgress {
        let current_dir = self.current_dir.try_lock().ok().and_then(|slot| slot.clone());
        ScanProgress {
            scan_id,
            sequence,
            state: state_from_code(self.state.load(Ordering::Relaxed)),
            observed_entries: self.observed_entries.load(Ordering::Relaxed),
            retained_nodes: self.retained_nodes.load(Ordering::Relaxed),
            directories: self.directories.load(Ordering::Relaxed),
            logical_bytes: self.logical_bytes.load(Ordering::Relaxed),
            allocated_bytes: self.allocated_bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            mutations: self.mutations.load(Ordering::Relaxed),
            pending_dirs: self.pending_dirs.load(Ordering::Relaxed),
            result_queue_depth: self.result_queue_depth.load(Ordering::Relaxed),
            elapsed_ms: self.elapsed_ms(),
            current_dir,
            rss_bytes: self.rss_bytes.load(Ordering::Relaxed).max(self.arena_bytes()),
            projected_peak_rss_bytes: self.projected_peak_bytes(),
            mean_name_bytes: self.mean_name_bytes(),
            directory_ratio_bp: self.directory_ratio_bp(),
        }
    }

    /// Stores a freshly sampled process RSS.
    pub fn set_rss_bytes(&self, bytes: u64) {
        self.rss_bytes.store(bytes, Ordering::Relaxed);
    }
}

/// What the emitter needs to run.
pub(crate) struct EmitterHandle {
    stop: Arc<CancelToken>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for EmitterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmitterHandle").finish_non_exhaustive()
    }
}

impl EmitterHandle {
    /// Stops the timer thread and waits for its last emission.
    pub(crate) fn stop(mut self) {
        self.stop.request();
        if let Some(join) = self.join.take() {
            drop(join.join());
        }
    }
}

/// Emits [`ScanProgressEvent`] at most [`PROGRESS_MAX_HZ`] times a second.
///
/// The thread owns nothing the scan needs, so a slow or disconnected webview
/// can never apply backpressure to the scanner. The managed [`AppState`] is
/// reached through the `AppHandle` on each tick, because `tauri::State` borrows
/// from the app and cannot cross a thread boundary.
pub(crate) fn spawn_emitter(app: tauri::AppHandle, scan_id: ScanId, counters: Arc<ProgressCounters>) -> EmitterHandle {
    let stop = Arc::new(CancelToken::new());
    let thread_stop = Arc::clone(&stop);
    let interval = Duration::from_millis(1_000 / u64::from(PROGRESS_MAX_HZ));
    let join = std::thread::Builder::new()
        .name("rdirstat-progress".to_owned())
        .spawn(move || {
            let mut sequence = 0_u64;
            loop {
                std::thread::sleep(interval);
                if sequence.is_multiple_of(RSS_SAMPLE_EVERY)
                    && let Some(rss) = sample_rss_bytes()
                {
                    counters.set_rss_bytes(rss);
                }
                sequence = sequence.saturating_add(1);
                let finished = thread_stop.is_cancelled();
                let progress = counters.snapshot(scan_id, sequence);
                tauri::Manager::state::<AppState>(&app).record_progress(progress.clone());
                emit(&app, progress);
                if finished {
                    return;
                }
            }
        });
    EmitterHandle { stop, join: join.ok() }
}

fn emit(app: &tauri::AppHandle, progress: ScanProgress) {
    use tauri_specta::Event as _;
    if let Err(error) = ScanProgressEvent(progress).emit(app) {
        tracing::debug!(%error, "dropping a progress event; the webview is gone or busy");
    }
}

/// Samples this process's resident set size.
///
/// `libc::proc_pidinfo` / `mach_task_basic_info` would need `unsafe`, which the
/// workspace denies in this crate, so this shells out to `ps` at most once a
/// second. Returns `None` rather than a wrong number if the read fails.
fn sample_rss_bytes() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("/bin/ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let kib: u64 = text.trim().parse().ok()?;
    Some(kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_absolute_and_sequenced() {
        let counters = ProgressCounters::new();
        counters.observed_entries.store(1_000, Ordering::Relaxed);
        counters.retained_nodes.store(900, Ordering::Relaxed);
        counters.directories.store(90, Ordering::Relaxed);
        counters.name_bytes.store(18_000, Ordering::Relaxed);

        let snapshot = counters.snapshot(ScanId::FIRST, 7);
        assert_eq!(snapshot.sequence, 7);
        assert_eq!(snapshot.observed_entries, 1_000);
        assert_eq!(snapshot.mean_name_bytes, 20, "18000 / 900");
        assert_eq!(snapshot.directory_ratio_bp, 1_000, "90 / 900 == 10%");
        assert_eq!(snapshot.state, ScanState::Scanning);
    }

    #[test]
    fn empty_counters_do_not_divide_by_zero() {
        let counters = ProgressCounters::new();
        let snapshot = counters.snapshot(ScanId::FIRST, 0);
        assert_eq!(snapshot.mean_name_bytes, 0);
        assert_eq!(snapshot.directory_ratio_bp, 0);
        assert_eq!(snapshot.rss_bytes, 0);
    }

    #[test]
    fn the_projection_grows_with_the_pending_queue() {
        let counters = ProgressCounters::new();
        counters.observed_entries.store(1_000, Ordering::Relaxed);
        counters.retained_nodes.store(1_000, Ordering::Relaxed);
        counters.directories.store(100, Ordering::Relaxed);
        counters.name_bytes.store(20_000, Ordering::Relaxed);
        let flat = counters.projected_peak_bytes();
        counters.pending_dirs.store(50, Ordering::Relaxed);
        assert!(
            counters.projected_peak_bytes() > flat,
            "50 pending dirs at a fan-out of 10 must project more memory than none"
        );
    }

    #[test]
    fn the_current_directory_is_offered_not_forced() {
        let counters = ProgressCounters::new();
        counters.offer_current_dir(b"/Users/nobody/Music");
        let snapshot = counters.snapshot(ScanId::FIRST, 1);
        assert_eq!(
            snapshot.current_dir.map(|path| path.as_str().to_owned()),
            Some("/Users/nobody/Music".to_owned())
        );
    }

    #[test]
    fn state_codes_round_trip() {
        for state in [
            ScanState::Idle,
            ScanState::Scanning,
            ScanState::Cancelling,
            ScanState::Finalizing,
            ScanState::Ready,
            ScanState::Failed,
        ] {
            assert_eq!(state_from_code(state_code(state)), state);
        }
    }
}
