//! The scan driver: bounded reader workers feeding a single arena builder.
//!
//! # Integration seam
//!
//! **This module is the temporary home of `rdirstat-scan`.** The command layer
//! calls exactly one function, [`run`], and cares only about
//! [`ScanRequest`] in and [`ScanOutcome`] out. When `crates/rdirstat-scan`
//! lands its supervisor, replace the body of [`run`] with a call into it and
//! delete the rest of this file; nothing in `commands.rs`, `state.rs`, or
//! `progress.rs` changes.
//!
//! What is implemented here is real, not a stub: a bounded work queue, N reader
//! workers on `spawn_blocking`-equivalent OS threads, a single builder that is
//! the only writer of the arena, cooperative cancellation checked before every
//! directory read and between batches, the symlink / mount / firmlink /
//! hard-link / sparse policy, default root exclusions, aggregation, and the
//! memory-limit projection. What is *not* implemented is listed in
//! `RDIRSTAT-SCAN GAPS` below.
//!
//! ## `RDIRSTAT-SCAN GAPS`
//!
//! - Categorization is left at [`CategoryId::UNCATEGORIZED`]: `rdirstat-classify`
//!   owns the taxonomy and its longest-suffix-first matcher, and it is a stub at
//!   the time of writing. Every node therefore carries category `0`.
//! - [`RuleSyntax::Regex`] rules are refused at start
//!   ([`StartError::InvalidOptions`]); only the `*`/`?` glob subset is compiled
//!   here. No regex engine is a dependency of this crate.
//! - The default exclusion list is a conservative placeholder, not the
//!   canonical docs/02 list.
//! - `getattrlistbulk` is out of scope (it needs `unsafe`); this reads with
//!   `std::fs::read_dir` + `DirEntry::metadata`, i.e. one `lstat` per entry.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use rdirstat_core::{
    CategoryId, CompletedScan, ConfigHash, DirTotals, DisplayPath, ErrorClass, ErrorClassCount, Kind,
    MAX_DETAILED_ERRORS, MAX_TREE_DEPTH, Node, NodeId, Operation, RuleAction, RuleScope, RuleSyntax, ScanCounts,
    ScanError, ScanId, ScanOptions, ScanState, ScanTotals, TreeBuilder, TreeGeneration, VolumeId, flags,
};

use crate::fsident;
use crate::progress::ProgressCounters;
use crate::state::CancelToken;

/// Capacity of the directory work queue. The builder never blocks on it — it
/// keeps overflow in a local deque — so this bounds memory, not progress.
const WORK_CAPACITY: usize = 4_096;
/// Capacity of the result queue. Readers *do* block here; that is the
/// backpressure that keeps the builder from being buried.
const RESULT_CAPACITY: usize = 64;
/// How often the memory projection is re-checked, in observed entries.
const MEMORY_CHECK_EVERY: u64 = 1 << 20;
/// `SF_FIRMLINK`, the flag macOS sets on `/System/Volumes/Data` style links.
const SF_FIRMLINK: u32 = 0x0080_0000;

/// Everything one scan needs.
#[derive(Debug)]
pub(crate) struct ScanRequest {
    /// The validated scan root.
    pub root: PathBuf,
    /// The options in force.
    pub options: ScanOptions,
    /// The attempt id.
    pub scan_id: ScanId,
    /// The generation this scan will publish under.
    pub generation: TreeGeneration,
    /// Cooperative cancellation.
    pub cancel: Arc<CancelToken>,
    /// The atomics the 10 Hz emitter reads.
    pub counters: Arc<ProgressCounters>,
}

/// How a scan ended.
#[derive(Debug)]
pub(crate) enum ScanOutcome {
    /// A complete (possibly partial-by-permission) tree, ready to publish.
    Completed(Box<CompletedScan>),
    /// Cancelled. Nothing is published and the partial arena is dropped.
    Cancelled,
    /// A fatal failure. Nothing is published.
    Failed(ScanError),
}

/// One entry as a reader observed it.
#[derive(Debug)]
struct RawEntry {
    name: Vec<u8>,
    observation: fsident::Observation,
    firmlink: bool,
    broken_symlink: bool,
}

/// One directory's worth of entries, or the error that stopped it.
#[derive(Debug)]
struct DirBatch {
    parent: NodeId,
    path: PathBuf,
    depth: u32,
    entries: Vec<RawEntry>,
    error: Option<ScanError>,
}

#[derive(Debug, Clone)]
struct DirJob {
    parent: NodeId,
    path: PathBuf,
    depth: u32,
}

