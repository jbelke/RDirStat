//! Application state and the concurrent-scan state machine.
//!
//! Published trees live behind `RwLock<BTreeMap<TreeGeneration, Arc<...>>>`. A
//! command takes the read lock only long enough to **clone one `Arc`** and then
//! drops it; nothing holds an application lock while computing a page, a
//! layout, or a path. That is what lets a 69M-node tree be read concurrently
//! while the next scan is already running.
//!
//! Each scan runs `Scanning -> Cancelling | Finalizing -> Ready | Failed`
//! independently. [`ScanId`] and [`TreeGeneration`] are monotonic, and every
//! read command quotes a generation, so a stale one is **rejected** rather than
//! applied to a tree the user never selected.
//!
//! ## Why this was singular, and what changed
//!
//! It held exactly one scan and one tree, and `begin_scan` refused a second
//! with [`StartError::AlreadyScanning`]. That was a real constraint, not an
//! oversight: memory is the binding one. A [`Node`](rdirstat_core::Node) is 48
//! bytes because 69M x 48 is 3.08 GiB against a 5.0 GiB peak-RSS gate, so two
//! large scans at once can exceed the whole budget the architecture is sized
//! for.
//!
//! Lifting it therefore did not mean deleting the check. It meant replacing a
//! *rule* with an *accounting*: [`ADMISSION`] decides when a scan may start,
//! and a scan that cannot start yet **waits** rather than failing.
//!
//! ## The two reasons a scan waits
//!
//! 1. **Another scan is already reading that filesystem.** Two scans on one
//!    device contend for the same head or the same queue and finish *later*
//!    than running them in sequence, so the second is held until the first
//!    ends. Scans on different devices run genuinely in parallel, which is the
//!    case worth having.
//! 2. **The memory budget has no headroom.** The sum of the running scans'
//!    projected peaks plus the trees already retained is within
//!    [`MIN_HEADROOM_BYTES`] of [`SCAN_MEMORY_BUDGET_BYTES`].
//!
//! A running scan is **never** cancelled to admit a new one. Work already paid
//! for outranks work merely asked for, and a user who watched a scan reach 90%
//! and then saw it killed by their own next click would be right to be angry.
//!
//! ## Retained trees are a cache, not the record
//!
//! Every completed scan is also written to the snapshot store, so evicting one
//! from memory loses nothing that `restore_snapshot` cannot bring back. That is
//! what makes [`MAX_RETAINED_TREES`] safe to enforce: past it, the
//! least-recently-read tree is dropped, because five 69M-node trees resident at
//! once is 15 GiB and no budget survives that.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};

use rdirstat_core::{
    ActionError, CancelState, CompletedScan, DisplayPath, NodeId, QueryError, ReadyScanRow, RunningScanRow, ScanId,
    ScanOptions, ScanProgress, ScanState, ScanStatus, StartError, TreeGeneration, WaitReason,
};
use rdirstat_treemap::{CategorySet, DominantCategories, FilteredWeights, SizeMetric};

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
pub(crate) struct CancelToken {
    requested: AtomicBool,
    resources_closed: AtomicBool,
    scan: rdirstat_scan::CancelToken,
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The scanner's view of this token. Cloning shares the flag.
    #[must_use]
    pub(crate) fn scan_token(&self) -> rdirstat_scan::CancelToken {
        self.scan.clone()
    }

    /// Requests cancellation. Idempotent.
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.scan.cancel();
    }

    /// Whether cancellation has been requested. Checked before each directory
    /// read, between batches, and while waiting on channels.
    #[must_use]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Marks every reader handle closed and every queued batch dropped.
    pub(crate) fn mark_resources_closed(&self) {
        self.resources_closed.store(true, Ordering::Release);
    }

    /// Whether the scan's I/O resources are known to be released.
    #[must_use]
    pub(crate) fn resources_closed(&self) -> bool {
        self.resources_closed.load(Ordering::Acquire)
    }
}

/// The memory all live scans and retained trees share.
///
/// 5.0 GiB, the peak-RSS gate the architecture is sized against
/// (docs/01-ARCHITECTURE.md). It is a *shared* budget rather than a per-scan
/// one, and that is the whole point: the moment scans became concurrent, a
/// per-scan limit stopped being a ceiling on anything the process actually
/// does.
pub(crate) const SCAN_MEMORY_BUDGET_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Room a new scan must find before it is admitted.
///
/// A scan that has not started has no measured cost, so admission cannot be
/// exact. Requiring headroom is what converts that ignorance into a safe
/// decision instead of an optimistic one.
pub(crate) const MIN_HEADROOM_BYTES: u64 = 768 * 1024 * 1024;

/// What a scan is charged before it has reported anything.
///
/// Not a prediction — a placeholder that stops two simultaneous starts from
/// both reading the budget as empty and both being admitted. It is replaced by
/// the scanner's own projection as soon as the first progress event lands, at
/// most 100 ms later.
const ASSUMED_NEW_SCAN_BYTES: u64 = 512 * 1024 * 1024;

/// How many completed trees stay resident.
///
/// Four, because five 69M-node trees is 15 GiB and the budget above is 5. Past
/// this the least-recently-read tree is evicted; it is still in the snapshot
/// store, so `restore_snapshot` brings it back. See the module docs.
pub(crate) const MAX_RETAINED_TREES: usize = 4;

/// Ceiling on scans waiting to start.
///
/// A queue nobody can read is a queue nobody wanted. This is high enough that
/// no realistic click-through reaches it and low enough that a stuck loop
/// cannot grow it without bound.
pub(crate) const MAX_WAITING_SCANS: usize = 32;

