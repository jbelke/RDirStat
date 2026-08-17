//! Relaxed counters, a best-effort current directory, and a 10 Hz publisher.
//!
//! Workers bump `Relaxed` atomics; the supervisor publishes one coherent
//! snapshot at most [`PROGRESS_MAX_HZ`] times a second. Every field is an
//! **absolute** counter plus a sequence number, so a dropped event costs
//! nothing. Completion is a state transition the supervisor returns, never
//! something inferred from a missing event
//! (docs/02-SCANNER.md#progress-backpressure-and-cancellation).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rdirstat_core::{DisplayPath, PROGRESS_MAX_HZ, ScanId, ScanProgress, ScanState};

/// Minimum wall time between two published snapshots.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

// Written as a literal because `Duration::from_millis` is const and integer
// conversion is not; the assertion is what keeps the two in step.
const _: () = assert!(
    PROGRESS_MAX_HZ == 10,
    "PROGRESS_INTERVAL must be 1000 / PROGRESS_MAX_HZ"
);

/// Shared, monotonically increasing scan counters.
///
/// `Relaxed` throughout: these are statistics, and no decision is made on them.
/// Termination is decided by the supervisor's exact pending count, which is
/// mirrored into [`Counters::pending_dirs`] for display only.
#[derive(Debug, Default)]
pub struct Counters {
    /// Entries observed, including ones aggregation would omit.
    pub observed_entries: AtomicU64,
    /// Nodes retained in the arena.
    pub retained_nodes: AtomicU64,
    /// Directories retained.
    pub directories: AtomicU64,
    /// Logical bytes accumulated, after hard-link policy.
    pub logical_bytes: AtomicU64,
    /// Allocated bytes accumulated, after hard-link policy.
    pub allocated_bytes: AtomicU64,
    /// Errors recorded, all classes.
    pub errors: AtomicU64,
    /// Entries that vanished or changed under the scan.
    pub mutations: AtomicU64,
    /// Directories queued but not yet absorbed. Mirrored from the supervisor's
    /// exact count.
    pub pending_dirs: AtomicU64,
    /// Occupancy of the bounded result channel.
    pub result_queue_depth: AtomicU64,
}

impl Counters {
    /// Zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `delta` to `counter`.
    pub fn add(counter: &AtomicU64, delta: u64) {
        counter.fetch_add(delta, Ordering::Relaxed);
    }

    /// Reads `counter`.
    #[must_use]
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// Overwrites `counter`.
    pub fn set(counter: &AtomicU64, value: u64) {
        counter.store(value, Ordering::Relaxed);
    }
}

/// Best-effort display text for the directory being read.
///
/// Written through `try_lock` and skipped under contention: a `format!` per
/// entry in a scan worker is a bug, and a progress string is never worth
/// blocking a reader.
#[derive(Debug, Default)]
pub struct CurrentDir {
    bytes: Mutex<Vec<u8>>,
}

impl CurrentDir {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the current path if the lock is free right now.
    pub fn offer(&self, path: &[u8]) {
        if let Ok(mut slot) = self.bytes.try_lock() {
            slot.clear();
            slot.extend_from_slice(path);
        }
    }

    /// The current path, escaped for display, or `None` under contention or
    /// before anything has been offered.
    #[must_use]
    pub fn snapshot(&self) -> Option<DisplayPath> {
        let slot = self.bytes.try_lock().ok()?;
        if slot.is_empty() {
            return None;
        }
        Some(DisplayPath::from_bytes(&slot))
    }
}

/// Where progress snapshots go.
///
/// `src-tauri` implements this by emitting
/// [`SCAN_PROGRESS_EVENT`](rdirstat_core::SCAN_PROGRESS_EVENT); the CLI
/// implements it by printing a line; tests implement it by pushing to a `Vec`.
pub trait ProgressSink: Send + Sync {
    /// Publishes one snapshot. Must not block: it runs on the supervisor
    /// thread, between directories.
    fn publish(&self, progress: &ScanProgress);
}

/// A sink that discards everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn publish(&self, _progress: &ScanProgress) {}
}

