//! Application state and the one-active-scan state machine.
//!
//! The published tree lives behind `RwLock<Option<Arc<CompletedScan>>>`. A
//! command takes the read lock only long enough to **clone the `Arc`** and then
//! drops it; nothing holds an application lock while computing a page, a
//! layout, or a path. That is what lets a 69M-node tree be read concurrently
//! while the next scan is already running.
//!
//! The lifecycle is `Idle -> Scanning -> Cancelling | Finalizing -> Ready |
//! Failed`, with at most one active scan. [`ScanId`] and [`TreeGeneration`] are
//! monotonic, and every read command **rejects a stale generation** rather than
//! applying an old selection to a new tree.

use std::collections::hash_map::RandomState;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};

use rdirstat_core::{
    ActionError, CancelState, CompletedScan, QueryError, ScanId, ScanProgress, ScanState, ScanStatus, StartError,
    TreeGeneration,
};

use crate::progress::ProgressCounters;

/// Cooperative cancellation for one scan.
///
/// Two flags, because docs/01 makes "cancel acknowledged" and "all I/O
/// resources closed" distinct states: no design can promise a hard deadline for
/// an uninterruptible remote-filesystem syscall.
///
/// It also *owns* the [`rdirstat_scan::CancelToken`] the scanner itself polls.
/// Wrapping rather than polling is deliberate: a bridge thread that copied one
/// flag onto the other would add latency to the one operation docs/00 gives a
/// 100 ms budget, and would be a thread per scan doing nothing else.
/// [`request`](Self::request) sets both, so there is no window in which the
/// shell believes a scan is cancelling and the scanner has not been told.
#[derive(Debug, Default)]
pub struct CancelToken {
    requested: AtomicBool,
    resources_closed: AtomicBool,
    scan: rdirstat_scan::CancelToken,
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The scanner's view of this token. Cloning shares the flag.
    #[must_use]
    pub fn scan_token(&self) -> rdirstat_scan::CancelToken {
        self.scan.clone()
    }

    /// Requests cancellation. Idempotent.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.scan.cancel();
    }

    /// Whether cancellation has been requested. Checked before each directory
    /// read, between batches, and while waiting on channels.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Marks every reader handle closed and every queued batch dropped.
    pub fn mark_resources_closed(&self) {
        self.resources_closed.store(true, Ordering::Release);
    }

    /// Whether the scan's I/O resources are known to be released.
    #[must_use]
    pub fn resources_closed(&self) -> bool {
        self.resources_closed.load(Ordering::Acquire)
    }
}

/// The scan currently occupying the single active slot.
#[derive(Debug, Clone)]
pub struct ActiveScan {
    /// Which attempt this is.
    pub scan_id: ScanId,
    /// Its cancellation token.
    pub cancel: Arc<CancelToken>,
    /// The atomics the 10 Hz progress timer reads.
    pub counters: Arc<ProgressCounters>,
}

/// Everything the state machine tracks, under one small mutex.
///
/// Deliberately separate from the published tree: taking this lock is `O(1)`
/// and never overlaps a tree read.
#[derive(Debug, Default)]
struct Lifecycle {
    state: ScanState,
    active: Option<ActiveScan>,
    generation: TreeGeneration,
    last_progress: Option<ScanProgress>,
}

/// The Tauri-managed application state.
///
/// Owned by `src-tauri` — it is the one type in the IPC signatures that
/// `rdirstat-core` does not define.
#[derive(Debug)]
pub struct AppState {
    /// The published, immutable tree. Read-locked only to clone the `Arc`.
    published: RwLock<Option<Arc<CompletedScan>>>,
    lifecycle: Mutex<Lifecycle>,
    next_scan_id: AtomicU64,
    next_generation: AtomicU64,
    /// Randomly keyed per process, so a confirmation token minted by this run
    /// cannot be replayed against another.
    token_keys: RandomState,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Recovers a poisoned lock instead of panicking.
///
/// A panic in one command must not take the whole state machine down: the data
/// behind these locks is either a plain enum or an immutable `Arc`, so there is
/// no torn invariant to protect.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(rwlock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    rwlock.read().unwrap_or_else(PoisonError::into_inner)
}

impl AppState {
    /// Fresh state: nothing published, nothing running.
    #[must_use]
    pub fn new() -> Self {
        Self {
            published: RwLock::new(None),
            lifecycle: Mutex::new(Lifecycle::default()),
            next_scan_id: AtomicU64::new(ScanId::FIRST.get()),
            next_generation: AtomicU64::new(TreeGeneration::FIRST.get()),
            token_keys: RandomState::new(),
        }
    }

    /// The keys used to sign confirmation tokens.
    #[must_use]
    pub(crate) fn token_keys(&self) -> &RandomState {
        &self.token_keys
    }

    /// Clones the published `Arc`, holding the read lock for exactly that long.
    #[must_use]
    pub fn published(&self) -> Option<Arc<CompletedScan>> {
        read(&self.published).clone()
    }