/// One scan that has been admitted and is running.
#[derive(Debug, Clone)]
pub(crate) struct ActiveScan {
    /// Which attempt this is.
    pub(crate) scan_id: ScanId,
    /// The generation this scan will publish under. Assigned at admission
    /// rather than at completion, so a progress event can name the tree it is
    /// building before that tree exists.
    pub(crate) generation: TreeGeneration,
    /// The root, for display in a list of several running scans.
    pub(crate) root: DisplayPath,
    /// `st_dev` of the root: which filesystem this scan is reading.
    ///
    /// The key the per-device queue is grouped by. It identifies a mounted
    /// *filesystem*, which is not quite the physical disk — two APFS volumes in
    /// one container have different `st_dev` and share one SSD, so two scans
    /// across them are admitted together. That is the right call anyway: an SSD
    /// serves concurrent readers well, and the case this protects against is a
    /// spinning disk or a network mount, which are separate filesystems by
    /// construction. `volumes.rs` can resolve the true physical device if this
    /// ever proves too coarse, at the cost of a `diskutil` fork per admission.
    pub(crate) device: u64,
    /// Its cancellation token.
    pub(crate) cancel: Arc<CancelToken>,
    /// The atomics the 10 Hz progress timer reads.
    pub(crate) counters: Arc<ProgressCounters>,
}

/// Everything the state machine tracks, under one small mutex.
///
/// Deliberately separate from the published tree: taking this lock is `O(1)`
/// and never overlaps a tree read.
#[derive(Debug, Default)]
struct Lifecycle {
    /// Admitted and running, newest last. A `BTreeMap` so the order a user sees
    /// is the order they started them, which is the only order that makes
    /// sense in a list they are watching.
    running: BTreeMap<ScanId, RunningScan>,
    /// Admitted in principle, waiting for a device or for memory. Drained in
    /// arrival order — a scan that has been waiting longest goes first, so a
    /// user who queues five folders gets them in the order they clicked.
    waiting: VecDeque<WaitingScan>,
    /// Scans that ended without a tree, kept so the user is told.
    ///
    /// A failure must not vanish just because the scan that produced it is no
    /// longer running. Under the single-scan machine this was implicit — the
    /// lifecycle simply sat in `Failed` until something else started — and
    /// deriving the state from live scans would have quietly dropped it.
    ///
    /// Cleared when a new scan is accepted, which is exactly when the old
    /// machine left `Failed` too. Cancelled scans are *not* recorded here: the
    /// user asked for that and does not need to be told it happened.
    failed: VecDeque<RunningScanRow>,
    /// The most recently published generation. Retained for the scalar half of
    /// [`ScanStatus`], which the existing single-scan UI still reads.
    generation: TreeGeneration,
}

/// How many failures are remembered. Small: this is a notice, not a log.
const MAX_REMEMBERED_FAILURES: usize = 8;

/// A scan that is running, plus what the state machine knows about it.
#[derive(Debug)]
struct RunningScan {
    active: ActiveScan,
    state: ScanState,
    last_progress: Option<ScanProgress>,
}

impl RunningScan {
    /// The memory this scan is expected to reach.
    ///
    /// From the scanner's own projection once progress has arrived. Before the
    /// first event there is nothing to go on, so it is charged
    /// [`ASSUMED_NEW_SCAN_BYTES`] — a scan whose cost is unknown must not be
    /// accounted as free, or two of them race each other into the budget in the
    /// window before either reports.
    fn projected_bytes(&self) -> u64 {
        self.last_progress
            .as_ref()
            .map(|progress| progress.projected_peak_rss_bytes.max(progress.rss_bytes))
            .filter(|bytes| *bytes > 0)
            .unwrap_or(ASSUMED_NEW_SCAN_BYTES)
    }
}

/// What [`AppState::accept`] decided.
///
/// Both outcomes are success. A scan that must wait has an id, appears in
/// [`AppState::status`], and can be cancelled — the caller simply does not
/// spawn a thread for it yet.
#[derive(Debug)]
pub(crate) enum Admission {
    /// Start it now.
    Start(Box<WaitingScan>),
    /// Hold it. [`AppState::take_admissible`] returns it once the reason clears.
    Wait { scan_id: ScanId, reason: WaitReason },
}

impl Admission {
    /// The scan, asserting it started.
    ///
    /// Panics rather than returning an `Option` because every use is an
    /// assertion about admission, and an `expect` on a `None` would lose the
    /// reason the scan was held — which is the only interesting part of the
    /// failure.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn started(self, what: &str) -> WaitingScan {
        match self {
            Self::Start(pending) => *pending,
            Self::Wait { reason, .. } => {
                panic!("{what} should have started, but is waiting: {reason:?}")
            }
        }
    }
}

/// A scan that has an id but has not been allowed to start yet.
///
/// It carries the request verbatim because `state.rs` decides *when* a scan
/// runs and `commands.rs` decides *how*: this struct is opaque here and is
/// handed back untouched when its turn comes.
#[derive(Debug)]
pub(crate) struct WaitingScan {
    pub(crate) scan_id: ScanId,
    pub(crate) generation: TreeGeneration,
    pub(crate) root: PathBuf,
    pub(crate) root_display: DisplayPath,
    pub(crate) device: u64,
    pub(crate) options: ScanOptions,
    pub(crate) cancel: Arc<CancelToken>,
    pub(crate) counters: Arc<ProgressCounters>,
}

/// The Tauri-managed application state.
///
/// Owned by `src-tauri` — it is the one type in the IPC signatures that
/// `rdirstat-core` does not define.
#[derive(Debug)]
pub struct AppState {
    /// The published, immutable trees, keyed by generation. Read-locked only
    /// long enough to clone one `Arc`.
    ///
    /// A map rather than a slot because scans are concurrent and each produces
    /// its own tree. Bounded by [`MAX_RETAINED_TREES`]; see the module docs for
    /// why evicting one is safe.
    published: RwLock<BTreeMap<TreeGeneration, Arc<CompletedScan>>>,
    /// Generations in least-recently-read order, oldest first. Drives eviction,
    /// so the tree the user is actually looking at is the last to go even if it
    /// was the first to be scanned.
    read_order: Mutex<Vec<TreeGeneration>>,
    lifecycle: Mutex<Lifecycle>,
    next_scan_id: AtomicU64,
    next_generation: AtomicU64,
    /// Randomly keyed per process, so a confirmation token minted by this run
    /// cannot be replayed against another.
    token_keys: RandomState,
    /// The last filtered-layout weights, so a window resize does not recompute
    /// them on every drag step. See [`filter_weights`](Self::filter_weights).
    filter_cache: Mutex<Option<FilterCache>>,
    /// The last resolved directory colours, memoised for the same reason and on
    /// the same terms. See [`dominant_categories`](Self::dominant_categories).
    dominant_cache: Mutex<Option<DominantCache>>,
}

