//! The CLI's reference scan engine: `StdReader` semantics, a bounded parallel
//! worker pool, and one single-writer builder.
//!
//! # Why this lives in the CLI
//!
//! `crates/rdirstat-scan` owns the production supervisor. This module is the
//! measurement surface's own walker, written against the same rules in
//! `docs/02-SCANNER.md` so the CLI can profile and verify the arena *today*,
//! and so `rdirstat verify` has an independent implementation to disagree with
//! when the two ever drift. It is deliberately confined to one function
//! boundary — [`scan`] — so the integration owner can point it at the scan
//! crate's supervisor by replacing one call, not by unpicking the CLI.
//!
//! # The rules it implements
//!
//! - **Never follow symlinks.** `DirEntry::metadata` is `symlink_metadata` on
//!   Unix; a symlink is always a leaf.
//! - **Never cross a device boundary** unless asked. A child whose `st_dev`
//!   differs from its parent's is marked [`flags::MOUNT_POINT`] and is not
//!   descended.
//! - **Never descend a firmlink.** `SF_FIRMLINK` is authoritative when the
//!   platform returns it.
//! - **Count hard-linked content once.** The first observed `(dev, ino)`
//!   contributes; later ones stay visible and contribute zero via
//!   [`Node::contributed_size`]. That policy lives in `rdirstat-core` and is
//!   not reimplemented here.
//! - **Keep logical and allocated separate.** `st_size` and `st_blocks * 512`
//!   are carried side by side and never summed.
//! - **Errors are recorded, not propagated.** Only a root failure, the memory
//!   limit, and arena exhaustion end a scan.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use rdirstat_core::{
    CompletedScan, ConfigHash, DirTotals, DisplayPath, ErrorClass, ErrorClassCount, Kind, MAX_DETAILED_ERRORS,
    MAX_TREE_DEPTH, Node, NodeId, Operation, ScanCounts, ScanError, ScanId, ScanOptions, ScanTotals, TreeBuilder,
    TreeGeneration, flags,
};

use crate::exclude::Rules;

/// Darwin `SF_FIRMLINK`. A firmlink is never descended during a root-volume
/// scan; the `/System/Volumes/Data` exclusion is defense in depth for readers
/// that do not surface this bit.
const SF_FIRMLINK: u32 = 0x0080_0000;

/// Entries per batch handed to the builder. Bounds the message size so a
/// directory with a million entries cannot become one allocation.
const BATCH_ENTRIES: usize = 1024;

/// Queued messages between the workers and the builder. Workers block here
/// instead of growing an O(node count) backlog.
const RESULT_QUEUE: usize = 256;

/// Directories that may sit in the dispatch channel at once.
const WORK_QUEUE: usize = 512;

/// How often the cancel flag and the memory projection are re-read, in entries.
const CHECK_INTERVAL: u64 = 1024;

/// Default reader workers when `--threads` is absent.
///
/// Not `num_cpus` by reflex: `docs/02-SCANNER.md` is explicit that too much
/// concurrency turns a sequential disk workload into seek contention. Eight,
/// capped by the machine, is a conservative starting point that the benchmark
/// profile is expected to revise.
const DEFAULT_WORKERS: u16 = 8;

/// What [`scan`] was asked to do.
#[derive(Debug)]
pub(crate) struct Config {
    /// The directory to scan.
    pub(crate) root: PathBuf,
    /// Traversal policy.
    pub(crate) options: ScanOptions,
    /// Whether to draw a progress line on stderr.
    pub(crate) progress: bool,
}

/// Everything measured about a scan that is not part of its result.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measurement {
    /// Wall-clock duration.
    pub(crate) wall_ms: u64,
    /// Reader workers actually started.
    pub(crate) threads: u16,
    /// Retained nodes × `size_of::<Node>()`, after the arena was shrunk.
    pub(crate) arena_bytes: u64,
    /// Name bytes interned.
    pub(crate) name_blob_bytes: u64,
    /// Name-blob capacity at the end of the scan, before the arena froze. The
    /// budget line is capacity, not length.
    pub(crate) name_blob_capacity_bytes: u64,
    /// Directory index bytes: `DirTotals` plus its parallel `NodeId`.
    pub(crate) dir_index_bytes: u64,
    /// Sampled peak resident set, if the sampler produced a reading.
    pub(crate) peak_rss_bytes: Option<u64>,
    /// Mean interned name length.
    pub(crate) mean_name_bytes: u32,
    /// Directories as a fraction of retained nodes, in basis points. Integer
    /// because no float belongs in the progress or measurement path.
    pub(crate) directory_ratio_bp: u32,
}

/// A completed scan plus its measurements.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// The frozen result.
    pub(crate) scan: CompletedScan,
    /// What it cost.
    pub(crate) measurement: Measurement,
}

/// One directory entry as the reader saw it.
///
/// The production reader interns this into a `SmallName`; here it is a boxed
/// slice, which is one allocation per entry and the honest cost of the
/// reference implementation.
#[derive(Debug)]
struct RawEntry {
    name: Box<[u8]>,
    kind: Kind,
    size: u64,
    alloc: u64,
    mtime: i64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    platform_flags: u32,
}

/// One directory handed to a worker: `(parent_id, path, root device)`.
#[derive(Debug)]
struct Job {
    parent: NodeId,
    path: Arc<Path>,
    device: u64,
    depth: u32,
}