/// The memory picture the scan can actually measure.
///
/// **Not process RSS.** Reading true resident size on Darwin needs
/// `mach_task_basic_info`, which needs `unsafe`, and this crate has none. These
/// are the arena's own resident bytes, which is the term that dominates and the
/// only one the scanner controls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaFootprint {
    /// Bytes held by the node array, the name blob, and the directory tables.
    pub resident_bytes: u64,
    /// Projected footprint once every queued directory has been absorbed, at
    /// the observed bytes-per-directory rate.
    pub projected_peak_bytes: u64,
    /// Mean retained name length in bytes.
    pub mean_name_bytes: u32,
    /// Retained directories as a fraction of retained nodes, in basis points.
    pub directory_ratio_bp: u32,
}

impl ArenaFootprint {
    /// Bytes per arena node.
    pub const NODE_BYTES: u64 = 48;
    /// Bytes per directory row: [`DirTotals`](rdirstat_core::DirTotals) plus
    /// its [`NodeId`](rdirstat_core::NodeId).
    pub const DIRECTORY_BYTES: u64 = 60;

    /// Measures the arena and projects the remaining growth.
    #[must_use]
    pub fn measure(nodes: usize, name_capacity: usize, name_len: usize, directories: usize, pending: u64) -> Self {
        let nodes64 = u64::try_from(nodes).unwrap_or(u64::MAX);
        let dirs64 = u64::try_from(directories).unwrap_or(u64::MAX);
        let names64 = u64::try_from(name_capacity).unwrap_or(u64::MAX);
        let resident = nodes64
            .saturating_mul(Self::NODE_BYTES)
            .saturating_add(dirs64.saturating_mul(Self::DIRECTORY_BYTES))
            .saturating_add(names64);

        // Bytes per completed directory, applied to the directories still
        // queued. Linear, stated as such, and never a promise.
        let per_dir = resident.checked_div(dirs64).unwrap_or(0);
        let projected = resident.saturating_add(per_dir.saturating_mul(pending));

        let mean_name = u64::try_from(name_len)
            .unwrap_or(u64::MAX)
            .checked_div(nodes64)
            .and_then(|mean| u32::try_from(mean).ok())
            .unwrap_or(0);
        let ratio = dirs64
            .saturating_mul(10_000)
            .checked_div(nodes64)
            .and_then(|ratio| u32::try_from(ratio).ok())
            .unwrap_or(0);

        Self {
            resident_bytes: resident,
            projected_peak_bytes: projected,
            mean_name_bytes: mean_name,
            directory_ratio_bp: ratio,
        }
    }
}

/// Throttles snapshots to at most [`PROGRESS_MAX_HZ`] per second.
pub struct ProgressPublisher<'a> {
    sink: &'a dyn ProgressSink,
    scan_id: ScanId,
    started: Instant,
    last: Option<Instant>,
    sequence: u64,
}

impl core::fmt::Debug for ProgressPublisher<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProgressPublisher")
            .field("scan_id", &self.scan_id)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

impl<'a> ProgressPublisher<'a> {
    /// A publisher that has not emitted anything yet.
    #[must_use]
    pub fn new(sink: &'a dyn ProgressSink, scan_id: ScanId, started: Instant) -> Self {
        Self {
            sink,
            scan_id,
            started,
            last: None,
            sequence: 0,
        }
    }

    /// Milliseconds since the scan started.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Publishes unless the last snapshot was less than
    /// [`PROGRESS_INTERVAL`] ago.
    pub fn maybe_publish(
        &mut self,
        state: ScanState,
        counters: &Counters,
        current: &CurrentDir,
        footprint: ArenaFootprint,
    ) {
        let now = Instant::now();
        if let Some(last) = self.last
            && now.duration_since(last) < PROGRESS_INTERVAL
        {
            return;
        }
        self.last = Some(now);
        self.publish(state, counters, current, footprint);
    }