/// One memoised set of directory colours.
///
/// Same single-slot reasoning as [`FilterCache`]: one view is on screen at a
/// time, and each entry is a byte plus a `u64` per directory.
#[derive(Debug)]
struct DominantCache {
    generation: TreeGeneration,
    root: NodeId,
    metric: SizeMetric,
    colours: Arc<DominantCategories>,
}

/// One memoised set of filtered layout weights.
///
/// Deliberately a single entry rather than a map. The user filters one view at a
/// time, so a second slot would almost never be hit and every slot holds a
/// `Vec<u64>` the length of the directory index — ~10 MB at 1.2M directories.
/// Caching aggressively here would trade a real memory budget for a hit rate
/// nobody would notice.
#[derive(Debug)]
struct FilterCache {
    generation: TreeGeneration,
    root: NodeId,
    metric: SizeMetric,
    weights: Arc<FilteredWeights>,
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
    pub(crate) fn new() -> Self {
        Self {
            published: RwLock::new(BTreeMap::new()),
            read_order: Mutex::new(Vec::new()),
            lifecycle: Mutex::new(Lifecycle::default()),
            next_scan_id: AtomicU64::new(ScanId::FIRST.get()),
            next_generation: AtomicU64::new(TreeGeneration::FIRST.get()),
            token_keys: RandomState::new(),
            filter_cache: Mutex::new(None),
            dominant_cache: Mutex::new(None),
        }
    }

    /// Filtered layout weights for `(generation, root, metric, set)`, computed
    /// once and reused.
    ///
    /// The weights do **not** depend on the viewport, which is the whole point:
    /// a window resize issues a layout request per drag step, and recomputing an
    /// `O(subtree)` pass each time costs 163 ms per step on a 12M-node tree
    /// against 0.35 ms for the unfiltered path. Sharing one set across a drag
    /// makes a filtered resize cost what an unfiltered one does.
    ///
    /// Every field of the key is load-bearing. A stale `generation` would answer
    /// from a tree that no longer exists; a stale `root` or `metric` or `set`
    /// would produce a layout that disagrees with its own legend. `set` is
    /// checked against the stored weights rather than tracked separately, so the
    /// two cannot drift.
    pub(crate) fn filter_weights(
        &self,
        scan: &CompletedScan,
        root: NodeId,
        metric: SizeMetric,
        set: CategorySet,
    ) -> Arc<FilteredWeights> {
        let mut slot = lock(&self.filter_cache);
        if let Some(entry) = slot.as_ref()
            && entry.generation == scan.generation
            && entry.root == root
            && entry.metric == metric
            && entry.weights.set() == set
        {
            return Arc::clone(&entry.weights);
        }

        // Timed here rather than inside `layout_tiles`, because that is where the
        // cost actually lives now. Leaving it to the layout timer would report
        // sub-millisecond figures for every request while the expensive pass ran
        // just outside the measured window — an instrument that stops measuring
        // the thing it was added for is worse than no instrument.
        let started = std::time::Instant::now();
        let weights = Arc::new(FilteredWeights::build(&scan.tree, root, metric, set));
        tracing::debug!(
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            nodes = scan.tree.len(),
            directories = scan.tree.directory_count(),
            "built filtered layout weights"
        );
        *slot = Some(FilterCache {
            generation: scan.generation,
            root,
            metric,
            weights: Arc::clone(&weights),
        });
        weights
    }

    /// The category a directory borrows from the heaviest thing inside it,
    /// memoised per `(generation, root, metric)`.
    ///
    /// A directory has no content category, so without this every tile an
    /// icicle or a sunburst draws is uncategorized — those two are bounded by
    /// depth rather than by area, and on a deep disk they never reach a file.
    /// The pass is `O(subtree)`, and the viewport is not part of its key, so a
    /// window resize computes it once and then reuses it exactly like the
    /// filtered weights beside it.
    pub(crate) fn dominant_categories(
        &self,
        scan: &CompletedScan,
        root: NodeId,
        metric: SizeMetric,
    ) -> Arc<DominantCategories> {
        let mut slot = lock(&self.dominant_cache);
        if let Some(entry) = slot.as_ref()
            && entry.generation == scan.generation
            && entry.root == root
            && entry.metric == metric
        {
            return Arc::clone(&entry.colours);
        }

        let started = std::time::Instant::now();
        let colours = Arc::new(DominantCategories::build(&scan.tree, root, metric));
        tracing::debug!(
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            nodes = scan.tree.len(),
            directories = scan.tree.directory_count(),
            "resolved directory colours"
        );
        *slot = Some(DominantCache {
            generation: scan.generation,
            root,
            metric,
            colours: Arc::clone(&colours),
        });
        colours
    }

    /// The keys used to sign confirmation tokens.
    #[must_use]
    pub(crate) fn token_keys(&self) -> &RandomState {
        &self.token_keys
    }

    /// The most recently published tree, if any.
    ///
    /// "Most recent" and not "the current one": with concurrent scans there is
    /// no single current tree, and which one the *user* is looking at is the
    /// frontend's business. Everything that needs a specific tree quotes a
    /// generation and goes through [`tree_for_query`](Self::tree_for_query).
    #[must_use]
    pub fn published(&self) -> Option<Arc<CompletedScan>> {
        read(&self.published).values().next_back().map(Arc::clone)
    }