/// Worker to builder.
#[derive(Debug)]
enum Message {
    /// A bounded slice of one directory's entries.
    Batch {
        parent: NodeId,
        directory: Arc<Path>,
        device: u64,
        depth: u32,
        entries: Vec<RawEntry>,
        errors: Vec<ScanError>,
    },
    /// That directory is done, successfully or not.
    Finished { parent: NodeId, failure: Option<ScanError> },
}

/// Runs a scan to completion.
///
/// # Errors
///
/// Only for a fatal condition: the root is unusable, the arena is exhausted, or
/// the configured memory limit was crossed. A directory that cannot be read is
/// recorded in the result and the scan continues — a partial-failure scan is a
/// success with a payload.
pub(crate) fn scan(config: &Config) -> Result<Outcome, ScanError> {
    let started = Instant::now();
    let started_unix_ms = unix_millis();
    let root = config.root.clone();

    let metadata = fs::symlink_metadata(&root).map_err(|error| ScanError::RootUnavailable {
        path: display(&root),
        reason: error.to_string(),
        class: class_of(&error),
    })?;
    if !metadata.is_dir() {
        return Err(ScanError::RootUnavailable {
            path: display(&root),
            reason: "scan root is not a directory".to_owned(),
            class: ErrorClass::NotADirectory,
        });
    }

    let rules = Rules::compile(&config.options.exclusions).map_err(|error| ScanError::RootUnavailable {
        path: display(&root),
        reason: error.to_string(),
        class: ErrorClass::Other,
    })?;

    let workers = worker_count(config.options.workers);
    let cancel = Arc::new(AtomicBool::new(false));
    let (work_tx, work_rx) = crossbeam_channel::bounded::<Job>(WORK_QUEUE);
    let (message_tx, message_rx) = crossbeam_channel::bounded::<Message>(RESULT_QUEUE);

    let mut handles = Vec::with_capacity(usize::from(workers));
    for index in 0..workers {
        let work_rx = work_rx.clone();
        let message_tx = message_tx.clone();
        let cancel = Arc::clone(&cancel);
        let handle = thread::Builder::new()
            .name(format!("rdirstat-reader-{index}"))
            .spawn(move || worker(&work_rx, &message_tx, &cancel))
            .map_err(|error| ScanError::RootUnavailable {
                path: display(&root),
                reason: format!("could not start reader thread: {error}"),
                class: ErrorClass::Other,
            })?;
        handles.push(handle);
    }
    drop(work_rx);
    drop(message_tx);

    let mut builder = Builder::new(config, rules, &metadata)?;
    let result = builder.run(&work_tx, &message_rx, &cancel, started);

    // Order matters: dropping the work channel retires idle workers, and
    // dropping the result receiver unblocks any worker that is mid-send after a
    // fatal error. Joining before both are dropped is how a cancel path hangs.
    drop(work_tx);
    drop(message_rx);
    for handle in handles {
        drop(handle.join());
    }
    result?;

    builder.finish(config, started, started_unix_ms, workers)
}