    /// Publishes unconditionally. Used for the terminal snapshot, which must
    /// not be lost to throttling.
    pub fn publish(&mut self, state: ScanState, counters: &Counters, current: &CurrentDir, footprint: ArenaFootprint) {
        self.sequence = self.sequence.saturating_add(1);
        let progress = ScanProgress {
            scan_id: self.scan_id,
            sequence: self.sequence,
            state,
            observed_entries: Counters::get(&counters.observed_entries),
            retained_nodes: Counters::get(&counters.retained_nodes),
            directories: Counters::get(&counters.directories),
            logical_bytes: Counters::get(&counters.logical_bytes),
            allocated_bytes: Counters::get(&counters.allocated_bytes),
            errors: Counters::get(&counters.errors),
            mutations: Counters::get(&counters.mutations),
            pending_dirs: Counters::get(&counters.pending_dirs),
            result_queue_depth: u32::try_from(Counters::get(&counters.result_queue_depth)).unwrap_or(u32::MAX),
            elapsed_ms: self.elapsed_ms(),
            current_dir: current.snapshot(),
            rss_bytes: footprint.resident_bytes,
            projected_peak_rss_bytes: footprint.projected_peak_bytes,
            mean_name_bytes: footprint.mean_name_bytes,
            directory_ratio_bp: footprint.directory_ratio_bp,
        };
        self.sink.publish(&progress);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[derive(Debug, Default)]
    struct Recorder {
        seen: StdMutex<Vec<ScanProgress>>,
    }

    impl ProgressSink for Recorder {
        fn publish(&self, progress: &ScanProgress) {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(progress.clone());
            }
        }
    }

    #[test]
    fn the_publisher_throttles_to_ten_hertz() {
        let recorder = Recorder::default();
        let counters = Counters::new();
        let current = CurrentDir::new();
        let mut publisher = ProgressPublisher::new(&recorder, ScanId::FIRST, Instant::now());

        for _ in 0..100 {
            publisher.maybe_publish(ScanState::Scanning, &counters, &current, ArenaFootprint::default());
        }
        let seen = recorder.seen.lock().expect("not poisoned");
        assert_eq!(seen.len(), 1, "a hundred calls in one instant is one event");
        assert_eq!(seen[0].sequence, 1);
        assert_eq!(seen[0].state, ScanState::Scanning);
    }

    #[test]
    fn sequence_numbers_are_monotonic_and_counters_are_absolute() {
        let recorder = Recorder::default();
        let counters = Counters::new();
        let current = CurrentDir::new();
        let mut publisher = ProgressPublisher::new(&recorder, ScanId::FIRST, Instant::now());

        Counters::add(&counters.observed_entries, 5);
        publisher.publish(ScanState::Scanning, &counters, &current, ArenaFootprint::default());
        Counters::add(&counters.observed_entries, 7);
        publisher.publish(ScanState::Finalizing, &counters, &current, ArenaFootprint::default());

        let seen = recorder.seen.lock().expect("not poisoned");
        assert_eq!(seen.len(), 2);
        assert_eq!((seen[0].sequence, seen[0].observed_entries), (1, 5));
        assert_eq!(
            (seen[1].sequence, seen[1].observed_entries),
            (2, 12),
            "absolute, not a delta"
        );
    }

    #[test]
    fn current_dir_is_best_effort_and_escapes_bytes() {
        let current = CurrentDir::new();
        assert_eq!(current.snapshot(), None);
        current.offer(&[b'/', 0xff]);
        let shown = current.snapshot().expect("offered");
        assert_eq!(shown.as_str(), "/%FF");
    }

    #[test]
    fn the_footprint_is_arena_bytes_and_says_so() {
        let footprint = ArenaFootprint::measure(1_000, 20_000, 18_000, 40, 10);
        assert_eq!(footprint.resident_bytes, 1_000 * 48 + 40 * 60 + 20_000);
        assert!(footprint.projected_peak_bytes > footprint.resident_bytes);
        assert_eq!(footprint.mean_name_bytes, 18);
        assert_eq!(footprint.directory_ratio_bp, 400, "40 of 1000 is 4%");
        assert_eq!(ArenaFootprint::measure(0, 0, 0, 0, 0).projected_peak_bytes, 0);
    }
}