    /// Every resident tree, oldest generation first.
    #[must_use]
    pub fn published_all(&self) -> Vec<Arc<CompletedScan>> {
        read(&self.published).values().map(Arc::clone).collect()
    }

    /// Bytes the resident trees are holding, for the admission budget.
    fn retained_bytes(&self) -> u64 {
        read(&self.published)
            .values()
            .map(|scan| scan.tree.retained_bytes())
            .fold(0_u64, u64::saturating_add)
    }

    /// Notes that `generation` was just read, for eviction ordering.
    ///
    /// Called on every successful tree lookup. Keeping this out of the
    /// `published` lock is deliberate: a read must never contend with another
    /// read, and the ordering is advisory — getting it slightly wrong evicts a
    /// slightly less useful tree, which is recoverable, whereas serialising
    /// every page query behind one mutex is not.
    fn touch(&self, generation: TreeGeneration) {
        let mut order = lock(&self.read_order);
        order.retain(|entry| *entry != generation);
        order.push(generation);
    }

    /// The newest running scan's counters, if one is running.
    ///
    /// `scan_errors` answers from these while a scan is active and from the
    /// published tree once it is not, which is the only way a user can ask
    /// "what are those 334 errors" *while the number is still climbing*.
    ///
    /// Newest rather than "the" scan, because there may be several. A caller
    /// that means a *specific* scan uses
    /// [`counters_for`](Self::counters_for) — this exists for the surfaces
    /// that legitimately mean "whatever is happening now".
    #[must_use]
    pub fn active_counters(&self) -> Option<Arc<ProgressCounters>> {
        lock(&self.lifecycle)
            .running
            .values()
            .next_back()
            .map(|running| Arc::clone(&running.active.counters))
    }

    /// One scan's counters, by id.
    #[must_use]
    pub fn counters_for(&self, scan_id: ScanId) -> Option<Arc<ProgressCounters>> {
        lock(&self.lifecycle)
            .running
            .get(&scan_id)
            .map(|running| Arc::clone(&running.active.counters))
    }

    /// The requested tree, or the reason a read command cannot proceed.
    ///
    /// A lookup rather than a comparison, which is the single change that made
    /// concurrent scans possible without touching one command signature: every
    /// read command already quotes the generation it means, so once several
    /// trees are resident there is nothing left to disambiguate.
    ///
    /// **A generation this process no longer holds is still rejected.** It is
    /// reported against the newest tree, so the frontend's existing
    /// stale-generation path — refetch under `current` — keeps working
    /// unchanged for an evicted tree as well as a superseded one.
    ///
    /// # Errors
    ///
    /// [`QueryError::NoScan`] when nothing is published, or
    /// [`QueryError::StaleGeneration`] when that generation is not resident.
    pub fn tree_for_query(&self, requested: TreeGeneration) -> Result<Arc<CompletedScan>, QueryError> {
        let found = read(&self.published).get(&requested).map(Arc::clone);
        if let Some(scan) = found {
            self.touch(requested);
            return Ok(scan);
        }
        let newest = self.published().ok_or(QueryError::NoScan)?;
        Err(QueryError::StaleGeneration {
            requested,
            current: newest.generation,
        })
    }

    /// The requested tree for a Reveal or Trash action.
    ///
    /// # Errors
    ///
    /// [`ActionError::NoScan`] or [`ActionError::StaleGeneration`].
    pub fn tree_for_action(&self, requested: TreeGeneration) -> Result<Arc<CompletedScan>, ActionError> {
        let found = read(&self.published).get(&requested).map(Arc::clone);
        if let Some(scan) = found {
            self.touch(requested);
            return Ok(scan);
        }
        let newest = self.published().ok_or(ActionError::NoScan)?;
        Err(ActionError::StaleGeneration {
            requested,
            current: newest.generation,
        })
    }

    /// Accepts a scan, starting it if there is room.
    ///
    /// Replaces the old `begin_scan`, which refused outright when a scan was
    /// already running. The refusal is gone; the *reasoning behind* it is not
    /// — see [`admissible`](Self::admissible).
    ///
    /// # Errors
    ///
    /// [`StartError::AlreadyScanning`] only when the wait queue is full, which
    /// is a runaway caller rather than a busy machine. The error carries the
    /// scan it is queued behind so the message can say something true.
    pub(crate) fn accept(
        &self,
        root: PathBuf,
        root_display: DisplayPath,
        device: u64,
        options: ScanOptions,
    ) -> Result<Admission, StartError> {
        let retained = self.retained_bytes();
        let mut lifecycle = lock(&self.lifecycle);

        if lifecycle.waiting.len() >= MAX_WAITING_SCANS {
            let blocking = lifecycle
                .running
                .keys()
                .next_back()
                .copied()
                .or_else(|| lifecycle.waiting.front().map(|scan| scan.scan_id))
                .unwrap_or(ScanId::FIRST);
            return Err(StartError::AlreadyScanning { scan_id: blocking });
        }

        // A new scan clears the last one's failure notice, which is where the
        // single-scan machine also left `Failed` behind.
        lifecycle.failed.clear();

        let scan_id = ScanId::from_raw(self.next_scan_id.fetch_add(1, Ordering::Relaxed));
        let generation = TreeGeneration::from_raw(self.next_generation.fetch_add(1, Ordering::Relaxed));
        let pending = WaitingScan {
            scan_id,
            generation,
            root,
            root_display,
            device,
            options,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        };

        if let Some(reason) = Self::admissible(&lifecycle, device, retained) {
            lifecycle.waiting.push_back(pending);
            return Ok(Admission::Wait { scan_id, reason });
        }
        drop(Self::promote(&mut lifecycle, &pending));
        Ok(Admission::Start(Box::new(pending)))
    }