/// A reader worker: take a directory, stream its entries back in batches.
///
/// The `read_dir` span is what `--trace-chrome` turns into a worker timeline.
/// A phased parallel scanner's real cost is usually the gaps between these
/// spans — a worker waiting on the builder — and that is invisible in a
/// wall-clock number.
fn worker(work: &Receiver<Job>, out: &Sender<Message>, cancel: &AtomicBool) {
    while let Ok(job) = work.recv() {
        let span = tracing::debug_span!("read_dir", depth = job.depth);
        let entered = span.enter();
        let failure = if cancel.load(Ordering::Relaxed) {
            None
        } else {
            read_directory(&job, out, cancel)
        };
        drop(entered);
        if out
            .send(Message::Finished {
                parent: job.parent,
                failure,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Reads one directory with `StdReader` semantics.
///
/// Returns the directory-level failure, if any. Per-entry failures travel with
/// the batch they were found in.
fn read_directory(job: &Job, out: &Sender<Message>, cancel: &AtomicBool) -> Option<ScanError> {
    let iterator = match fs::read_dir(&job.path) {
        Ok(iterator) => iterator,
        Err(error) => return Some(scan_error(&job.path, Operation::OpenDir, &error)),
    };

    let mut entries = Vec::with_capacity(BATCH_ENTRIES);
    let mut errors = Vec::new();
    let mut seen: u64 = 0;

    for entry in iterator {
        seen += 1;
        if seen.is_multiple_of(CHECK_INTERVAL) && cancel.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(scan_error(&job.path, Operation::ReadDir, &error));
                continue;
            }
        };
        let path = entry.path();
        // Unix `DirEntry::metadata` is `symlink_metadata`: a symlink is never
        // followed, which is the whole traversal policy in one syscall.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(scan_error(&path, Operation::Metadata, &error));
                continue;
            }
        };
        let kind = kind_of(&metadata);
        let broken = kind == Kind::Symlink && fs::metadata(&path).is_err();
        let mut platform = platform_flags(&metadata);
        if broken {
            platform |= BROKEN_SYMLINK_MARKER;
        }
        entries.push(RawEntry {
            name: entry.file_name().as_os_str().as_bytes().to_vec().into_boxed_slice(),
            kind,
            size: metadata.size(),
            alloc: metadata.blocks().saturating_mul(512),
            mtime: metadata.mtime(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            mode: metadata.mode(),
            platform_flags: platform,
        });

        if entries.len() >= BATCH_ENTRIES {
            let batch = Message::Batch {
                parent: job.parent,
                directory: Arc::clone(&job.path),
                device: job.device,
                depth: job.depth,
                entries: std::mem::replace(&mut entries, Vec::with_capacity(BATCH_ENTRIES)),
                errors: std::mem::take(&mut errors),
            };
            if out.send(batch).is_err() {
                return None;
            }
        }
    }

    if !entries.is_empty() || !errors.is_empty() {
        let batch = Message::Batch {
            parent: job.parent,
            directory: Arc::clone(&job.path),
            device: job.device,
            depth: job.depth,
            entries,
            errors,
        };
        if out.send(batch).is_err() {
            return None;
        }
    }
    None
}

/// A private marker bit, set by the reader when a symlink target is missing.
/// It never reaches the arena; the builder translates it into
/// [`flags::BROKEN_SYMLINK`].
const BROKEN_SYMLINK_MARKER: u32 = 1 << 31;

/// The single-writer side: owns the arena, assigns every id, and is the only
/// thing that touches the builder.
struct Builder {
    tree: TreeBuilder,
    root_path: PathBuf,
    root_device: u64,
    counts: ScanCounts,
    errors: Vec<ScanError>,
    error_counts: HashMap<(ErrorClass, Option<Operation>), u64>,
    excluded_roots: Vec<DisplayPath>,
    hard_links: HashSet<(u64, u64)>,
    mutations: u64,
    rules: Rules,
    options: ScanOptions,
    queue: VecDeque<Job>,
    pending: u64,
    progress: Option<Progress>,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Builder")
            .field("nodes", &self.tree.len())
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Builder {
    /// Seeds the arena with the root node and the first job.
    fn new(config: &Config, rules: Rules, metadata: &fs::Metadata) -> Result<Self, ScanError> {
        let mut tree = TreeBuilder::new();
        let name = tree.intern(config.root.as_os_str().as_bytes())?;
        let root = tree.push_node(Node::directory(name, metadata.mtime()))?;
        tree.register_directory(root, DirTotals::EMPTY)?;

        let mut queue = VecDeque::new();
        queue.push_back(Job {
            parent: root,
            path: Arc::from(config.root.as_path()),
            device: metadata.dev(),
            depth: 0,
        });

        // The root counts as one observed, retained directory.
        let counts = ScanCounts {
            observed_entries: 1,
            directories: 1,
            retained_nodes: 1,
            ..ScanCounts::default()
        };

        Ok(Self {
            tree,
            root_path: config.root.clone(),
            root_device: metadata.dev(),
            counts,
            errors: Vec::new(),
            error_counts: HashMap::new(),
            excluded_roots: Vec::new(),
            hard_links: HashSet::new(),
            mutations: 0,
            rules,
            options: config.options.clone(),
            queue,
            pending: 1,
            progress: config.progress.then(Progress::new),
        })
    }

    /// The dispatch loop.
    ///
    /// Never blocks on a send, always drains results: that is what makes two
    /// bounded channels deadlock-free. Termination is decided by an exact
    /// pending-directory counter, never by an empty queue.
    fn run(
        &mut self,
        work: &Sender<Job>,
        messages: &Receiver<Message>,
        cancel: &AtomicBool,
        started: Instant,
    ) -> Result<(), ScanError> {
        let mut fatal: Option<ScanError> = None;
        loop {
            while let Some(job) = self.queue.pop_front() {
                match work.try_send(job) {
                    Ok(()) => {}
                    Err(TrySendError::Full(job)) => {
                        self.queue.push_front(job);
                        break;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        self.pending = 0;
                        break;
                    }
                }
            }
            if self.pending == 0 {
                break;
            }
            match messages.recv_timeout(Duration::from_millis(20)) {
                Ok(message) => {
                    if let Err(error) = self.absorb(message) {
                        cancel.store(true, Ordering::Relaxed);
                        fatal = Some(error);
                        self.queue.clear();
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if let Some(progress) = self.progress.as_mut() {
                progress.maybe_emit(&self.counts, self.pending, started);
            }
            if let Some(error) = self.memory_check() {
                cancel.store(true, Ordering::Relaxed);
                fatal = Some(error);
                self.queue.clear();
                self.pending = 0;
            }
            if fatal.is_some() && self.pending == 0 {
                break;
            }
        }
        if let Some(progress) = self.progress.as_mut() {
            progress.clear();
        }
        match fatal {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Folds one worker message into the arena.
    fn absorb(&mut self, message: Message) -> Result<(), ScanError> {
        match message {
            Message::Batch {
                parent,
                directory,
                device,
                depth,
                entries,
                errors,
            } => {
                // The builder is the one serialization point in the design, so
                // it gets its own span: if the workers are idle, this is where
                // the time went.
                let span = tracing::debug_span!("absorb", entries = entries.len());
                let _entered = span.enter();
                for error in errors {
                    self.record(error);
                }
                for entry in entries {
                    self.push_entry(parent, &directory, device, depth, &entry)?;
                }
            }
            Message::Finished { parent, failure } => {
                self.pending = self.pending.saturating_sub(1);
                if let Some(failure) = failure {
                    self.mark_unreadable(parent, &failure);
                    self.record(failure);
                }
            }
        }
        Ok(())
    }

    /// Adds one observed entry to its parent.
    fn push_entry(
        &mut self,
        parent: NodeId,
        directory: &Arc<Path>,
        device: u64,
        depth: u32,
        entry: &RawEntry,
    ) -> Result<(), ScanError> {
        self.counts.observed_entries = self.counts.observed_entries.saturating_add(1);
        if entry.kind == Kind::Directory {
            self.push_directory(parent, directory, device, depth, entry)
        } else {
            self.push_leaf(parent, entry)
        }
    }

    /// Retains a child directory and decides whether to descend into it.
    fn push_directory(
        &mut self,
        parent: NodeId,
        directory: &Arc<Path>,
        device: u64,
        depth: u32,
        entry: &RawEntry,
    ) -> Result<(), ScanError> {
        let path = directory.join(OsStr::from_bytes(&entry.name));
        let relative = path
            .strip_prefix(&self.root_path)
            .map_or_else(|_| entry.name.to_vec(), |rest| rest.as_os_str().as_bytes().to_vec());

        let excluded = !self.rules.is_empty() && self.rules.is_excluded(&entry.name, &relative);
        let crosses = entry.dev != device;
        let firmlink = entry.platform_flags & SF_FIRMLINK != 0;

        let mut bits = flags::NONE;
        if excluded {
            bits |= flags::EXCLUDED;
        }
        if crosses {
            bits |= flags::MOUNT_POINT;
        }
        if firmlink {
            bits |= flags::FIRMLINK;
        }
        if is_package_name(&entry.name) {
            bits |= flags::PACKAGE;
        }

        let name = self.tree.intern(&entry.name)?;
        let node = Node::directory(name, entry.mtime).with_flags(bits);
        let id = self.tree.push_child(parent, node)?;
        self.tree.register_directory(id, DirTotals::EMPTY)?;
        self.counts.directories = self.counts.directories.saturating_add(1);
        self.counts.retained_nodes = self.counts.retained_nodes.saturating_add(1);
        if let Some(totals) = self.tree.dir_totals_mut(parent) {
            totals.latest_mtime = totals.latest_mtime.max(entry.mtime);
        }

        let descend =
            !excluded && !firmlink && (!crosses || self.options.cross_filesystems) && depth + 1 < MAX_TREE_DEPTH;
        if descend {
            self.queue.push_back(Job {
                parent: id,
                path: Arc::from(path),
                device: entry.dev,
                depth: depth + 1,
            });
            self.pending = self.pending.saturating_add(1);
        } else if excluded {
            self.counts.excluded_paths = self.counts.excluded_paths.saturating_add(1);
            if self.excluded_roots.len() < MAX_DETAILED_ERRORS {
                self.excluded_roots.push(display(&path));
            }
        }
        Ok(())
    }

    /// Retains — or aggregates away — one leaf.
    fn push_leaf(&mut self, parent: NodeId, entry: &RawEntry) -> Result<(), ScanError> {
        let mut bits = flags::NONE;
        if entry.kind.is_file() {
            if entry.nlink > 1 {
                bits |= flags::HARD_LINK;
                if self.options.count_hard_links_once && !self.hard_links.insert((entry.dev, entry.ino)) {
                    bits |= flags::HARD_LINK_REPEAT;
                    self.counts.hard_link_repeats = self.counts.hard_link_repeats.saturating_add(1);
                }
            }
            if entry.alloc < entry.size {
                bits |= flags::SPARSE;
            }
            if entry.mode & 0o111 != 0 {
                bits |= flags::EXECUTABLE;
            }
        }
        if entry.kind == Kind::Symlink && entry.platform_flags & BROKEN_SYMLINK_MARKER != 0 {
            bits |= flags::BROKEN_SYMLINK;
        }

        let node = Node::leaf(
            rdirstat_core::NameRef::EMPTY,
            entry.kind,
            entry.size,
            entry.alloc,
            entry.mtime,
        )
        .with_flags(bits);
        let logical = node.contributed_size();
        let allocated = node.contributed_alloc();

        let aggregated = self
            .options
            .aggregate_below_bytes
            .is_some_and(|threshold| entry.kind.is_file() && entry.size < threshold);

        if aggregated {
            self.counts.aggregated_nodes = self.counts.aggregated_nodes.saturating_add(1);
            if let Some(totals) = self.tree.dir_totals_mut(parent) {
                totals.absorb_direct_file(logical, allocated, entry.mtime);
                totals.observed_entries = totals.observed_entries.saturating_add(1);
            }
            if let Some(node) = self.tree.node_mut(parent) {
                node.flags |= flags::AGGREGATED;
            }
            return Ok(());
        }

        let name = self.tree.intern(&entry.name)?;
        let mut node = node;
        node.name = name;
        self.tree.push_child(parent, node)?;
        self.counts.retained_nodes = self.counts.retained_nodes.saturating_add(1);
        match entry.kind {
            Kind::File => self.counts.files = self.counts.files.saturating_add(1),
            Kind::Symlink => self.counts.symlinks = self.counts.symlinks.saturating_add(1),
            _ => self.counts.special = self.counts.special.saturating_add(1),
        }
        if let Some(totals) = self.tree.dir_totals_mut(parent) {
            totals.absorb_direct_file(logical, allocated, entry.mtime);
            totals.observed_entries = totals.observed_entries.saturating_add(1);
            totals.retained_nodes = totals.retained_nodes.saturating_add(1);
        }
        Ok(())
    }

    /// Marks a directory whose read failed, so its totals read as a floor.
    fn mark_unreadable(&mut self, node: NodeId, failure: &ScanError) {
        if failure.class() != ErrorClass::PermissionDenied && failure.class() != ErrorClass::InputOutput {
            return;
        }
        if let Some(entry) = self.tree.node_mut(node) {
            entry.flags |= flags::UNREADABLE;
        }
        if let Some(totals) = self.tree.dir_totals_mut(node) {
            totals.unreadable = totals.unreadable.saturating_add(1);
        }
        self.counts.unreadable_dirs = self.counts.unreadable_dirs.saturating_add(1);
    }

    /// Records an error: full detail up to the cap, uncapped counts forever.
    fn record(&mut self, error: ScanError) {
        if matches!(error, ScanError::Vanished { .. }) {
            self.mutations = self.mutations.saturating_add(1);
        }
        let key = (error.class(), error.operation());
        *self.error_counts.entry(key).or_insert(0) += 1;
        if self.errors.len() < MAX_DETAILED_ERRORS {
            self.errors.push(error);
        }
    }

    /// Scanner-owned resident bytes, as counted rather than sampled.
    fn resident_estimate(&self) -> u64 {
        let nodes = u64::try_from(self.tree.len()).unwrap_or(u64::MAX);
        let (_, names) = self.tree.name_bytes();
        let names = u64::try_from(names).unwrap_or(u64::MAX);
        let dirs = u64::try_from(self.tree.directory_count()).unwrap_or(u64::MAX);
        let links = u64::try_from(self.hard_links.len()).unwrap_or(u64::MAX);
        nodes
            .saturating_mul(node_bytes())
            .saturating_add(names)
            .saturating_add(dirs.saturating_mul(60))
            .saturating_add(links.saturating_mul(24))
    }

    /// Ends the scan before allocation failure if the projection crosses the
    /// configured ceiling.
    fn memory_check(&self) -> Option<ScanError> {
        let limit = self.options.memory_limit_bytes?;
        let projected = self.resident_estimate();
        (projected > limit).then_some(ScanError::MemoryLimit {
            projected_bytes: projected,
            limit_bytes: limit,
        })
    }

    /// Rolls up, freezes, and packages the result.
    fn finish(
        mut self,
        config: &Config,
        started: Instant,
        started_unix_ms: i64,
        workers: u16,
    ) -> Result<Outcome, ScanError> {
        let (name_len, name_capacity) = self.tree.name_bytes();
        self.tree.rollup()?;
        let directories = self.tree.directory_count();
        let tree = self.tree.finish()?;
        let wall = started.elapsed();

        let root = tree.root();
        let totals = tree.dir_totals(root).map_or(
            ScanTotals {
                logical: 0,
                allocated: 0,
            },
            |dir| ScanTotals {
                logical: dir.logical,
                allocated: dir.allocated,
            },
        );

        let nodes = tree.len();
        self.counts.retained_nodes = u64::try_from(nodes).unwrap_or(u64::MAX);
        self.counts.directories = u64::try_from(directories).unwrap_or(u64::MAX);

        let mut error_counts: Vec<ErrorClassCount> = self
            .error_counts
            .into_iter()
            .map(|((class, operation), count)| ErrorClassCount {
                class,
                operation,
                count,
            })
            .collect();
        // Deterministic order, so two runs of the same scan produce the same
        // JSON: most frequent first, then a stable textual tiebreak.
        error_counts.sort_by_cached_key(|count| {
            (
                std::cmp::Reverse(count.count),
                format!("{:?}|{:?}", count.class, count.operation),
            )
        });

        let volume = crate::volume::identify(&config.root, self.root_device);
        let measurement = Measurement {
            wall_ms: u64::try_from(wall.as_millis()).unwrap_or(u64::MAX),
            threads: workers,
            arena_bytes: u64::try_from(nodes).unwrap_or(u64::MAX).saturating_mul(node_bytes()),
            name_blob_bytes: u64::try_from(name_len).unwrap_or(u64::MAX),
            name_blob_capacity_bytes: u64::try_from(name_capacity).unwrap_or(u64::MAX),
            dir_index_bytes: u64::try_from(directories).unwrap_or(u64::MAX).saturating_mul(60),
            peak_rss_bytes: None,
            mean_name_bytes: mean_name_bytes(name_len, nodes),
            directory_ratio_bp: ratio_bp(directories, nodes),
        };

        let scan = CompletedScan {
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            root_path: config.root.clone(),
            root,
            volume,
            started_unix_ms,
            finished_unix_ms: started_unix_ms.saturating_add(i64::try_from(wall.as_millis()).unwrap_or(i64::MAX)),
            options: config.options.clone(),
            exclusion_hash: config_hash(&exclusion_fingerprint(&config.options)),
            // rdirstat-classify has not landed a taxonomy yet, so every node
            // carries CategoryId::UNCATEGORIZED and the hash says exactly that
            // rather than pretending a configuration was applied.
            category_config_hash: config_hash("uncategorized"),
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
            counts: self.counts,
            totals,
            mutations: self.mutations,
            errors: self.errors,
            error_counts,
            excluded_roots: self.excluded_roots,
            tree,
        };
        Ok(Outcome { scan, measurement })
    }
}

/// The stderr progress line, throttled to at most `PROGRESS_MAX_HZ`.
#[derive(Debug)]
struct Progress {
    last: Instant,
    sequence: u64,
    drawn: bool,
}

impl Progress {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            sequence: 0,
            drawn: false,
        }
    }

    fn maybe_emit(&mut self, counts: &ScanCounts, pending: u64, started: Instant) {
        let interval = Duration::from_millis(1000 / u64::from(rdirstat_core::PROGRESS_MAX_HZ));
        if self.last.elapsed() < interval {
            return;
        }
        self.last = Instant::now();
        self.sequence += 1;
        self.drawn = true;
        // Integer arithmetic only: no float belongs in the progress path.
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        eprint!(
            "\r  {} entries · {} dirs · {} pending · {}.{}s   ",
            counts.observed_entries,
            counts.directories,
            pending,
            elapsed_ms / 1000,
            (elapsed_ms % 1000) / 100
        );
    }

    fn clear(&mut self) {
        if self.drawn {
            eprint!("\r{:60}\r", "");
            self.drawn = false;
        }
    }
}

/// `size_of::<Node>()`, which the contract pins at 48.
fn node_bytes() -> u64 {
    u64::try_from(size_of::<Node>()).unwrap_or(48)
}

fn mean_name_bytes(name_len: usize, nodes: usize) -> u32 {
    if nodes == 0 {
        return 0;
    }
    u32::try_from(name_len / nodes).unwrap_or(u32::MAX)
}

fn ratio_bp(directories: usize, nodes: usize) -> u32 {
    if nodes == 0 {
        return 0;
    }
    let numerator = u128::try_from(directories).unwrap_or(0).saturating_mul(10_000);
    let denominator = u128::try_from(nodes).unwrap_or(1).max(1);
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
}

/// Reader workers, clamped to something the machine can actually run.
fn worker_count(requested: Option<u16>) -> u16 {
    let available = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let available = u16::try_from(available).unwrap_or(u16::MAX);
    match requested {
        Some(0) | None => DEFAULT_WORKERS.min(available.max(1)),
        Some(count) => count.min(1024),
    }
}

/// A stable textual fingerprint of the effective rule set.
fn exclusion_fingerprint(options: &ScanOptions) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for rule in &options.exclusions {
        let _ignored = writeln!(
            text,
            "{:?}|{:?}|{:?}|{}|{}",
            rule.action, rule.scope, rule.syntax, rule.pattern, rule.case_sensitive
        );
    }
    text
}

/// FNV-1a 64 of `text`, widened into the 32-byte digest field.
///
/// Not SHA-256: no hash crate is in the scan path's dependency set and adding
/// one for a diagnostic fingerprint is not a trade this crate gets to make.
/// It is a fingerprint that changes when the configuration changes, which is
/// the only property anything reads it for.
fn config_hash(text: &str) -> ConfigHash {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut digest = [0_u8; 32];
    digest[..8].copy_from_slice(&hash.to_be_bytes());
    ConfigHash::from_digest(&digest)
}

/// macOS bundle suffixes that the UI treats as one thing.
fn is_package_name(name: &[u8]) -> bool {
    const SUFFIXES: [&[u8]; 7] = [
        b".app",
        b".bundle",
        b".framework",
        b".photoslibrary",
        b".fcpbundle",
        b".logicx",
        b".xcodeproj",
    ];
    SUFFIXES.iter().any(|suffix| ends_with_ignore_ascii_case(name, suffix))
}

/// Suffix comparison without allocating a lowercase copy per entry.
fn ends_with_ignore_ascii_case(name: &[u8], suffix: &[u8]) -> bool {
    if name.len() < suffix.len() {
        return false;
    }
    let tail = &name[name.len() - suffix.len()..];
    tail.eq_ignore_ascii_case(suffix)
}

fn kind_of(metadata: &fs::Metadata) -> Kind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_socket() {
        Kind::Socket
    } else if file_type.is_fifo() {
        Kind::Fifo
    } else if file_type.is_char_device() {
        Kind::CharDevice
    } else if file_type.is_block_device() {
        Kind::BlockDevice
    } else {
        Kind::Unknown
    }
}

#[cfg(target_os = "macos")]
fn platform_flags(metadata: &fs::Metadata) -> u32 {
    use std::os::macos::fs::MetadataExt as _;
    metadata.st_flags()
}

#[cfg(not(target_os = "macos"))]
fn platform_flags(_metadata: &fs::Metadata) -> u32 {
    0
}

fn display(path: &Path) -> DisplayPath {
    DisplayPath::from_bytes(path.as_os_str().as_bytes())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// Turns one `io::Error` into a typed, classified [`ScanError`].
fn scan_error(path: &Path, operation: Operation, error: &io::Error) -> ScanError {
    let class = class_of(error);
    let os_code = error.raw_os_error().unwrap_or(0);
    match class {
        ErrorClass::PermissionDenied => ScanError::PermissionDenied {
            path: display(path),
            operation,
            os_code,
        },
        ErrorClass::NotFound => ScanError::Vanished {
            path: display(path),
            operation,
        },
        _ => ScanError::Io {
            path: display(path),
            operation,
            class,
            os_code,
        },
    }
}

/// Maps an OS error onto the stable class the UI keys guidance off.
fn class_of(error: &io::Error) -> ErrorClass {
    // Darwin errno values. Matching on the raw code first keeps the mapping
    // stable across `io::ErrorKind` additions.
    match error.raw_os_error() {
        Some(1 | 13) => ErrorClass::PermissionDenied,
        Some(2) => ErrorClass::NotFound,
        Some(20) => ErrorClass::NotADirectory,
        Some(62) => ErrorClass::SymlinkLoop,
        Some(23 | 24) => ErrorClass::TooManyOpenFiles,
        Some(63) => ErrorClass::NameTooLong,
        Some(5) => ErrorClass::InputOutput,
        Some(60 | 64 | 65) => ErrorClass::RemoteUnavailable,
        _ => match error.kind() {
            io::ErrorKind::PermissionDenied => ErrorClass::PermissionDenied,
            io::ErrorKind::NotFound => ErrorClass::NotFound,
            io::ErrorKind::NotADirectory => ErrorClass::NotADirectory,
            io::ErrorKind::InvalidData => ErrorClass::InvalidName,
            _ => ErrorClass::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn options() -> ScanOptions {
        ScanOptions::default()
    }

    /// `ScanOptions` is `#[non_exhaustive]`, so tests mutate a `Default`
    /// instead of writing a struct literal.
    fn options_with(mutate: impl FnOnce(&mut ScanOptions)) -> ScanOptions {
        let mut options = ScanOptions::default();
        mutate(&mut options);
        options
    }

    fn scan_at(root: &Path, options: ScanOptions) -> Outcome {
        scan(&Config {
            root: root.to_path_buf(),
            options,
            progress: false,
        })
        .expect("fixture scans cleanly")
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut file = File::create(path).expect("create");
        file.write_all(bytes).expect("write");
    }

    #[test]
    fn an_empty_directory_has_one_node_and_zero_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.tree.len(), 1);
        assert_eq!(outcome.scan.totals.logical, 0);
        assert_eq!(outcome.scan.totals.allocated, 0);
        assert_eq!(outcome.scan.counts.directories, 1);
    }

    #[test]
    fn totals_roll_up_through_nested_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("a/b/c")).expect("mkdir");
        write(&dir.path().join("a/one"), &[7_u8; 1000]);
        write(&dir.path().join("a/b/two"), &[7_u8; 2000]);
        write(&dir.path().join("a/b/c/three"), &[7_u8; 3000]);

        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.totals.logical, 6000);
        assert!(outcome.scan.totals.allocated >= 6000, "allocated is block-rounded up");
        assert_eq!(outcome.scan.counts.files, 3);
        assert_eq!(outcome.scan.counts.directories, 4);
        assert_eq!(outcome.scan.tree.len(), 7);
    }

    #[test]
    fn a_symlink_is_a_leaf_and_its_target_is_not_walked_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("real")).expect("mkdir");
        write(&dir.path().join("real/file"), &[1_u8; 4096]);
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).expect("symlink");

        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.counts.symlinks, 1);
        assert_eq!(outcome.scan.counts.files, 1);
        assert_eq!(outcome.scan.counts.directories, 2);
    }

    #[test]
    fn a_symlink_loop_terminates() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("a")).expect("mkdir");
        std::os::unix::fs::symlink(dir.path().join("a"), dir.path().join("a/self")).expect("symlink");
        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.counts.symlinks, 1);
        assert_eq!(outcome.scan.counts.directories, 2);
    }

    #[test]
    fn a_broken_symlink_is_flagged_and_still_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dangling")).expect("symlink");
        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.counts.symlinks, 1);
        let broken = outcome
            .scan
            .tree
            .nodes()
            .iter()
            .any(|node| node.has_flags(flags::BROKEN_SYMLINK));
        assert!(broken, "the dangling link carries BROKEN_SYMLINK");
    }

    #[test]
    fn a_hard_link_contributes_once_and_stays_visible() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("original"), &[3_u8; 8192]);
        fs::hard_link(dir.path().join("original"), dir.path().join("second")).expect("link");

        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.counts.files, 2, "both entries stay visible");
        assert_eq!(outcome.scan.counts.hard_link_repeats, 1);
        assert_eq!(outcome.scan.totals.logical, 8192, "the bytes are counted once");

        let every_time = options_with(|options| options.count_hard_links_once = false);
        let doubled = scan_at(dir.path(), every_time);
        assert_eq!(doubled.scan.totals.logical, 16384);
    }

    #[test]
    fn an_exclusion_retains_the_node_but_never_descends_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("keep")).expect("mkdir");
        fs::create_dir_all(dir.path().join("skip/deep")).expect("mkdir");
        write(&dir.path().join("keep/a"), &[1_u8; 1024]);
        write(&dir.path().join("skip/deep/b"), &[1_u8; 4096]);

        let excluded = options_with(|options| {
            options.exclusions = crate::exclude::effective_rules(&["skip".to_owned()], false, false);
        });
        let outcome = scan_at(dir.path(), excluded);
        assert_eq!(outcome.scan.totals.logical, 1024);
        assert_eq!(outcome.scan.counts.excluded_paths, 1);
        assert_eq!(outcome.scan.excluded_roots.len(), 1);
        assert!(outcome.scan.is_partial());
    }

    #[test]
    fn aggregation_omits_small_files_but_keeps_their_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("small"), &[1_u8; 100]);
        write(&dir.path().join("large"), &vec![1_u8; 100_000]);

        let aggregated = options_with(|options| options.aggregate_below_bytes = Some(1024));
        let outcome = scan_at(dir.path(), aggregated);
        assert_eq!(outcome.scan.totals.logical, 100_100, "bytes survive aggregation");
        assert_eq!(outcome.scan.counts.aggregated_nodes, 1);
        assert_eq!(outcome.scan.counts.files, 1, "only the large file is retained");
        assert!(outcome.scan.is_aggregated());
        assert!(outcome.scan.counts.observed_entries > outcome.scan.counts.retained_nodes);
    }

    #[test]
    fn an_unreadable_directory_is_recorded_and_the_scan_continues() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("locked")).expect("mkdir");
        write(&dir.path().join("visible"), &[1_u8; 2048]);
        write(&dir.path().join("locked/hidden"), &[1_u8; 4096]);
        fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");

        let outcome = scan(&Config {
            root: dir.path().to_path_buf(),
            options: options(),
            progress: false,
        });
        // Restore before any assertion can fail, so TempDir can clean up.
        fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o755)).expect("chmod");
        let outcome = outcome.expect("an unreadable child is not a fatal error");

        assert_eq!(outcome.scan.totals.logical, 2048);
        assert_eq!(outcome.scan.counts.unreadable_dirs, 1);
        assert!(outcome.scan.is_partial());
        assert!(
            outcome
                .scan
                .error_counts
                .iter()
                .any(|count| count.class == ErrorClass::PermissionDenied)
        );
    }

    #[test]
    fn a_missing_root_is_a_fatal_error_not_an_empty_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = scan(&Config {
            root: dir.path().join("nope"),
            options: options(),
            progress: false,
        })
        .expect_err("a missing root fails the scan");
        assert!(matches!(error, ScanError::RootUnavailable { .. }));
        assert!(error.is_fatal());
    }

    #[test]
    fn a_file_root_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("file"), b"x");
        let error = scan(&Config {
            root: dir.path().join("file"),
            options: options(),
            progress: false,
        })
        .expect_err("a file is not a scan root");
        assert!(matches!(error, ScanError::RootUnavailable { .. }));
    }

    #[test]
    fn the_memory_limit_ends_the_scan_before_allocation_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..64 {
            write(&dir.path().join(format!("file-{index}")), &[1_u8; 16]);
        }
        let limited = options_with(|options| options.memory_limit_bytes = Some(64));
        let error = scan(&Config {
            root: dir.path().to_path_buf(),
            options: limited,
            progress: false,
        })
        .expect_err("the ceiling is enforced");
        assert!(matches!(error, ScanError::MemoryLimit { .. }));
    }

    #[test]
    fn worker_counts_are_clamped_but_honour_an_explicit_request() {
        assert_eq!(worker_count(Some(3)), 3);
        assert!(worker_count(None) >= 1);
        assert!(worker_count(Some(0)) >= 1);
    }

    #[test]
    fn a_single_worker_and_many_workers_agree() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..8 {
            let child = dir.path().join(format!("d{index}"));
            fs::create_dir(&child).expect("mkdir");
            write(&child.join("f"), &[1_u8; 512]);
        }
        let one = scan_at(dir.path(), options_with(|options| options.workers = Some(1)));
        let many = scan_at(dir.path(), options_with(|options| options.workers = Some(16)));
        assert_eq!(one.scan.totals.logical, many.scan.totals.logical);
        assert_eq!(one.scan.totals.allocated, many.scan.totals.allocated);
        assert_eq!(one.scan.tree.len(), many.scan.tree.len());
    }

    #[test]
    fn name_bytes_are_retained_exactly_as_the_filesystem_gave_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        // NFD "café": macOS stores the decomposed form, and a scanner that
        // round-trips through a normalizing String silently renames the file.
        let nfd = b"cafe\xcc\x81";
        write(&dir.path().join(OsStr::from_bytes(nfd)), &[1_u8; 64]);

        // Invalid UTF-8 is only creatable where the host permits it; APFS and
        // HFS+ reject it with EILSEQ. Try, and only assert when it worked.
        let invalid = b"weird-\xff\xfe-name";
        let invalid_created = File::create(dir.path().join(OsStr::from_bytes(invalid))).is_ok();

        let outcome = scan_at(dir.path(), options());
        let has = |wanted: &[u8]| {
            outcome.scan.tree.nodes().iter().any(|node| {
                outcome
                    .scan
                    .tree
                    .names()
                    .bytes(node.name)
                    .is_some_and(|bytes| bytes == wanted)
            })
        };
        assert!(has(nfd), "decomposed bytes survive unchanged");
        if invalid_created {
            assert!(has(invalid), "non-UTF-8 bytes survive unchanged");
        }
    }

    #[test]
    fn a_special_file_is_a_zero_byte_leaf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success());
        let outcome = scan_at(dir.path(), options());
        assert_eq!(outcome.scan.counts.special, 1);
        assert_eq!(outcome.scan.totals.allocated, 0);
    }

    #[test]
    fn the_arena_measurement_matches_the_node_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a"), &[1_u8; 10]);
        let outcome = scan_at(dir.path(), options());
        let nodes = u64::try_from(outcome.scan.tree.len()).expect("small");
        assert_eq!(outcome.measurement.arena_bytes, nodes * 48);
        assert!(outcome.measurement.name_blob_bytes > 0);
    }
}