/// A compiled exclusion rule: the glob is pre-split so nothing compiles inside
/// the scan loop.
#[derive(Debug, Clone)]
struct CompiledRule {
    action: RuleAction,
    scope: RuleScope,
    pattern: String,
    case_sensitive: bool,
}

/// The conservative default root exclusions.
///
/// Root-relative, so they only bite when the scan root is `/`. This is a
/// placeholder for the canonical docs/02 list, which `rdirstat-scan` owns.
const DEFAULT_EXCLUSIONS: [&str; 11] = [
    "dev",
    "net",
    "home",
    "Volumes",
    "System/Volumes/*",
    "private/var/vm",
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".vol",
];

/// Compiles the effective rule set.
///
/// # Errors
///
/// The `detail` of a [`StartError::InvalidOptions`](rdirstat_core::StartError::InvalidOptions)
/// when a rule cannot be compiled here.
pub(crate) fn compile_rules(options: &ScanOptions) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for rule in &options.exclusions {
        if rule.syntax == RuleSyntax::Regex {
            return Err(format!(
                "regex exclusion `{}` is not supported by this build; use a glob",
                rule.pattern
            ));
        }
        if rule.pattern.is_empty() {
            return Err("an exclusion pattern may not be empty".to_owned());
        }
        names.push(rule.pattern.clone());
    }
    Ok(names)
}

fn effective_rules(options: &ScanOptions) -> Vec<CompiledRule> {
    let mut rules = Vec::with_capacity(options.exclusions.len() + DEFAULT_EXCLUSIONS.len());
    // User rules first: first match wins, so an explicit `Include` can override
    // a shipped default.
    for rule in &options.exclusions {
        rules.push(CompiledRule {
            action: rule.action,
            scope: rule.scope,
            pattern: rule.pattern.clone(),
            case_sensitive: rule.case_sensitive,
        });
    }
    if options.apply_default_exclusions {
        for pattern in DEFAULT_EXCLUSIONS {
            rules.push(CompiledRule {
                action: RuleAction::Exclude,
                scope: RuleScope::RootRelativePath,
                pattern: (*pattern).to_owned(),
                case_sensitive: true,
            });
        }
    }
    rules
}