    /// Why a scan on `device` cannot start yet, or `None` if it can.
    ///
    /// The device check comes first deliberately. It is exact and free, while
    /// the memory check rests on a projection — so when both would block, the
    /// user is told the reason that is certainly true.
    fn admissible(lifecycle: &Lifecycle, device: u64, retained: u64) -> Option<WaitReason> {
        if let Some(busy) = lifecycle
            .running
            .values()
            .find(|running| running.active.device == device)
        {
            return Some(WaitReason::SameDevice {
                scan_id: busy.active.scan_id,
            });
        }

        let live = lifecycle
            .running
            .values()
            .map(RunningScan::projected_bytes)
            .fold(0_u64, u64::saturating_add);
        let committed = live.saturating_add(retained);
        // Strictly less than, so a budget exactly consumed still refuses. The
        // first scan is always admitted regardless: refusing it would leave the
        // app unable to do the one thing it is for, and a lone scan that
        // exceeds the gate is the pre-existing single-scan case that
        // `ScanError::MemoryLimit` already handles.
        if !lifecycle.running.is_empty() && committed.saturating_add(MIN_HEADROOM_BYTES) >= SCAN_MEMORY_BUDGET_BYTES {
            return Some(WaitReason::Memory);
        }
        None
    }

    /// Moves a pending scan into the running set.
    fn promote(lifecycle: &mut Lifecycle, pending: &WaitingScan) -> ActiveScan {
        let active = ActiveScan {
            scan_id: pending.scan_id,
            generation: pending.generation,
            root: pending.root_display.clone(),
            device: pending.device,
            cancel: Arc::clone(&pending.cancel),
            counters: Arc::clone(&pending.counters),
        };
        lifecycle.running.insert(
            pending.scan_id,
            RunningScan {
                active: active.clone(),
                state: ScanState::Scanning,
                last_progress: None,
            },
        );
        active
    }

    /// Every waiting scan that can now start, removed from the queue.
    ///
    /// Called after a scan ends. Returns a `Vec` rather than one scan because
    /// a single completion can release several — one big scan finishing can
    /// free enough budget for two small ones — and the caller spawning them in
    /// a loop is simpler than being told to call again until it returns `None`.
    ///
    /// Arrival order, so the user gets them in the order they clicked.
    #[must_use]
    pub(crate) fn take_admissible(&self) -> Vec<WaitingScan> {
        let retained = self.retained_bytes();
        let mut lifecycle = lock(&self.lifecycle);
        let mut started = Vec::new();

        // Re-checked from scratch on each pass, because promoting one scan
        // changes both the device set and the projected total for the next.
        let mut index = 0;
        while index < lifecycle.waiting.len() {
            let device = lifecycle.waiting[index].device;
            if Self::admissible(&lifecycle, device, retained).is_some() {
                index += 1;
                continue;
            }
            let Some(pending) = lifecycle.waiting.remove(index) else {
                break;
            };
            drop(Self::promote(&mut lifecycle, &pending));
            started.push(pending);
            // Deliberately not advancing `index`: `remove` shifted the queue
            // down, so the next candidate is already at this position.
        }
        started
    }

    /// Moves one scan into `Finalizing` — traversal done, rollup running.
    ///
    /// [`AppState::record_progress`] does this automatically from the progress
    /// stream; this is the explicit entry point for a supervisor that reports
    /// the transition directly instead.
    pub fn mark_finalizing(&self, scan_id: ScanId) {
        let mut lifecycle = lock(&self.lifecycle);
        if let Some(running) = lifecycle.running.get_mut(&scan_id) {
            running.state = ScanState::Finalizing;
        }
    }

    /// Mints the next [`TreeGeneration`]. Monotonic and never reused.
    #[must_use]
    pub fn next_generation(&self) -> TreeGeneration {
        TreeGeneration::from_raw(self.next_generation.fetch_add(1, Ordering::Relaxed))
    }

    /// Publishes a completed scan and retires its running slot.
    ///
    /// The insert happens after `rollup()` and `validate()`, so a reader can
    /// never observe a half-built arena. It **adds** rather than replaces:
    /// concurrent scans each keep their own tree, and the user chooses which to
    /// look at.
    pub fn publish(&self, scan: Arc<CompletedScan>) {
        let generation = scan.generation;
        let scan_id = scan.scan_id;
        {
            let mut trees = self.published.write().unwrap_or_else(PoisonError::into_inner);
            trees.insert(generation, scan);
        }
        {
            let mut order = lock(&self.read_order);
            order.retain(|entry| *entry != generation);
            order.push(generation);
        }
        {
            let mut lifecycle = lock(&self.lifecycle);
            lifecycle.running.remove(&scan_id);
            lifecycle.generation = generation;
        }
        self.evict_beyond_limit();
    }

    /// Retires a scan that ended without a tree — cancelled, or failed.
    ///
    /// Separate from [`publish`](Self::publish) because there is nothing to
    /// publish, and because forgetting to call it would leave a dead scan
    /// holding a device and a slice of the memory budget forever.
    pub(crate) fn retire(&self, scan_id: ScanId) {
        lock(&self.lifecycle).running.remove(&scan_id);
    }

    /// Drops least-recently-read trees past [`MAX_RETAINED_TREES`].
    ///
    /// Safe because every published tree is also in the snapshot store, so this
    /// evicts a cache rather than losing a result. The tree the user is looking
    /// at is the most recently read, so it is the last candidate — which is the
    /// property that makes eviction invisible.
    fn evict_beyond_limit(&self) {
        let mut order = lock(&self.read_order);
        let mut trees = self.published.write().unwrap_or_else(PoisonError::into_inner);
        while trees.len() > MAX_RETAINED_TREES {
            let Some(oldest) = order.first().copied() else {
                break;
            };
            order.remove(0);
            if trees.remove(&oldest).is_some() {
                tracing::info!(
                    generation = oldest.get(),
                    resident = trees.len(),
                    "evicted the least-recently-read tree; it is still in the snapshot store"
                );
            }
        }
    }