    /// The published tree, or the reason a read command cannot proceed.
    ///
    /// **A stale generation is rejected, never silently upgraded.** The
    /// frontend refetches under `current` rather than mixing two trees.
    ///
    /// # Errors
    ///
    /// [`QueryError::NoScan`] when nothing is published, or
    /// [`QueryError::StaleGeneration`] when `requested` is not the live tree.
    pub fn tree_for_query(&self, requested: TreeGeneration) -> Result<Arc<CompletedScan>, QueryError> {
        let scan = self.published().ok_or(QueryError::NoScan)?;
        if scan.generation == requested {
            Ok(scan)
        } else {
            Err(QueryError::StaleGeneration {
                requested,
                current: scan.generation,
            })
        }
    }

    /// The published tree for a Reveal or Trash action.
    ///
    /// # Errors
    ///
    /// [`ActionError::NoScan`] or [`ActionError::StaleGeneration`].
    pub fn tree_for_action(&self, requested: TreeGeneration) -> Result<Arc<CompletedScan>, ActionError> {
        let scan = self.published().ok_or(ActionError::NoScan)?;
        if scan.generation == requested {
            Ok(scan)
        } else {
            Err(ActionError::StaleGeneration {
                requested,
                current: scan.generation,
            })
        }
    }

    /// Claims the single active-scan slot.
    ///
    /// # Errors
    ///
    /// [`StartError::AlreadyScanning`] when a scan is running, cancelling, or
    /// finalizing. v1 supports exactly one.
    pub fn begin_scan(&self) -> Result<ActiveScan, StartError> {
        let mut lifecycle = lock(&self.lifecycle);
        if let Some(active) = &lifecycle.active {
            return Err(StartError::AlreadyScanning {
                scan_id: active.scan_id,
            });
        }
        let scan_id = ScanId::from_raw(self.next_scan_id.fetch_add(1, Ordering::Relaxed));
        let active = ActiveScan {
            scan_id,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        };
        lifecycle.active = Some(active.clone());
        lifecycle.state = ScanState::Scanning;
        lifecycle.last_progress = None;
        Ok(active)
    }

    /// Moves the active scan into `Finalizing` — traversal done, rollup running.
    ///
    /// [`AppState::record_progress`] does this automatically from the progress
    /// stream; this is the explicit entry point for a supervisor that reports
    /// the transition directly instead.
    pub fn mark_finalizing(&self, scan_id: ScanId) {
        let mut lifecycle = lock(&self.lifecycle);
        if lifecycle
            .active
            .as_ref()
            .is_some_and(|active| active.scan_id == scan_id)
        {
            lifecycle.state = ScanState::Finalizing;
        }
    }

    /// Mints the next [`TreeGeneration`]. Monotonic and never reused.
    #[must_use]
    pub fn next_generation(&self) -> TreeGeneration {
        TreeGeneration::from_raw(self.next_generation.fetch_add(1, Ordering::Relaxed))
    }

    /// Publishes a completed scan and releases the active slot.
    ///
    /// The swap happens after `rollup()` and `validate()`, so a reader can
    /// never observe a half-built arena.
    pub fn publish(&self, scan: Arc<CompletedScan>) {
        let generation = scan.generation;
        {
            let mut slot = self.published.write().unwrap_or_else(PoisonError::into_inner);
            *slot = Some(scan);
        }
        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.active = None;
        lifecycle.state = ScanState::Ready;
        lifecycle.generation = generation;
    }

    /// Releases the active slot without publishing.
    ///
    /// A cancelled or failed scan never becomes `Ready`; the previously
    /// published tree, if any, stays visible.
    pub fn release_unpublished(&self, failed: bool) {
        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.active = None;
        lifecycle.state = if failed {
            ScanState::Failed
        } else if lifecycle.generation.is_none() {
            ScanState::Idle
        } else {
            ScanState::Ready
        };
    }

    /// Requests cancellation of `scan_id`.
    #[must_use]
    pub fn cancel(&self, scan_id: ScanId) -> CancelState {
        let mut lifecycle = lock(&self.lifecycle);
        let Some(active) = lifecycle.active.clone() else {
            // Nothing running. Either it just finished, or the id is unknown.
            return if lifecycle.state == ScanState::Ready || lifecycle.state == ScanState::Failed {
                CancelState::AlreadyFinished
            } else {
                CancelState::NotFound
            };
        };
        if active.scan_id != scan_id {
            return CancelState::NotFound;
        }
        active.cancel.request();
        lifecycle.state = ScanState::Cancelling;
        if active.cancel.resources_closed() {
            CancelState::ResourcesClosed
        } else {
            CancelState::Acknowledged
        }
    }