/// Matches a `*` / `?` glob against a subject. Linear-time backtracking; there
/// is no character class, no `**`, and no regex.
fn glob_match(pattern: &str, subject: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let subject: Vec<char> = subject.chars().collect();
    let (mut p, mut s) = (0_usize, 0_usize);
    let (mut star, mut resume) = (usize::MAX, 0_usize);
    while s < subject.len() {
        let matched = p < pattern.len() && (pattern[p] == '?' || pattern.get(p).copied() == subject.get(s).copied());
        if matched {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = p;
            resume = s;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            resume += 1;
            s = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn rule_matches(rule: &CompiledRule, name: &str, relative: &str) -> bool {
    let subject = match rule.scope {
        RuleScope::DirectoryName => name,
        _ => relative,
    };
    if rule.case_sensitive {
        glob_match(&rule.pattern, subject)
    } else {
        glob_match(&rule.pattern.to_lowercase(), &subject.to_lowercase())
    }
}

/// Whether a path is excluded. First match wins; an `Include` rule short-circuits.
fn is_excluded(rules: &[CompiledRule], name: &str, relative: &str) -> bool {
    for rule in rules {
        if rule_matches(rule, name, relative) {
            return rule.action == RuleAction::Exclude;
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn st_flags(metadata: &std::fs::Metadata) -> u32 {
    use std::os::macos::fs::MetadataExt as _;
    metadata.st_flags()
}

#[cfg(not(target_os = "macos"))]
const fn st_flags(_metadata: &std::fs::Metadata) -> u32 {
    0
}

/// Reads exactly one directory. Runs on a reader worker thread.
fn read_directory(job: &DirJob, cancel: &CancelToken) -> DirBatch {
    let mut batch = DirBatch {
        parent: job.parent,
        path: job.path.clone(),
        depth: job.depth,
        entries: Vec::new(),
        error: None,
    };
    if cancel.is_cancelled() {
        return batch;
    }
    let iterator = match std::fs::read_dir(&job.path) {
        Ok(iterator) => iterator,
        Err(error) => {
            batch.error = Some(fsident::scan_error(&job.path, Operation::OpenDir, &error));
            return batch;
        }
    };
    for entry in iterator {
        if cancel.is_cancelled() {
            batch.entries.clear();
            return batch;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                batch.error = Some(fsident::scan_error(&job.path, Operation::ReadDir, &error));
                continue;
            }
        };
        // `DirEntry::metadata` is `lstat` on unix: symlinks are never followed.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                // A vanished entry is a mutation, not a scan failure.
                batch.error = Some(fsident::scan_error(&entry.path(), Operation::Metadata, &error));
                continue;
            }
        };
        let observation = fsident::observe_metadata(&metadata);
        let broken_symlink = observation.kind == Kind::Symlink && std::fs::metadata(entry.path()).is_err();
        batch.entries.push(RawEntry {
            name: entry.file_name().as_encoded_bytes().to_vec(),
            observation,
            firmlink: st_flags(&metadata) & SF_FIRMLINK != 0,
            broken_symlink,
        });
    }
    batch
}

fn spawn_workers(
    worker_count: usize,
    work_rx: &Receiver<DirJob>,
    result_tx: &Sender<DirBatch>,
    cancel: &Arc<CancelToken>,
) -> Vec<std::thread::JoinHandle<()>> {
    (0..worker_count)
        .filter_map(|index| {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let cancel = Arc::clone(cancel);
            std::thread::Builder::new()
                .name(format!("rdirstat-reader-{index}"))
                .spawn(move || {
                    while let Ok(job) = work_rx.recv() {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let batch = read_directory(&job, &cancel);
                        if result_tx.send(batch).is_err() {
                            break;
                        }
                    }
                })
                .ok()
        })
        .collect()
}

fn worker_count(options: &ScanOptions) -> usize {
    options.workers.map_or_else(
        || {
            std::thread::available_parallelism()
                .map_or(4, std::num::NonZeroUsize::get)
                .clamp(2, 8)
        },
        |requested| usize::from(requested).clamp(1, 64),
    )
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// A deterministic, non-cryptographic digest of the effective configuration.
///
/// It is a *comparison key* — "were these two scans configured the same way" —
/// not a security primitive. Four FNV-1a passes with different offset bases
/// fill the 32 bytes [`ConfigHash::from_digest`] wants.
fn config_hash(parts: &[&str]) -> ConfigHash {
    const BASES: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xff51_afd7_ed55_8ccd,
        0xc4ce_b9fe_1a85_ec53,
    ];
    let mut digest = [0_u8; 32];
    for (lane, base) in BASES.iter().enumerate() {
        let mut hash = *base;
        for part in parts {
            for byte in part.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let bytes = hash.to_be_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if let Some(slot) = digest.get_mut(lane * 8 + index) {
                *slot = *byte;
            }
        }
    }
    ConfigHash::from_digest(&digest)
}

fn options_hash(options: &ScanOptions) -> ConfigHash {
    let mut parts: Vec<String> = vec![
        format!("cross={}", options.cross_filesystems),
        format!("links_once={}", options.count_hard_links_once),
        format!("defaults={}", options.apply_default_exclusions),
        format!("aggregate={:?}", options.aggregate_below_bytes),
    ];
    for rule in &options.exclusions {
        parts.push(format!(
            "{:?}:{:?}:{:?}:{}:{}",
            rule.action, rule.scope, rule.syntax, rule.pattern, rule.case_sensitive
        ));
    }
    let borrowed: Vec<&str> = parts.iter().map(String::as_str).collect();
    config_hash(&borrowed)
}

/// Running tallies the builder keeps; mirrored into the progress atomics.
#[derive(Debug, Default)]
struct Tally {
    counts: ScanCounts,
    mutations: u64,
    errors: Vec<ScanError>,
    error_counts: Vec<ErrorClassCount>,
    excluded_roots: Vec<DisplayPath>,
}

impl Tally {
    fn record(&mut self, error: ScanError) {
        let class = error.class();
        let operation = error.operation();
        if let Some(existing) = self
            .error_counts
            .iter_mut()
            .find(|entry| entry.class == class && entry.operation == operation)
        {
            existing.count = existing.count.saturating_add(1);
        } else {
            self.error_counts.push(ErrorClassCount {
                class,
                operation,
                count: 1,
            });
        }
        if class == ErrorClass::NotFound {
            self.mutations = self.mutations.saturating_add(1);
        }
        if self.errors.len() < MAX_DETAILED_ERRORS {
            self.errors.push(error);
        }
    }
}

/// Runs one scan to completion, cancellation, or fatal failure.
///
/// Called from a dedicated OS thread — never from the async command executor.
#[allow(
    clippy::too_many_lines,
    reason = "one builder loop; splitting it would hide the ownership story"
)]
pub(crate) fn run(request: ScanRequest) -> ScanOutcome {
    let ScanRequest {
        root,
        options,
        scan_id,
        generation,
        cancel,
        counters,
    } = request;

    let started_unix_ms = now_unix_ms();
    let root_metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) => {
            return ScanOutcome::Failed(ScanError::RootUnavailable {
                path: DisplayPath::from_bytes(root.as_os_str().as_encoded_bytes()),
                reason: error.to_string(),
                class: fsident::error_class(&error),
            });
        }
    };
    let root_observation = fsident::observe_metadata(&root_metadata);
    let rules = effective_rules(&options);

    let mut builder = TreeBuilder::new();
    let root_name = match builder.intern(root.as_os_str().as_encoded_bytes()) {
        Ok(name) => name,
        Err(error) => return ScanOutcome::Failed(ScanError::Arena(error)),
    };
    let root_id = match builder.push_node(Node::directory(root_name, root_observation.mtime)) {
        Ok(id) => id,
        Err(error) => return ScanOutcome::Failed(ScanError::Arena(error)),
    };
    if let Err(error) = builder.register_directory(root_id, DirTotals::EMPTY) {
        return ScanOutcome::Failed(ScanError::Arena(error));
    }
    counters
        .name_bytes
        .fetch_add(root.as_os_str().as_encoded_bytes().len() as u64, Ordering::Relaxed);
    counters.retained_nodes.store(1, Ordering::Relaxed);

    let (work_tx, work_rx) = crossbeam_channel::bounded::<DirJob>(WORK_CAPACITY);
    let (result_tx, result_rx) = crossbeam_channel::bounded::<DirBatch>(RESULT_CAPACITY);
    let workers = spawn_workers(worker_count(&options), &work_rx, &result_tx, &cancel);
    drop(work_rx);
    drop(result_tx);

    let mut tally = Tally::default();
    let mut hard_links: HashSet<(u64, u64)> = HashSet::new();
    let mut local: VecDeque<DirJob> = VecDeque::new();
    let mut pending: u64 = 1;
    let mut fatal: Option<ScanError> = None;
    let root_prefix_len = root.as_os_str().as_encoded_bytes().len();

    local.push_back(DirJob {
        parent: root_id,
        path: root.clone(),
        depth: 0,
    });
    counters.pending_dirs.store(pending, Ordering::Relaxed);

    'builder: loop {
        while let Some(job) = local.pop_front() {
            match work_tx.try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(job)) => {
                    local.push_front(job);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break 'builder,
            }
        }
        if pending == 0 || cancel.is_cancelled() {
            break;
        }
        let batch = match result_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(batch) => batch,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        pending = pending.saturating_sub(1);
        counters
            .result_queue_depth
            .store(u32::try_from(result_rx.len()).unwrap_or(u32::MAX), Ordering::Relaxed);
        counters.offer_current_dir(batch.path.as_os_str().as_encoded_bytes());

        if let Some(error) = batch.error {
            if error.class() == ErrorClass::PermissionDenied {
                tally.counts.unreadable_dirs = tally.counts.unreadable_dirs.saturating_add(1);
                if let Some(node) = builder.node_mut(batch.parent) {
                    node.flags |= flags::UNREADABLE;
                }
                if let Some(totals) = builder.dir_totals_mut(batch.parent) {
                    totals.unreadable = totals.unreadable.saturating_add(1);
                }
            }
            tally.record(error);
            counters.errors.store(
                tally.error_counts.iter().map(|entry| entry.count).sum(),
                Ordering::Relaxed,
            );
        }

        tally.counts.directories = tally.counts.directories.saturating_add(1);
        counters.directories.fetch_add(1, Ordering::Relaxed);

        for entry in batch.entries {
            if cancel.is_cancelled() {
                break 'builder;
            }
            tally.counts.observed_entries = tally.counts.observed_entries.saturating_add(1);
            counters.observed_entries.fetch_add(1, Ordering::Relaxed);

            let child_path = batch.path.join(OsStr::from_bytes(&entry.name));
            let relative_bytes = child_path
                .as_os_str()
                .as_encoded_bytes()
                .get(root_prefix_len..)
                .unwrap_or_default();
            let relative = String::from_utf8_lossy(relative_bytes.strip_prefix(b"/").unwrap_or(relative_bytes));
            let name = String::from_utf8_lossy(&entry.name);

            if is_excluded(&rules, &name, &relative) {
                tally.counts.excluded_paths = tally.counts.excluded_paths.saturating_add(1);
                if tally.excluded_roots.len() < MAX_DETAILED_ERRORS {
                    tally
                        .excluded_roots
                        .push(DisplayPath::from_bytes(child_path.as_os_str().as_encoded_bytes()));
                }
                continue;
            }

            let observation = entry.observation;
            let crosses_device = observation.device != root_observation.device;
            let is_directory = observation.kind == Kind::Directory;

            let mut bits = flags::NONE;
            if entry.firmlink {
                bits |= flags::FIRMLINK;
            }
            if crosses_device {
                bits |= flags::MOUNT_POINT;
            }
            if observation.executable && observation.kind == Kind::File {
                bits |= flags::EXECUTABLE;
            }
            if entry.broken_symlink {
                bits |= flags::BROKEN_SYMLINK;
            }
            if observation.kind == Kind::File && observation.allocated < observation.size {
                bits |= flags::SPARSE;
            }

            let mut contributed = observation.size;
            let mut contributed_alloc = observation.allocated;
            if observation.links > 1 && observation.kind == Kind::File {
                bits |= flags::HARD_LINK;
                if options.count_hard_links_once && !hard_links.insert((observation.device, observation.inode)) {
                    bits |= flags::HARD_LINK_REPEAT;
                    contributed = 0;
                    contributed_alloc = 0;
                    tally.counts.hard_link_repeats = tally.counts.hard_link_repeats.saturating_add(1);
                }
            }

            // Aggregation: the entry is observed and its bytes counted, but no
            // node is retained. The parent is marked so the UI can say so.
            let aggregated = !is_directory
                && options
                    .aggregate_below_bytes
                    .is_some_and(|threshold| observation.size < threshold);
            if aggregated {
                tally.counts.aggregated_nodes = tally.counts.aggregated_nodes.saturating_add(1);
                if let Some(node) = builder.node_mut(batch.parent) {
                    node.flags |= flags::AGGREGATED;
                }
                if let Some(totals) = builder.dir_totals_mut(batch.parent) {
                    totals.observed_entries = totals.observed_entries.saturating_add(1);
                    totals.absorb_direct_file(contributed, contributed_alloc, observation.mtime);
                }
                counters.logical_bytes.fetch_add(contributed, Ordering::Relaxed);
                counters.allocated_bytes.fetch_add(contributed_alloc, Ordering::Relaxed);
                continue;
            }

            let name_ref = match builder.intern(&entry.name) {
                Ok(name_ref) => name_ref,
                Err(error) => {
                    fatal = Some(ScanError::Arena(error));
                    break 'builder;
                }
            };
            let node = if is_directory {
                Node::directory(name_ref, observation.mtime).with_flags(bits)
            } else {
                Node::leaf(
                    name_ref,
                    observation.kind,
                    observation.size,
                    observation.allocated,
                    observation.mtime,
                )
                .with_flags(bits)
                .with_category(CategoryId::UNCATEGORIZED)
            };
            let child_id = match builder.push_child(batch.parent, node) {
                Ok(id) => id,
                Err(error) => {
                    fatal = Some(ScanError::Arena(error));
                    break 'builder;
                }
            };
            tally.counts.retained_nodes = tally.counts.retained_nodes.saturating_add(1);
            counters.retained_nodes.fetch_add(1, Ordering::Relaxed);
            counters
                .name_bytes
                .fetch_add(entry.name.len() as u64, Ordering::Relaxed);

            if is_directory {
                if let Err(error) = builder.register_directory(child_id, DirTotals::EMPTY) {
                    fatal = Some(ScanError::Arena(error));
                    break 'builder;
                }
                let descend = batch.depth + 1 < MAX_TREE_DEPTH
                    && !entry.firmlink
                    && (options.cross_filesystems || !crosses_device);
                if descend {
                    local.push_back(DirJob {
                        parent: child_id,
                        path: child_path,
                        depth: batch.depth + 1,
                    });
                    pending = pending.saturating_add(1);
                }
            } else {
                match observation.kind {
                    Kind::File => tally.counts.files = tally.counts.files.saturating_add(1),
                    Kind::Symlink => tally.counts.symlinks = tally.counts.symlinks.saturating_add(1),
                    _ => tally.counts.special = tally.counts.special.saturating_add(1),
                }
                if let Some(totals) = builder.dir_totals_mut(batch.parent) {
                    totals.observed_entries = totals.observed_entries.saturating_add(1);
                    totals.retained_nodes = totals.retained_nodes.saturating_add(1);
                    totals.absorb_direct_file(contributed, contributed_alloc, observation.mtime);
                }
                counters.logical_bytes.fetch_add(contributed, Ordering::Relaxed);
                counters.allocated_bytes.fetch_add(contributed_alloc, Ordering::Relaxed);
            }
        }

        counters.pending_dirs.store(pending, Ordering::Relaxed);

        if let Some(limit) = options.memory_limit_bytes
            && tally.counts.observed_entries % MEMORY_CHECK_EVERY < 4_096
        {
            let projected = counters.projected_peak_bytes();
            if projected > limit {
                fatal = Some(ScanError::MemoryLimit {
                    projected_bytes: projected,
                    limit_bytes: limit,
                });
                break;
            }
        }
    }

    // Shut the readers down and account for every handle before reporting.
    // Order matters. Dropping the *receiver* first is what unblocks a worker
    // that is parked in `result_tx.send()` on a full queue — draining with
    // `try_recv` would race it and the join below would hang forever on a
    // cancel. With the receiver gone, `send` fails, the worker breaks, and then
    // the closed work channel ends the rest.
    drop(work_tx);
    drop(result_rx);
    for worker in workers {
        drop(worker.join());
    }
    cancel.mark_resources_closed();

    if let Some(error) = fatal {
        return ScanOutcome::Failed(error);
    }
    if cancel.is_cancelled() {
        counters.set_state(ScanState::Cancelling);
        return ScanOutcome::Cancelled;
    }

    counters.set_state(ScanState::Finalizing);
    if let Err(error) = builder.rollup() {
        return ScanOutcome::Failed(ScanError::Arena(error));
    }
    let tree = match builder.finish() {
        Ok(tree) => tree,
        Err(error) => return ScanOutcome::Failed(ScanError::Arena(error)),
    };

    let totals = tree
        .dir_totals(root_id)
        .map_or(ScanTotals::default(), |dir| ScanTotals {
            logical: dir.logical,
            allocated: dir.allocated,
        });
    // The root itself is a retained node that no parent rollup counted.
    tally.counts.retained_nodes = tally.counts.retained_nodes.saturating_add(1);
    tally.counts.directories = tally.counts.directories.max(1);

    let volume = VolumeId {
        device: root_observation.device,
        fs_type: crate::volumes::fs_type_at(&root).unwrap_or_else(|| "unknown".to_owned()),
        volume_uuid: None,
        mount_point: DisplayPath::from_bytes(
            crate::volumes::mount_point_at(&root)
                .unwrap_or_else(|| root.clone())
                .as_os_str()
                .as_encoded_bytes(),
        ),
        case_preserving: true,
        case_sensitive: false,
    };

    ScanOutcome::Completed(Box::new(CompletedScan {
        scan_id,
        generation,
        root_path: root,
        root: root_id,
        volume,
        started_unix_ms,
        finished_unix_ms: now_unix_ms(),
        exclusion_hash: options_hash(&options),
        category_config_hash: config_hash(&["uncategorized-v0"]),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        options,
        counts: tally.counts,
        totals,
        mutations: tally.mutations,
        errors: tally.errors,
        error_counts: tally.error_counts,
        excluded_roots: tally.excluded_roots,
        tree,
    }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("top.txt"), vec![b'x'; 100]).expect("write");
        std::fs::write(dir.path().join("a/one.bin"), vec![b'y'; 2_000]).expect("write");
        std::fs::write(dir.path().join("a/b/two.bin"), vec![b'z'; 30_000]).expect("write");
        std::os::unix::fs::symlink(dir.path().join("top.txt"), dir.path().join("link")).expect("symlink");
        dir
    }

    /// `ScanOptions` is `#[non_exhaustive]`, so a struct literal is not
    /// available to a downstream crate. Build it by assignment instead.
    fn options() -> ScanOptions {
        ScanOptions::default()
    }

    fn scan(root: &Path, options: ScanOptions) -> ScanOutcome {
        run(ScanRequest {
            root: root.to_path_buf(),
            options,
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        })
    }

    #[test]
    fn a_small_tree_scans_to_completion_with_correct_totals() {
        let dir = fixture();
        let ScanOutcome::Completed(scan) = scan(dir.path(), ScanOptions::default()) else {
            panic!("expected a completed scan");
        };
        assert_eq!(scan.counts.directories, 3, "root, a, a/b");
        assert_eq!(scan.counts.files, 3);
        assert_eq!(scan.counts.symlinks, 1);
        assert_eq!(scan.totals.logical, 100 + 2_000 + 30_000 + link_len(dir.path()));
        assert!(scan.totals.allocated >= scan.totals.logical.min(4_096));
        assert_eq!(scan.root, NodeId::ROOT);
        assert_eq!(scan.tree.root(), NodeId::ROOT);
    }

    fn link_len(root: &Path) -> u64 {
        std::fs::symlink_metadata(root.join("link")).expect("stat").len()
    }

    #[test]
    fn every_node_is_reachable_and_paths_reconstruct() {
        let dir = fixture();
        let ScanOutcome::Completed(scan) = scan(dir.path(), ScanOptions::default()) else {
            panic!("expected a completed scan");
        };
        let mut found = false;
        let mut stack = vec![scan.root];
        while let Some(node) = stack.pop() {
            for child in scan.tree.children(node) {
                stack.push(child);
                let mut bytes = Vec::new();
                scan.tree.path_bytes(child, &mut bytes).expect("path");
                let path = Path::new(OsStr::from_bytes(&bytes));
                assert!(path.starts_with(dir.path()), "{path:?} escaped the root");
                if path.ends_with("two.bin") {
                    found = true;
                    assert!(std::fs::symlink_metadata(path).is_ok(), "{path:?} must exist");
                }
            }
        }
        assert!(found, "the deepest file must be reachable");
    }

    #[test]
    fn a_cancelled_scan_publishes_nothing() {
        let dir = fixture();
        let cancel = Arc::new(CancelToken::new());
        cancel.request();
        let outcome = run(ScanRequest {
            root: dir.path().to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::clone(&cancel),
            counters: Arc::new(ProgressCounters::new()),
        });
        assert!(matches!(outcome, ScanOutcome::Cancelled));
        assert!(cancel.resources_closed(), "readers must be accounted for");
    }

    #[test]
    fn cancelling_a_scan_already_in_flight_terminates_and_closes_every_reader() {
        // Wide and deep enough that the bounded result queue is genuinely in
        // use when the cancel lands: this is the shape that used to hang on
        // join, because a worker was parked in `result_tx.send()`.
        let dir = tempfile::tempdir().expect("tempdir");
        for outer in 0..40_u32 {
            let branch = dir.path().join(format!("d{outer:03}"));
            std::fs::create_dir_all(&branch).expect("mkdir");
            for inner in 0..40_u32 {
                std::fs::create_dir_all(branch.join(format!("s{inner:03}"))).expect("mkdir");
                std::fs::write(branch.join(format!("f{inner:03}.bin")), b"x").expect("write");
            }
        }

        let cancel = Arc::new(CancelToken::new());
        let counters = Arc::new(ProgressCounters::new());
        let trigger = Arc::clone(&cancel);
        let watch = Arc::clone(&counters);
        let canceller = std::thread::spawn(move || {
            // Cancel once the builder is demonstrably mid-flight.
            for _ in 0..2_000 {
                if watch.observed_entries.load(Ordering::Relaxed) > 50 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            trigger.request();
        });

        let outcome = run(ScanRequest {
            root: dir.path().to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::clone(&cancel),
            counters,
        });
        drop(canceller.join());

        assert!(
            matches!(outcome, ScanOutcome::Cancelled),
            "a cancelled scan must publish nothing"
        );
        assert!(cancel.resources_closed(), "every reader handle must be accounted for");
    }

    #[test]
    fn a_missing_root_is_fatal_and_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = scan(&dir.path().join("does-not-exist"), ScanOptions::default());
        let ScanOutcome::Failed(error) = outcome else {
            panic!("expected a fatal failure");
        };
        assert!(matches!(error, ScanError::RootUnavailable { .. }));
        assert!(error.is_fatal());
    }

    #[test]
    fn aggregation_omits_nodes_but_keeps_their_bytes() {
        let dir = fixture();
        let mut options = options();
        options.aggregate_below_bytes = Some(10_000);
        let ScanOutcome::Completed(scan) = scan(dir.path(), options) else {
            panic!("expected a completed scan");
        };
        assert!(scan.is_aggregated());
        assert!(scan.is_partial());
        assert!(scan.counts.aggregated_nodes >= 2, "top.txt and one.bin are below 10 kB");
        assert_eq!(
            scan.totals.logical,
            100 + 2_000 + 30_000 + link_len(dir.path()),
            "aggregation must not lose bytes"
        );
        assert!(scan.counts.retained_nodes < scan.counts.observed_entries);
    }

    #[test]
    fn an_exclusion_skips_a_subtree_and_is_reported() {
        let dir = fixture();
        let mut options = options();
        options.apply_default_exclusions = false;
        options.exclusions = vec![rdirstat_core::ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::DirectoryName,
            syntax: RuleSyntax::Glob,
            pattern: "a".to_owned(),
            case_sensitive: true,
        }];
        let ScanOutcome::Completed(scan) = scan(dir.path(), options) else {
            panic!("expected a completed scan");
        };
        assert_eq!(scan.counts.excluded_paths, 1);
        assert_eq!(scan.counts.directories, 1, "only the root was read");
        assert_eq!(scan.totals.logical, 100 + link_len(dir.path()));
        assert!(!scan.excluded_roots.is_empty());
    }

    #[test]
    fn hard_links_are_counted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        std::fs::write(&original, vec![b'q'; 4_096]).expect("write");
        std::fs::hard_link(&original, dir.path().join("copy.bin")).expect("hard link");

        let ScanOutcome::Completed(scan) = scan(dir.path(), ScanOptions::default()) else {
            panic!("expected a completed scan");
        };
        assert_eq!(scan.totals.logical, 4_096, "the second link contributes zero bytes");
        assert_eq!(scan.counts.hard_link_repeats, 1);
        assert_eq!(scan.counts.files, 2, "but both entries stay visible");
    }

    #[test]
    fn hard_links_are_counted_twice_when_the_policy_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        std::fs::write(&original, vec![b'q'; 4_096]).expect("write");
        std::fs::hard_link(&original, dir.path().join("copy.bin")).expect("hard link");

        let mut options = options();
        options.count_hard_links_once = false;
        let ScanOutcome::Completed(scan) = scan(dir.path(), options) else {
            panic!("expected a completed scan");
        };
        assert_eq!(scan.totals.logical, 8_192);
        assert_eq!(scan.counts.hard_link_repeats, 0);
    }

    #[test]
    fn an_unreadable_directory_is_recorded_and_the_scan_still_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::write(locked.join("secret"), b"x").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let outcome = scan(dir.path(), ScanOptions::default());
        // Restore before any assertion can unwind past the cleanup.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");

        let ScanOutcome::Completed(scan) = outcome else {
            panic!("a permission failure is recorded, never fatal");
        };
        assert!(scan.is_partial());
        assert_eq!(scan.counts.unreadable_dirs, 1);
        assert!(!scan.errors.is_empty());
        assert!(scan.errors.iter().all(|error| !error.is_fatal()));
    }

    #[test]
    fn globs_match_the_documented_subset() {
        assert!(glob_match("*.log", "server.log"));
        assert!(glob_match("System/Volumes/*", "System/Volumes/Data"));
        assert!(glob_match("?ache", "cache"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("*.log", "server.txt"));
        assert!(!glob_match("cache", "MyCacheBackup"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let rules = vec![
            CompiledRule {
                action: RuleAction::Include,
                scope: RuleScope::DirectoryName,
                pattern: "Caches".to_owned(),
                case_sensitive: true,
            },
            CompiledRule {
                action: RuleAction::Exclude,
                scope: RuleScope::DirectoryName,
                pattern: "*".to_owned(),
                case_sensitive: true,
            },
        ];
        assert!(!is_excluded(&rules, "Caches", "Library/Caches"));
        assert!(is_excluded(&rules, "Other", "Library/Other"));
    }

    #[test]
    fn regex_rules_are_refused_at_start_not_mid_scan() {
        let mut options = options();
        options.exclusions = vec![rdirstat_core::ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::RootRelativePath,
            syntax: RuleSyntax::Regex,
            pattern: ".*".to_owned(),
            case_sensitive: true,
        }];
        assert!(compile_rules(&options).is_err());
        assert!(compile_rules(&ScanOptions::default()).is_ok());
    }

    #[test]
    fn the_config_hash_is_deterministic_and_option_sensitive() {
        let base = ScanOptions::default();
        let mut other = ScanOptions::default();
        other.cross_filesystems = true;
        assert_eq!(options_hash(&base), options_hash(&base));
        assert_ne!(options_hash(&base), options_hash(&other));
        assert_eq!(options_hash(&base).as_str().len(), 64);
    }

    #[test]
    fn an_empty_directory_scans_to_an_empty_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ScanOutcome::Completed(scan) = scan(dir.path(), ScanOptions::default()) else {
            panic!("expected a completed scan");
        };
        assert_eq!(scan.totals.logical, 0);
        assert_eq!(scan.tree.len(), 1, "just the root");
        assert_eq!(scan.tree.child_count(scan.root), 0);
        assert!(!scan.is_partial());
    }
}