    /// Publishes a tree restored from a `*.rdstat` snapshot.
    ///
    /// Returns the generation it was published under, or `None` if it was not
    /// published at all.
    ///
    /// Two things make this different from [`publish`](Self::publish), and both
    /// are why it is a separate method rather than a flag:
    ///
    /// 1. **It never clobbers.** A restore is started at launch and finishes
    ///    whenever the file finishes reading. If the user was quick enough to
    ///    start a real scan in the meantime — or one already published — the
    ///    restore is dropped. Live observation always outranks a cache.
    /// 2. **The ids are re-stamped.** [`ScanId`] and [`TreeGeneration`] are
    ///    per-process counters. The ones in the file came from a previous run,
    ///    and republishing them would let a stale generation collide with a
    ///    live one, which is precisely the confusion the generation check in
    ///    [`tree_for_query`](Self::tree_for_query) exists to prevent.
    ///
    /// The active slot is not claimed: nothing is scanning, so there is nothing
    /// to cancel and no progress to emit.
    pub fn publish_restored_if_idle(&self, scan: CompletedScan) -> Option<TreeGeneration> {
        self.publish_restored_inner(scan, false)
    }

    /// Publishes a restored tree at the user's explicit request, replacing
    /// whatever is on screen.
    ///
    /// The difference from [`publish_restored_if_idle`](Self::publish_restored_if_idle)
    /// is *who asked*. The launch path must never clobber, because the user may
    /// have started a real scan while the file was still being read and live
    /// observation outranks a cache. Switching drives is the opposite: the
    /// current tree is precisely what the user asked to be rid of, so refusing
    /// because one exists would make the command do nothing.
    ///
    /// A running scan still wins. Replacing the published tree out from under a
    /// scan that is about to publish its own would leave the app showing one
    /// volume and finalizing another.
    pub fn publish_restored(&self, scan: CompletedScan) -> Option<TreeGeneration> {
        self.publish_restored_inner(scan, true)
    }

    fn publish_restored_inner(&self, mut scan: CompletedScan, replace: bool) -> Option<TreeGeneration> {
        let generation = {
            let mut lifecycle = lock(&self.lifecycle);
            // "Any scan running" still blocks a restore, even though scans are
            // now plural. A restore is a cache being warmed at launch; a
            // running scan is the user watching something happen, and a tree
            // appearing underneath that is confusing however many trees the app
            // can now hold.
            if !lifecycle.running.is_empty() || (!replace && !lifecycle.generation.is_none()) {
                return None;
            }
            let generation = TreeGeneration::from_raw(self.next_generation.fetch_add(1, Ordering::Relaxed));
            scan.scan_id = ScanId::from_raw(self.next_scan_id.fetch_add(1, Ordering::Relaxed));
            scan.generation = generation;

            // Written while the lifecycle lock is held so a scan cannot start
            // between the check above and the insert below.
            let mut trees = self.published.write().unwrap_or_else(PoisonError::into_inner);
            trees.insert(generation, Arc::new(scan));
            lifecycle.generation = generation;
            generation
        };
        {
            let mut order = lock(&self.read_order);
            order.retain(|entry| *entry != generation);
            order.push(generation);
        }
        self.evict_beyond_limit();
        Some(generation)
    }

    /// Releases one scan's slot without publishing.
    ///
    /// A cancelled or failed scan never becomes `Ready`; every tree already
    /// published stays visible, including those from scans that are still
    /// running alongside this one.
    ///
    /// Takes a [`ScanId`] now that scans are plural — the old signature could
    /// only mean "the" scan, and with several running that is not a question
    /// with an answer.
    pub fn release_unpublished(&self, scan_id: ScanId, failed: bool) {
        let mut lifecycle = lock(&self.lifecycle);
        let Some(running) = lifecycle.running.remove(&scan_id) else {
            return;
        };
        if !failed {
            return;
        }
        let row = RunningScanRow {
            scan_id,
            generation: running.active.generation,
            root: running.active.root.clone(),
            state: ScanState::Failed,
            waiting: None,
            last_progress: running.last_progress,
        };
        lifecycle.failed.push_back(row);
        while lifecycle.failed.len() > MAX_REMEMBERED_FAILURES {
            lifecycle.failed.pop_front();
        }
    }

    /// Requests cancellation of `scan_id`.
    #[must_use]
    pub fn cancel(&self, scan_id: ScanId) -> CancelState {
        let mut lifecycle = lock(&self.lifecycle);

        // A scan that has not started yet is cancelled by removing it from the
        // queue. There is no thread to signal and no resources to close, so the
        // caller is told it is finished — which is true, and is what a Cancel
        // button on a "waiting" row must do.
        if let Some(index) = lifecycle.waiting.iter().position(|pending| pending.scan_id == scan_id) {
            lifecycle.waiting.remove(index);
            return CancelState::ResourcesClosed;
        }

        let Some(running) = lifecycle.running.get_mut(&scan_id) else {
            // Not running and not waiting. Either it just finished, or the id
            // is unknown. A generation at or below the last published one means
            // it ran and ended.
            return if lifecycle.generation.is_none() {
                CancelState::NotFound
            } else {
                CancelState::AlreadyFinished
            };
        };
        running.state = ScanState::Cancelling;
        let cancel = Arc::clone(&running.active.cancel);
        cancel.request();
        if cancel.resources_closed() {
            CancelState::ResourcesClosed
        } else {
            CancelState::Acknowledged
        }
    }