    /// Records the most recent progress snapshot so a client that just
    /// connected does not wait up to 100 ms for the next event.
    pub fn record_progress(&self, progress: ScanProgress) {
        let mut lifecycle = lock(&self.lifecycle);
        // Promote `Scanning -> Finalizing` when the scan reports rollup has
        // begun, so `scan_status` is truthful without the engine reaching into
        // this lock. Deliberately a promotion only: it must never pull the
        // state back out of `Cancelling`, which `cancel()` owns.
        if lifecycle.state == ScanState::Scanning
            && progress.state == ScanState::Finalizing
            && lifecycle
                .active
                .as_ref()
                .is_some_and(|active| active.scan_id == progress.scan_id)
        {
            lifecycle.state = ScanState::Finalizing;
        }
        lifecycle.last_progress = Some(progress);
    }

    /// The `O(1)` answer to "where is the application".
    #[must_use]
    pub fn status(&self) -> ScanStatus {
        let (state, active_scan, generation, last_progress) = {
            let lifecycle = lock(&self.lifecycle);
            (
                lifecycle.state,
                lifecycle.active.as_ref().map(|active| active.scan_id),
                lifecycle.generation,
                lifecycle.last_progress.clone(),
            )
        };
        ScanStatus {
            state,
            active_scan,
            generation,
            summary: self.published().map(|scan| scan.summary()),
            last_progress,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_scan_may_hold_the_slot() {
        let state = AppState::new();
        let first = state.begin_scan().expect("first scan claims the slot");
        let second = state.begin_scan();
        assert_eq!(
            second.expect_err("this call must be rejected"),
            StartError::AlreadyScanning { scan_id: first.scan_id }
        );
        state.release_unpublished(false);
        state.begin_scan().expect("the slot is free again");
    }

    #[test]
    fn scan_ids_and_generations_are_monotonic() {
        let state = AppState::new();
        let first = state.begin_scan().expect("claims");
        state.release_unpublished(false);
        let second = state.begin_scan().expect("claims");
        assert!(second.scan_id.get() > first.scan_id.get());

        let a = state.next_generation();
        let b = state.next_generation();
        assert!(b.get() > a.get());
        assert!(!a.is_none(), "a minted generation is never NONE");
    }

    #[test]
    fn a_query_without_a_published_tree_is_no_scan() {
        let state = AppState::new();
        assert_eq!(
            state
                .tree_for_query(TreeGeneration::FIRST)
                .expect_err("this call must be rejected"),
            QueryError::NoScan
        );
        assert_eq!(
            state
                .tree_for_action(TreeGeneration::FIRST)
                .expect_err("this call must be rejected"),
            ActionError::NoScan
        );
    }

    #[test]
    fn cancelling_an_unknown_scan_is_not_found() {
        let state = AppState::new();
        assert_eq!(state.cancel(ScanId::FIRST), CancelState::NotFound);
        let active = state.begin_scan().expect("claims");
        assert_eq!(
            state.cancel(ScanId::from_raw(active.scan_id.get() + 1)),
            CancelState::NotFound
        );
        assert_eq!(state.cancel(active.scan_id), CancelState::Acknowledged);
        assert!(active.cancel.is_cancelled());
    }

    #[test]
    fn progress_promotes_scanning_to_finalizing_but_never_out_of_cancelling() {
        let state = AppState::new();
        let active = state.begin_scan().expect("claims");
        assert_eq!(state.status().state, ScanState::Scanning);

        state.record_progress(ScanProgress {
            scan_id: active.scan_id,
            state: ScanState::Finalizing,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Finalizing);

        // A progress event from a *different* scan must not move this one.
        let state = AppState::new();
        let active = state.begin_scan().expect("claims");
        state.record_progress(ScanProgress {
            scan_id: ScanId::from_raw(active.scan_id.get() + 9),
            state: ScanState::Finalizing,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Scanning);

        // Once cancelling, a late Finalizing snapshot cannot undo it.
        assert_eq!(state.cancel(active.scan_id), CancelState::Acknowledged);
        state.record_progress(ScanProgress {
            scan_id: active.scan_id,
            state: ScanState::Finalizing,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Cancelling);
    }

    #[test]
    fn a_failed_scan_does_not_publish_and_says_so() {
        let state = AppState::new();
        state.begin_scan().expect("claims");
        state.release_unpublished(true);
        let status = state.status();
        assert_eq!(status.state, ScanState::Failed);
        assert!(status.summary.is_none());
        assert!(status.active_scan.is_none());
        assert!(state.published().is_none());
    }

    #[test]
    fn the_last_progress_snapshot_is_available_without_waiting_for_an_event() {
        let state = AppState::new();
        let active = state.begin_scan().expect("claims");
        assert!(state.status().last_progress.is_none());
        state.record_progress(ScanProgress {
            scan_id: active.scan_id,
            sequence: 12,
            observed_entries: 900,
            ..ScanProgress::default()
        });
        let recorded = state.status().last_progress.expect("recorded");
        assert_eq!(recorded.sequence, 12);
        assert_eq!(recorded.observed_entries, 900);
    }

    #[test]
    fn idle_status_is_the_default_state() {
        let status = AppState::new().status();
        assert_eq!(status.state, ScanState::Idle);
        assert!(status.active_scan.is_none());
        assert!(status.summary.is_none());
        assert!(status.generation.is_none());
    }
}