    /// Records a progress snapshot so a client that just connected does not
    /// wait up to 100 ms for the next event.
    ///
    /// Filed against the scan it names. A snapshot for a scan that is no longer
    /// running is dropped: it is a late event from something that has already
    /// published or been cancelled, and applying it would resurrect a finished
    /// row in the UI.
    pub fn record_progress(&self, progress: ScanProgress) {
        let mut lifecycle = lock(&self.lifecycle);
        let Some(running) = lifecycle.running.get_mut(&progress.scan_id) else {
            return;
        };
        // Promote `Scanning -> Finalizing` when the scan reports rollup has
        // begun, so `scan_status` is truthful without the engine reaching into
        // this lock. Deliberately a promotion only: it must never pull the
        // state back out of `Cancelling`, which `cancel()` owns.
        if running.state == ScanState::Scanning && progress.state == ScanState::Finalizing {
            running.state = ScanState::Finalizing;
        }
        running.last_progress = Some(progress);
    }

    /// The `O(1)` answer to "where is the application".
    ///
    /// Carries both a scalar summary and the full lists. The scalars are what
    /// the tray and the window title read; the lists are what a UI showing
    /// several scans reads. Keeping both is why making scans plural did not
    /// require touching every surface at once.
    #[must_use]
    pub fn status(&self) -> ScanStatus {
        let (running, waiting, generation) = {
            let lifecycle = lock(&self.lifecycle);
            let running: Vec<RunningScanRow> = lifecycle
                .running
                .values()
                .map(|scan| RunningScanRow {
                    scan_id: scan.active.scan_id,
                    generation: scan.active.generation,
                    root: scan.active.root.clone(),
                    state: scan.state,
                    waiting: None,
                    last_progress: scan.last_progress.clone(),
                })
                .collect();
            let waiting: Vec<RunningScanRow> = lifecycle
                .waiting
                .iter()
                .map(|pending| RunningScanRow {
                    scan_id: pending.scan_id,
                    generation: pending.generation,
                    root: pending.root_display.clone(),
                    state: ScanState::Scanning,
                    waiting: Some(Self::admissible(&lifecycle, pending.device, 0).unwrap_or(WaitReason::Memory)),
                    last_progress: None,
                })
                .collect();
            (running, waiting, lifecycle.generation)
        };
        let failed: Vec<RunningScanRow> = lock(&self.lifecycle).failed.iter().cloned().collect();

        // Running first, then waiting, then anything that failed — the order a
        // user watching this list expects to read it in.
        let mut rows = running;
        rows.extend(waiting);
        rows.extend(failed);

        let ready: Vec<ReadyScanRow> = self
            .published_all()
            .into_iter()
            .map(|scan| ReadyScanRow {
                generation: scan.generation,
                scan_id: scan.scan_id,
                root: DisplayPath::from_bytes(scan.root_path.as_os_str().as_encoded_bytes()),
                summary: scan.summary(),
            })
            .collect();

        // The busiest state wins, because "is this app busy" must not answer
        // Ready while something is still reading a disk.
        let state = rows
            .iter()
            .map(|row| row.state)
            .max_by_key(|state| match state {
                ScanState::Scanning => 4,
                ScanState::Cancelling => 3,
                ScanState::Finalizing => 2,
                ScanState::Failed => 1,
                // `ScanState` is non-exhaustive; an unknown state is not busy.
                _ => 0,
            })
            .unwrap_or(if generation.is_none() {
                ScanState::Idle
            } else {
                ScanState::Ready
            });

        ScanStatus {
            state,
            // The newest scan that is actually live. A failed row is reported,
            // but it is not "the active scan" and a caller polling this to
            // decide whether to show a spinner must not be told it is.
            active_scan: rows
                .iter()
                .rev()
                .find(|row| row.state != ScanState::Failed)
                .map(|row| row.scan_id),
            generation,
            summary: self.published().map(|scan| scan.summary()),
            last_progress: rows.iter().rev().find_map(|row| row.last_progress.clone()),
            running: rows,
            ready,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan on a device, accepted and started if admissible.
    fn accept(state: &AppState, device: u64) -> Admission {
        state
            .accept(
                PathBuf::from(format!("/fixture/{device}")),
                DisplayPath::from_bytes(format!("/fixture/{device}").as_bytes()),
                device,
                ScanOptions::default(),
            )
            .expect("a fixture scan is always acceptable")
    }

    fn started(admission: Admission) -> WaitingScan {
        match admission {
            Admission::Start(pending) => *pending,
            Admission::Wait { reason, .. } => panic!("expected a start, got {reason:?}"),
        }
    }

    // The whole point of the change: two folders on two devices scan together.
    #[test]
    fn two_scans_on_different_devices_both_start() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        let second = started(accept(&state, 2));
        assert_ne!(first.scan_id, second.scan_id);
        assert_eq!(state.status().running.len(), 2);
        assert_eq!(state.status().state, ScanState::Scanning);
    }

    // And the constraint that survived: two scans on ONE device would contend,
    // so the second waits instead of racing the first.
    #[test]
    fn a_second_scan_on_the_same_device_waits_behind_the_first() {
        let state = AppState::new();
        let first = started(accept(&state, 7));
        match accept(&state, 7) {
            Admission::Wait { reason, .. } => {
                assert_eq!(reason, WaitReason::SameDevice { scan_id: first.scan_id });
            }
            Admission::Start(_) => panic!("a same-device scan must not start"),
        }
        let rows = state.status().running;
        assert_eq!(rows.len(), 2, "a waiting scan is still shown");
        assert!(rows[1].waiting.is_some(), "and is shown as waiting");
    }

    // Waiting is not failing. The queued scan runs the moment the device frees.
    #[test]
    fn a_waiting_scan_starts_when_the_device_frees() {
        let state = AppState::new();
        let first = started(accept(&state, 7));
        let queued = accept(&state, 7);
        let Admission::Wait {
            scan_id: waiting_id, ..
        } = queued
        else {
            panic!("expected a wait");
        };

        assert!(state.take_admissible().is_empty(), "still blocked while the first runs");

        state.release_unpublished(first.scan_id, false);
        let released = state.take_admissible();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].scan_id, waiting_id);
    }

    #[test]
    fn one_completion_can_release_several_waiting_scans() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        // Different devices, so only the budget could hold these — and with one
        // scan running there is headroom, so they start immediately.
        let second = started(accept(&state, 2));
        let third = started(accept(&state, 3));
        assert_eq!(state.status().running.len(), 3);
        state.release_unpublished(first.scan_id, false);
        state.release_unpublished(second.scan_id, false);
        state.release_unpublished(third.scan_id, false);
        assert!(state.status().running.is_empty());
    }

    // Cancelling something that has not started must not need a thread to
    // signal — it is removed from the queue, and that is a complete answer.
    #[test]
    fn cancelling_a_waiting_scan_removes_it_from_the_queue() {
        let state = AppState::new();
        started(accept(&state, 7));
        let Admission::Wait { scan_id, .. } = accept(&state, 7) else {
            panic!("expected a wait");
        };
        assert_eq!(state.cancel(scan_id), CancelState::ResourcesClosed);
        assert_eq!(state.status().running.len(), 1, "only the running scan is left");
        assert!(state.take_admissible().is_empty(), "and it does not start later");
    }

    #[test]
    fn scan_ids_and_generations_are_monotonic() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        let second = started(accept(&state, 2));
        assert!(second.scan_id.get() > first.scan_id.get());
        assert!(second.generation.get() > first.generation.get());
        assert!(!first.generation.is_none(), "a minted generation is never NONE");
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
        let first = started(accept(&state, 1));
        assert_eq!(
            state.cancel(ScanId::from_raw(first.scan_id.get() + 9)),
            CancelState::NotFound
        );
        assert_eq!(state.cancel(first.scan_id), CancelState::Acknowledged);
        assert!(first.cancel.is_cancelled());
    }

    // Cancelling one of several running scans must leave the others alone.
    #[test]
    fn cancelling_one_scan_does_not_touch_its_neighbour() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        let second = started(accept(&state, 2));
        assert_eq!(state.cancel(first.scan_id), CancelState::Acknowledged);
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled(), "the neighbour keeps running");
    }

    #[test]
    fn progress_promotes_scanning_to_finalizing_but_never_out_of_cancelling() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        assert_eq!(state.status().state, ScanState::Scanning);

        state.record_progress(ScanProgress {
            scan_id: first.scan_id,
            state: ScanState::Finalizing,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Finalizing);

        // A snapshot for a scan that is not running is dropped entirely.
        state.record_progress(ScanProgress {
            scan_id: ScanId::from_raw(first.scan_id.get() + 9),
            state: ScanState::Scanning,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Finalizing);

        assert_eq!(state.cancel(first.scan_id), CancelState::Acknowledged);
        state.record_progress(ScanProgress {
            scan_id: first.scan_id,
            state: ScanState::Finalizing,
            ..ScanProgress::default()
        });
        assert_eq!(state.status().state, ScanState::Cancelling);
    }

    // Progress is filed per scan, so two running scans do not overwrite each
    // other's counters — the defect a single `last_progress` slot would cause.
    #[test]
    fn each_scan_keeps_its_own_progress() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        let second = started(accept(&state, 2));

        state.record_progress(ScanProgress {
            scan_id: first.scan_id,
            observed_entries: 100,
            ..ScanProgress::default()
        });
        state.record_progress(ScanProgress {
            scan_id: second.scan_id,
            observed_entries: 900,
            ..ScanProgress::default()
        });

        let rows = state.status().running;
        let entries = |id: ScanId| {
            rows.iter()
                .find(|row| row.scan_id == id)
                .and_then(|row| row.last_progress.as_ref())
                .map(|progress| progress.observed_entries)
        };
        assert_eq!(entries(first.scan_id), Some(100));
        assert_eq!(entries(second.scan_id), Some(900));
    }

    // A failure has to survive the scan that produced it, or the user is never
    // told. Under the single-scan machine this was implicit.
    #[test]
    fn a_failed_scan_does_not_publish_and_says_so() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        state.release_unpublished(first.scan_id, true);
        let status = state.status();
        assert_eq!(status.state, ScanState::Failed);
        assert!(status.summary.is_none());
        assert!(status.active_scan.is_none(), "a failed scan is not the active one");
        assert!(state.published().is_none());
        assert_eq!(status.running.len(), 1, "the failure is still reported");
    }

    #[test]
    fn starting_a_new_scan_clears_the_previous_failure_notice() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        state.release_unpublished(first.scan_id, true);
        assert_eq!(state.status().state, ScanState::Failed);

        started(accept(&state, 1));
        assert_eq!(state.status().state, ScanState::Scanning);
        assert_eq!(state.status().running.len(), 1);
    }

    // A cancelled scan is not a failure: the user asked for it.
    #[test]
    fn a_cancelled_scan_leaves_no_notice() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        state.release_unpublished(first.scan_id, false);
        let status = state.status();
        assert!(status.running.is_empty());
        assert_eq!(status.state, ScanState::Idle);
    }

    #[test]
    fn the_last_progress_snapshot_is_available_without_waiting_for_an_event() {
        let state = AppState::new();
        let first = started(accept(&state, 1));
        assert!(state.status().last_progress.is_none());
        state.record_progress(ScanProgress {
            scan_id: first.scan_id,
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
        assert!(status.running.is_empty());
        assert!(status.ready.is_empty());
    }

    // The queue is bounded, so a runaway caller cannot grow it without limit —
    // and the refusal names something true rather than a placeholder.
    #[test]
    fn the_waiting_queue_is_bounded() {
        let state = AppState::new();
        started(accept(&state, 7));
        for _ in 0..MAX_WAITING_SCANS {
            let _ = accept(&state, 7);
        }
        let overflow = state.accept(
            PathBuf::from("/fixture/7"),
            DisplayPath::from_bytes(b"/fixture/7"),
            7,
            ScanOptions::default(),
        );
        assert!(
            matches!(overflow, Err(StartError::AlreadyScanning { .. })),
            "{overflow:?}"
        );
    }
}
