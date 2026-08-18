//! Scan diffing: what appeared, what vanished, and what moved the needle.
//!
//! Two frozen arenas for the same root, compared. The snapshot store keeps up
//! to two `*.rdstat` files per scanned root, so "the tree as it was" and "the
//! tree as it is" are both loadable and this module is the comparison between
//! them. It loads nothing and opens nothing: it takes two [`Tree`] values and
//! returns a bounded report.
//!
//! ## The join key is a pair, not a string
//!
//! docs/06-DATA.md pins the join to the exact pair `(parent_path, name)` and
//! explicitly forbids concatenating the two into one key. That is not
//! fastidiousness: `/` is the only byte a macOS filename cannot contain, but a
//! name may contain a newline, a NUL-adjacent escape, or a literal `%`, and any
//! separator chosen for a synthetic key eventually collides with a real name.
//! It is also gratuitously expensive — building 12 million path strings to
//! compare them is more allocation than the entire rest of the app.
//!
//! So the pair is never materialised. The walk descends *corresponding
//! directories in lockstep*: at each step the `parent_path` half of the key is
//! implied by which directory pair is being visited, and only the `name` half
//! is compared, as raw bytes. Two entries join if and only if their parents
//! joined and their name bytes are byte-for-byte equal.
//!
//! Byte-for-byte means **case-sensitive**. A case-only rename is an exact
//! remove plus an exact add, which is what docs/06-DATA.md says it is. No
//! `(device, inode)` second pass, no move detection: inode numbers are reused,
//! so that inference is a heuristic and a diff that guesses is worse than one
//! that reports facts.
//!
//! ## Changes are reported at the shallowest node that owns them
//!
//! A directory that exists on one side only is **one** row, priced from its
//! [`DirTotals`] in constant time. The walk does not descend into it. Deleting
//! `node_modules` is one event — "gone, 1.2 GB, 412,003 entries" — not 412,003
//! events, and enumerating them would be both a hang and a worse answer.
//!
//! A directory that exists on both sides gets **no** row of its own, only a
//! descent. Its subtree delta is by construction the sum of its children's
//! deltas, so recording both would double-count, and a rollup row would always
//! outrank every real change (the scan root is always the biggest mover, which
//! is why "biggest movers first" and "roll directories up" cannot both hold).
//!
//! That gives an exact partition, and it is asserted as a test:
//!
//! ```text
//! added.logical_delta + removed.logical_delta
//!   + grown.logical_delta + shrunk.logical_delta
//!   == after_root.logical - before_root.logical
//! ```
//!
//! ## Logical and allocated are never reconciled
//!
//! Every row carries both deltas and both absolutes. Nothing here adds one to
//! the other or presents a single "size change". [`DiffMetric`] picks which of
//! the two *orders and classifies* the rows — matching the logical/allocated
//! toggle the rest of the interface already has — and the report echoes the
//! choice back so the UI can say which number it sorted by. When the selected
//! metric did not move but the other one did (a file rewritten sparse: same
//! `st_size`, different `st_blocks`), the row is still reported, classified by
//! the metric that moved. Filing a real size change as "unchanged" because the
//! selected column happened not to see it would be a lie of omission.
//!
//! ## Cost
//!
//! Let `C` be the number of entries reachable along the **common** structure —
//! the nodes whose whole ancestor chain exists in both trees — and `f` the
//! sibling fan-out of a directory.
//!
//! - **Time: `O(C · log f)`**, bounded above by `O((|A| + |B|) · log f)` when
//!   the two trees are identical, and far below it in the case that matters.
//!   Each matched directory pair sorts its two sibling lists once and merges
//!   them linearly; each entry is therefore touched exactly once. Added and
//!   removed subtrees cost `O(log D)` each — one binary search of the directory
//!   index for their totals — and are **not** walked, so replacing a 12M-node
//!   tree wholesale is `O(1)` work, not `O(12M)`.
//! - **Memory: `O(w + limit)`**, no term in the node count. Two scratch vectors
//!   hold one directory's children at a time (16 bytes per child, reused across
//!   every directory, so `w` is the widest single sibling set — not the tree).
//!   The explicit stack holds matched directory *pairs*, 8 bytes each, bounded
//!   by the matched-directory count and in practice by depth × sibling width.
//!   The four result buffers are capped at [`MAX_DIFF_ENTRIES`] rows each.
//!
//! The rejected alternative was a `HashMap<&[u8], NodeId>` per directory. It is
//! `O(f)` rather than `O(f log f)`, but it allocates and drops a table 1.2
//! million times per scan pair, hashes every name in both trees, and gives
//! nondeterministic output order for equal-magnitude rows. Sort-merge reuses
//! two buffers for the whole walk, is the literal `FULL OUTER JOIN` the doc
//! describes, and its ordering is reproducible.
//!
//! Traversal is iterative with an explicit stack, never recursive — the same
//! reason every other walk in this crate is: a 4096-deep chain is a real input
//! and a stack overflow is not a diagnosable failure.

use core::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::error::QueryError;
use crate::id::NodeId;
use crate::node::Kind;
use crate::tree::Tree;
use crate::wire::DisplayPath;

/// Hard ceiling on rows in one change list.
///
/// Four lists, so a report is at most `4 × MAX_DIFF_ENTRIES` rows however many
/// entries actually changed. The uncapped counts live in [`DiffSummary`]; the
/// lists are a leaderboard, exactly like
/// [`size_band_entries`](crate::bands::size_band_entries). A full change list
/// between two 12M-node trees is not a payload, it is a file dump.
pub const MAX_DIFF_ENTRIES: usize = 500;

/// Which size orders and classifies the comparison.
///
/// The rows always carry *both* deltas; this only decides which one decides
/// "grown" from "shrunk" and which one sorts the leaderboard. It exists so the
/// diff honours the same logical/allocated toggle the rest of the interface
/// has, rather than silently picking one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DiffMetric {
    /// `st_size` — what the files claim. The default, matching the UI default.
    #[default]
    Logical,
    /// `st_blocks * 512` — what the filesystem spent. On APFS this disagrees
    /// with logical for sparse, compressed, and cloned files.
    Allocated,
}

/// How one entry differs between the two scans.
///
/// There is deliberately no `MetadataChanged` variant. A metadata change has no
/// magnitude, so it cannot be ranked among size movers, and a variant that the
/// producer never emits is a lie in the generated TypeScript union. Metadata
/// changes are counted in [`DiffSummary::metadata_changed`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DiffChange {
    /// Present in the after scan, absent from the before scan.
    Added,
    /// Present in the before scan, absent from the after scan.
    Removed,
    /// Present in both; the selected metric rose.
    Grown,
    /// Present in both; the selected metric fell.
    Shrunk,
}

/// Which tree a [`DiffEntry::node`] indexes.
///
/// Stated rather than inferred from [`DiffChange`]. A removed entry's node id
/// belongs to the *before* tree, which is not the published generation and
/// names a path that no longer exists, so Reveal and Trash must be refused for
/// it. Making the caller derive that from the change kind is exactly the tribal
/// knowledge this codebase does not accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The earlier scan. Its node ids are not addressable by the live commands.
    Before,
    /// The later scan.
    After,
}

/// Which two scans a report compares.
///
/// Supplied by the caller, because a [`Tree`] does not know when it was taken.
/// It is a required part of [`DiffOptions`] rather than an optional decoration:
/// a diff that does not say what it is diffing is unreadable, and making the
/// labels structurally mandatory is cheaper than remembering to attach them.
///
/// [`Default`] is derived for tests and renders as an empty root with no time,
/// which the UI shows as "unknown" — a visible gap rather than a silent one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DiffScanInfo {
    /// The scan root, escaped for display. Never action authority.
    pub root: DisplayPath,
    /// When the scan finished, Unix milliseconds. `None` when unknown.
    pub taken_unix_ms: Option<i64>,
    /// Retained nodes in that scan's arena, so the header can say how much tree
    /// each side is.
    pub nodes: u64,
}

/// Signed totals for one change class.
///
/// The deltas are **signed sums**, not magnitudes, and they are separate per
/// metric. A row classified `Grown` by allocated bytes may carry a negative
/// logical delta; folding that into an absolute value would make the class
/// totals stop summing to the root delta.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DiffClassTotals {
    /// Entries in this class, uncapped — the list beside it is capped.
    pub entries: u64,
    /// Sum of logical deltas over the class, in bytes.
    pub logical_delta: i64,
    /// Sum of allocated deltas over the class, in bytes.
    pub allocated_delta: i64,
}

impl DiffClassTotals {
    /// An empty class.
    pub const ZERO: Self = Self {
        entries: 0,
        logical_delta: 0,
        allocated_delta: 0,
    };
}

/// Everything the comparison found, in counts. Never truncated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DiffSummary {
    /// Entries present only in the after scan.
    pub added: DiffClassTotals,
    /// Entries present only in the before scan.
    pub removed: DiffClassTotals,
    /// Entries in both whose selected metric rose.
    pub grown: DiffClassTotals,
    /// Entries in both whose selected metric fell.
    pub shrunk: DiffClassTotals,
    /// Entries in both with identical logical *and* allocated bytes, but a
    /// different mtime, kind, or flag set. Counted, not listed: see
    /// [`DiffChange`].
    pub metadata_changed: u64,
    /// Entries in both whose [`Kind`] differs — a file replaced by a directory
    /// of the same name, or a path that became a symlink. Overlaps the other
    /// counters on purpose: a kind change usually also changes size, and it is
    /// worth surfacing either way.
    pub kind_changed: u64,
    /// Entries in both with nothing at all to report.
    pub unchanged: u64,
    /// `(parent, name)` keys examined — added plus removed plus common.
    pub compared: u64,
    /// Directory pairs the walk descended into.
    pub descended: u64,
    /// Logical bytes at the before root.
    pub before_logical: u64,
    /// Allocated bytes at the before root.
    pub before_allocated: u64,
    /// Logical bytes at the after root.
    pub after_logical: u64,
    /// Allocated bytes at the after root.
    pub after_allocated: u64,
    /// `after_logical - before_logical`. The four class `logical_delta` values
    /// sum to exactly this.
    pub logical_delta: i64,
    /// `after_allocated - before_allocated`, on the same basis.
    pub allocated_delta: i64,
    /// The walk hit its traversal budget and stopped early, which only happens
    /// on an arena that escaped [`Tree::validate`]. The counts are a floor.
    pub truncated: bool,
}

/// One change, resolved for display.
///
/// Paths are reconstructed only for rows that survive the cap, which is what
/// keeps the report `O(limit)` in string work rather than `O(changes)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DiffEntry {
    /// How this entry differs.
    pub change: DiffChange,
    /// Which tree [`DiffEntry::node`] indexes.
    pub side: DiffSide,
    /// The arena node, in the tree named by [`DiffEntry::side`].
    pub node: NodeId,
    /// Full path, escaped for display.
    pub path: DisplayPath,
    /// Kind on the side this row is addressed to.
    pub kind: Kind,
    /// Whether the kind differs between the two scans.
    pub kind_changed: bool,
    /// Filesystem entries this one row covers, including the node itself. `1`
    /// for a leaf; the whole subtree for an added or removed directory.
    pub entries: u64,
    /// Logical bytes before. Zero for an added entry.
    pub before_logical: u64,
    /// Allocated bytes before. Zero for an added entry.
    pub before_allocated: u64,
    /// Logical bytes after. Zero for a removed entry.
    pub after_logical: u64,
    /// Allocated bytes after. Zero for a removed entry.
    pub after_allocated: u64,
    /// `after_logical - before_logical`, signed.
    pub logical_delta: i64,
    /// `after_allocated - before_allocated`, signed. Never summed with the
    /// logical delta.
    pub allocated_delta: i64,
    /// Modification time in the before scan, whole Unix seconds.
    pub before_mtime: Option<i64>,
    /// Modification time in the after scan, whole Unix seconds.
    pub after_mtime: Option<i64>,
    /// Content category index, from the side this row is addressed to.
    pub category: u8,
}

/// The whole comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DiffReport {
    /// The earlier scan.
    pub before: DiffScanInfo,
    /// The later scan.
    pub after: DiffScanInfo,
    /// Which size ordered and classified the rows.
    pub metric: DiffMetric,
    /// The per-list cap actually applied, after clamping to
    /// [`MAX_DIFF_ENTRIES`]. The UI compares it against the summary counts to
    /// say "showing 500 of 41,220".
    pub limit: u32,
    /// Uncapped counts and totals.
    pub summary: DiffSummary,
    /// Biggest additions first.
    pub added: Vec<DiffEntry>,
    /// Biggest removals first.
    pub removed: Vec<DiffEntry>,
    /// Biggest growth first.
    pub grown: Vec<DiffEntry>,
    /// Biggest shrinkage first.
    pub shrunk: Vec<DiffEntry>,
}

/// What to compare, and how to rank it.
///
/// Deliberately **not** a wire type. The two [`DiffScanInfo`] headers are
/// backend facts — they come from the snapshot store, which is the only thing
/// that knows when a `*.rdstat` file was written — and a serializable options
/// struct invites a frontend to supply them, which is how a diff ends up
/// mislabelled. The command exposes `metric` and `limit` to the frontend and
/// fills the headers itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffOptions {
    /// Identity of the earlier scan.
    pub before: DiffScanInfo,
    /// Identity of the later scan.
    pub after: DiffScanInfo,
    /// Which size orders and classifies.
    pub metric: DiffMetric,
    /// Rows per change list, clamped to [`MAX_DIFF_ENTRIES`].
    pub limit: usize,
}

impl DiffOptions {
    /// Options naming both scans, ranking by logical bytes, capped at
    /// [`MAX_DIFF_ENTRIES`].
    ///
    /// There is no `Default`: a default `limit` of zero would produce a report
    /// with correct counts and no rows, which looks like "nothing changed".
    #[must_use]
    pub fn new(before: DiffScanInfo, after: DiffScanInfo) -> Self {
        Self {
            before,
            after,
            metric: DiffMetric::Logical,
            limit: MAX_DIFF_ENTRIES,
        }
    }

    /// Replaces the ranking metric.
    #[must_use]
    pub fn with_metric(mut self, metric: DiffMetric) -> Self {
        self.metric = metric;
        self
    }

    /// Replaces the per-list cap.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// Compares the subtree at `before_root` in `before` against the subtree at
/// `after_root` in `after`.
///
/// The two roots are *declared* to correspond; their own names are not
/// compared, so a scan of `/Volumes/data` diffed against a scan of the same
/// volume mounted elsewhere still lines up. Everything below them joins on the
/// exact `(parent, name)` pair, case-sensitively.
///
/// See the module docs for the algorithm and its cost. In one line: a lockstep
/// sort-merge descent of the common structure that never walks into an added or
/// removed subtree.
///
/// # Errors
///
/// [`QueryError::UnknownNode`] naming the root that is not a node in its own
/// tree. Both roots are commonly [`NodeId::ROOT`], so the id in the error does
/// not always identify the side; this is a programming error rather than a
/// user-facing one, because both trees have already passed
/// [`Tree::validate`] by the time a caller has them.
pub fn diff_trees(
    before: &Tree,
    before_root: NodeId,
    after: &Tree,
    after_root: NodeId,
    options: DiffOptions,
) -> Result<DiffReport, QueryError> {
    let before_start = resolve_root(before, before_root)?;
    let after_start = resolve_root(after, after_root)?;

    let limit = options.limit.min(MAX_DIFF_ENTRIES);
    let mut state = DiffState::new(options.metric, limit);

    state.summary.before_logical = before.logical_of(before_start);
    state.summary.before_allocated = before.allocated_of(before_start);
    state.summary.after_logical = after.logical_of(after_start);
    state.summary.after_allocated = after.allocated_of(after_start);
    state.summary.logical_delta = signed_delta(state.summary.before_logical, state.summary.after_logical);
    state.summary.allocated_delta = signed_delta(state.summary.before_allocated, state.summary.after_allocated);

    if is_directory(before, before_start) && is_directory(after, after_start) {
        state.walk(before, before_start, after, after_start);
    } else {
        // Two leaves, or a root whose kind flipped. There are no children to
        // join, but the pair itself still changed, and dropping it would break
        // the "class deltas sum to the root delta" partition.
        state.compare_pair(before, before_start, after, after_start);
    }

    Ok(state.into_report(before, after, options))
}

/// Resolves a requested root to the node the walk actually starts from.
fn resolve_root(tree: &Tree, id: NodeId) -> Result<NodeId, QueryError> {
    // A virtual `<Files>` group is a view over one directory's leaves, not a
    // subtree, so "diff this group" is not a defined question. Resolve to the
    // owning directory, the way every other subtree query in this crate does,
    // rather than erroring on a request that has an obvious sane reading.
    let start = id.group_owner().unwrap_or(id);
    if tree.node(start).is_none() {
        return Err(QueryError::UnknownNode { node: id });
    }
    Ok(start)
}

fn is_directory(tree: &Tree, id: NodeId) -> bool {
    tree.node(id).is_some_and(|node| node.kind().is_directory())
}

/// `after - before` as a signed byte count.
///
/// Byte counts on any real volume are far below `i64::MAX`, but a corrupt
/// snapshot can claim `u64::MAX`, so both sides clamp and the subtraction
/// saturates instead of wrapping into a plausible-looking small number.
fn signed_delta(before: u64, after: u64) -> i64 {
    let before = i64::try_from(before).unwrap_or(i64::MAX);
    let after = i64::try_from(after).unwrap_or(i64::MAX);
    after.saturating_sub(before)
}

/// Entries this node stands for, including itself.
fn subtree_entries(tree: &Tree, id: NodeId) -> u64 {
    // `retained_nodes` counts what is *inside* a directory, so the directory
    // itself is added back: "142 entries appeared" should include the folder
    // the user is about to go looking for.
    tree.dir_totals(id)
        .map_or(1, |totals| u64::from(totals.retained_nodes).saturating_add(1))
}

/// One change, before its path has been reconstructed.
///
/// Deliberately path-free. Millions of these may be offered to the leaderboards
/// and only `4 × limit` survive, so building a [`DisplayPath`] here would be
/// the dominant cost of the whole comparison.
#[derive(Clone, Debug)]
struct Pending {
    node: NodeId,
    magnitude: u64,
    logical_delta: i64,
    allocated_delta: i64,
    before_logical: u64,
    before_allocated: u64,
    after_logical: u64,
    after_allocated: u64,
    before_mtime: Option<i64>,
    after_mtime: Option<i64>,
    entries: u64,
    kind: Kind,
    kind_changed: bool,
    category: u8,
}

/// A bounded, magnitude-ranked leaderboard that also keeps exact class totals.
///
/// The totals are accumulated on every offer and the rows are not, which is the
/// whole point: the summary is always the truth about how much changed, even
/// when the list beside it shows the top five hundred of forty thousand.
#[derive(Debug)]
struct TopN {
    limit: usize,
    best: Vec<Pending>,
    floor: u64,
    totals: DiffClassTotals,
}

impl TopN {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            best: Vec::with_capacity(limit.min(1024)),
            floor: 0,
            totals: DiffClassTotals::ZERO,
        }
    }

    fn offer(&mut self, item: Pending) {
        self.totals.entries = self.totals.entries.saturating_add(1);
        self.totals.logical_delta = self.totals.logical_delta.saturating_add(item.logical_delta);
        self.totals.allocated_delta = self.totals.allocated_delta.saturating_add(item.allocated_delta);

        if self.limit == 0 {
            return;
        }
        if self.best.len() < self.limit || item.magnitude > self.floor {
            self.best.push(item);
            // Sorting on every insertion past the cap keeps the vector bounded
            // without a heap; `limit` is in the hundreds, so this is cheaper
            // than it looks and never allocates unboundedly. Same trade-off as
            // `bands::size_band_entries`.
            if self.best.len() > self.limit {
                Self::rank(&mut self.best);
                self.best.truncate(self.limit);
                self.floor = self.best.last().map_or(0, |entry| entry.magnitude);
            }
        }
    }

    fn rank(rows: &mut [Pending]) {
        // Node id breaks ties so the output is reproducible across runs; two
        // files that changed by the same number of bytes must not swap places
        // between two identical requests.
        rows.sort_unstable_by(|a, b| b.magnitude.cmp(&a.magnitude).then_with(|| a.node.cmp(&b.node)));
    }

    /// Ranks, truncates, and resolves paths for the survivors.
    fn finish(
        mut self,
        tree: &Tree,
        change: DiffChange,
        side: DiffSide,
        scratch: &mut Vec<u8>,
    ) -> (DiffClassTotals, Vec<DiffEntry>) {
        Self::rank(&mut self.best);
        self.best.truncate(self.limit);

        let mut rows = Vec::with_capacity(self.best.len());
        for pending in self.best {
            scratch.clear();
            // A path that cannot be reconstructed is skipped rather than shown
            // blank: a row naming no file is worse than a shorter list. Same
            // rule as `bands::size_band_entries`.
            if tree.path_bytes(pending.node, scratch).is_err() {
                continue;
            }
            rows.push(DiffEntry {
                change,
                side,
                node: pending.node,
                path: DisplayPath::from_bytes(scratch),
                kind: pending.kind,
                kind_changed: pending.kind_changed,
                entries: pending.entries,
                before_logical: pending.before_logical,
                before_allocated: pending.before_allocated,
                after_logical: pending.after_logical,
                after_allocated: pending.after_allocated,
                logical_delta: pending.logical_delta,
                allocated_delta: pending.allocated_delta,
                before_mtime: pending.before_mtime,
                after_mtime: pending.after_mtime,
                category: pending.category,
            });
        }
        (self.totals, rows)
    }
}

/// The four leaderboards plus the running summary.
#[derive(Debug)]
struct DiffState {
    metric: DiffMetric,
    summary: DiffSummary,
    added: TopN,
    removed: TopN,
    grown: TopN,
    shrunk: TopN,
}

impl DiffState {
    fn new(metric: DiffMetric, limit: usize) -> Self {
        Self {
            metric,
            summary: DiffSummary::default(),
            added: TopN::new(limit),
            removed: TopN::new(limit),
            grown: TopN::new(limit),
            shrunk: TopN::new(limit),
        }
    }

    /// Orders `(selected, other)` for the configured metric.
    const fn order(&self, logical: i64, allocated: i64) -> (i64, i64) {
        match self.metric {
            DiffMetric::Logical => (logical, allocated),
            DiffMetric::Allocated => (allocated, logical),
        }
    }

    /// The lockstep descent. Iterative, explicit stack, never recursion.
    fn walk<'t>(&mut self, before: &'t Tree, before_root: NodeId, after: &'t Tree, after_root: NodeId) {
        let mut stack = vec![(before_root, after_root)];
        // Reused for every directory pair, so the per-directory cost is a sort
        // of a short slice and not an allocation.
        let mut left: Vec<(&'t [u8], NodeId)> = Vec::new();
        let mut right: Vec<(&'t [u8], NodeId)> = Vec::new();

        // Both arenas are finite and acyclic by `Tree`'s own freeze-time
        // validation, so this bound is a backstop against a tree that somehow
        // escaped it rather than an expected limit.
        let mut budget = before.len().saturating_add(after.len()).saturating_add(16);

        while let Some((old_dir, new_dir)) = stack.pop() {
            if budget == 0 {
                self.summary.truncated = true;
                break;
            }
            budget -= 1;
            self.summary.descended = self.summary.descended.saturating_add(1);

            collect_children(before, old_dir, &mut left);
            collect_children(after, new_dir, &mut right);
            self.join(before, &left, after, &right, &mut stack);
        }
    }

    /// The `FULL OUTER JOIN` itself: two name-sorted sibling lists, merged.
    fn join(
        &mut self,
        before: &Tree,
        left: &[(&[u8], NodeId)],
        after: &Tree,
        right: &[(&[u8], NodeId)],
        stack: &mut Vec<(NodeId, NodeId)>,
    ) {
        let mut i = 0;
        let mut j = 0;
        loop {
            match (left.get(i), right.get(j)) {
                (Some(old), Some(new)) => match old.0.cmp(new.0) {
                    // Byte order, so the comparison is case-sensitive and a
                    // case-only rename falls out as an exact remove plus add.
                    Ordering::Less => {
                        self.removed(before, old.1);
                        i += 1;
                    }
                    Ordering::Greater => {
                        self.added(after, new.1);
                        j += 1;
                    }
                    Ordering::Equal => {
                        self.common(before, old.1, after, new.1, stack);
                        i += 1;
                        j += 1;
                    }
                },
                (Some(old), None) => {
                    self.removed(before, old.1);
                    i += 1;
                }
                (None, Some(new)) => {
                    self.added(after, new.1);
                    j += 1;
                }
                (None, None) => return,
            }
        }
    }

    fn added(&mut self, after: &Tree, new: NodeId) {
        self.summary.compared = self.summary.compared.saturating_add(1);
        let logical = after.logical_of(new);
        let allocated = after.allocated_of(new);
        let node = after.node(new);
        let (selected, other) = self.order(signed_delta(0, logical), signed_delta(0, allocated));
        self.added.offer(Pending {
            node: new,
            magnitude: magnitude_of(selected, other),
            logical_delta: signed_delta(0, logical),
            allocated_delta: signed_delta(0, allocated),
            before_logical: 0,
            before_allocated: 0,
            after_logical: logical,
            after_allocated: allocated,
            before_mtime: None,
            after_mtime: node.map(|node| node.mtime),
            entries: subtree_entries(after, new),
            kind: node.map_or(Kind::Unknown, |node| node.kind()),
            kind_changed: false,
            category: node.map_or(0, |node| node.category),
        });
    }

    fn removed(&mut self, before: &Tree, old: NodeId) {
        self.summary.compared = self.summary.compared.saturating_add(1);
        let logical = before.logical_of(old);
        let allocated = before.allocated_of(old);
        let node = before.node(old);
        let (selected, other) = self.order(signed_delta(logical, 0), signed_delta(allocated, 0));
        self.removed.offer(Pending {
            node: old,
            magnitude: magnitude_of(selected, other),
            logical_delta: signed_delta(logical, 0),
            allocated_delta: signed_delta(allocated, 0),
            before_logical: logical,
            before_allocated: allocated,
            after_logical: 0,
            after_allocated: 0,
            before_mtime: node.map(|node| node.mtime),
            after_mtime: None,
            entries: subtree_entries(before, old),
            kind: node.map_or(Kind::Unknown, |node| node.kind()),
            kind_changed: false,
            category: node.map_or(0, |node| node.category),
        });
    }

    fn common(&mut self, before: &Tree, old: NodeId, after: &Tree, new: NodeId, stack: &mut Vec<(NodeId, NodeId)>) {
        self.summary.compared = self.summary.compared.saturating_add(1);
        if is_directory(before, old) && is_directory(after, new) {
            // No row of its own: a directory's subtree delta is the sum of its
            // children's, and reporting both would double-count. Its own mtime
            // is skipped for the same reason — it moves whenever any child is
            // created or deleted, so it restates a change recorded elsewhere.
            stack.push((old, new));
            return;
        }
        self.compare_pair(before, old, after, new);
    }

    /// Classifies one matched pair that the walk will not descend into.
    fn compare_pair(&mut self, before: &Tree, old: NodeId, after: &Tree, new: NodeId) {
        let before_logical = before.logical_of(old);
        let before_allocated = before.allocated_of(old);
        let after_logical = after.logical_of(new);
        let after_allocated = after.allocated_of(new);
        let logical_delta = signed_delta(before_logical, after_logical);
        let allocated_delta = signed_delta(before_allocated, after_allocated);

        let old_node = before.node(old);
        let new_node = after.node(new);
        let old_kind = old_node.map_or(Kind::Unknown, |node| node.kind());
        let new_kind = new_node.map_or(Kind::Unknown, |node| node.kind());
        let kind_changed = old_kind != new_kind;
        if kind_changed {
            self.summary.kind_changed = self.summary.kind_changed.saturating_add(1);
        }

        let (selected, other) = self.order(logical_delta, allocated_delta);
        // The selected metric decides the direction; the other one is consulted
        // only when the selected one did not move. A file rewritten sparse has
        // the same `st_size` and fewer blocks, and filing that as "unchanged"
        // because the logical column happened not to see it would be a lie.
        let direction = if selected == 0 {
            other.signum()
        } else {
            selected.signum()
        };

        if direction == 0 {
            let touched = kind_changed
                || old_node.map(|node| node.mtime) != new_node.map(|node| node.mtime)
                || old_node.map(|node| node.flags) != new_node.map(|node| node.flags);
            if touched {
                self.summary.metadata_changed = self.summary.metadata_changed.saturating_add(1);
            } else {
                self.summary.unchanged = self.summary.unchanged.saturating_add(1);
            }
            return;
        }

        let pending = Pending {
            node: new,
            magnitude: magnitude_of(selected, other),
            logical_delta,
            allocated_delta,
            before_logical,
            before_allocated,
            after_logical,
            after_allocated,
            before_mtime: old_node.map(|node| node.mtime),
            after_mtime: new_node.map(|node| node.mtime),
            entries: subtree_entries(after, new),
            kind: new_kind,
            kind_changed,
            category: new_node.map_or(0, |node| node.category),
        };
        if direction > 0 {
            self.grown.offer(pending);
        } else {
            self.shrunk.offer(pending);
        }
    }

    fn into_report(mut self, before: &Tree, after: &Tree, options: DiffOptions) -> DiffReport {
        let mut scratch = Vec::new();
        let (added_totals, added) = self
            .added
            .finish(after, DiffChange::Added, DiffSide::After, &mut scratch);
        let (removed_totals, removed) =
            self.removed
                .finish(before, DiffChange::Removed, DiffSide::Before, &mut scratch);
        let (grown_totals, grown) = self
            .grown
            .finish(after, DiffChange::Grown, DiffSide::After, &mut scratch);
        let (shrunk_totals, shrunk) = self
            .shrunk
            .finish(after, DiffChange::Shrunk, DiffSide::After, &mut scratch);

        self.summary.added = added_totals;
        self.summary.removed = removed_totals;
        self.summary.grown = grown_totals;
        self.summary.shrunk = shrunk_totals;

        DiffReport {
            before: options.before,
            after: options.after,
            metric: options.metric,
            limit: u32::try_from(options.limit.min(MAX_DIFF_ENTRIES)).unwrap_or(u32::MAX),
            summary: self.summary,
            added,
            removed,
            grown,
            shrunk,
        }
    }
}

/// Ranking magnitude: the selected metric, or the other one when the selected
/// metric did not move.
fn magnitude_of(selected: i64, other: i64) -> u64 {
    if selected == 0 {
        other.unsigned_abs()
    } else {
        selected.unsigned_abs()
    }
}

/// Fills `out` with one directory's children, keyed by raw name bytes and
/// sorted for the merge.
fn collect_children<'t>(tree: &'t Tree, dir: NodeId, out: &mut Vec<(&'t [u8], NodeId)>) {
    out.clear();
    for child in tree.children(dir) {
        // A name that does not resolve means the arena is corrupt. The child is
        // dropped rather than joined under an empty key, which would make every
        // unresolvable name in one tree match every unresolvable name in the
        // other and invent changes that do not exist.
        if let Some(name) = tree.name_bytes(child) {
            out.push((name, child));
        }
    }
    // Node id is the tie-break so that a corrupt arena holding two children
    // with the same name pairs them deterministically instead of by whichever
    // order the sort happened to leave them in.
    out.sort_unstable_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;
    use crate::dirs::DirTotals;
    use crate::node::Node;
    use crate::tree::TreeBuilder;

    /// A tiny tree builder, so a test reads as the shape it is asserting on.
    struct Fixture {
        builder: TreeBuilder,
        root: NodeId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut builder = TreeBuilder::new();
            let name = builder.intern(b"root").expect("interns");
            let root = builder.push_node(Node::directory(name, 0)).expect("pushes");
            builder.register_directory(root, DirTotals::EMPTY).expect("registers");
            Self { builder, root }
        }

        fn dir(&mut self, parent: NodeId, name: &[u8]) -> NodeId {
            let reference = self.builder.intern(name).expect("interns");
            let id = self
                .builder
                .push_child(parent, Node::directory(reference, 0))
                .expect("links");
            self.builder
                .register_directory(id, DirTotals::EMPTY)
                .expect("registers");
            if let Some(totals) = self.builder.dir_totals_mut(parent) {
                totals.observed_entries = totals.observed_entries.saturating_add(1);
            }
            id
        }

        fn file(&mut self, parent: NodeId, name: &[u8], logical: u64, allocated: u64, mtime: i64) -> NodeId {
            let reference = self.builder.intern(name).expect("interns");
            let node = Node::leaf(reference, Kind::File, logical, allocated, mtime);
            let id = self.builder.push_child(parent, node).expect("links");
            if let Some(totals) = self.builder.dir_totals_mut(parent) {
                totals.absorb_direct_file(node.contributed_size(), node.contributed_alloc(), mtime);
                totals.observed_entries = totals.observed_entries.saturating_add(1);
                totals.retained_nodes = totals.retained_nodes.saturating_add(1);
            }
            id
        }

        fn finish(mut self) -> (Tree, NodeId) {
            self.builder.rollup().expect("rolls up");
            let root = self.root;
            (self.builder.finish().expect("valid"), root)
        }
    }

    fn options() -> DiffOptions {
        DiffOptions::new(
            DiffScanInfo {
                root: DisplayPath::from_bytes(b"/root"),
                taken_unix_ms: Some(1_000),
                nodes: 0,
            },
            DiffScanInfo {
                root: DisplayPath::from_bytes(b"/root"),
                taken_unix_ms: Some(2_000),
                nodes: 0,
            },
        )
    }

    /// ```text
    /// before                              after
    ///   keep.txt     1000 / 4096            keep.txt     1000 / 4096
    ///   grow.bin     1000 / 4096            grow.bin     9000 / 12288
    ///   shrink.bin   5000 / 8192            shrink.bin   1000 / 4096
    ///   gone.bin     2000 / 4096            new.bin      4000 / 8192
    ///   touched.txt   700 / 4096  mtime 100 touched.txt   700 / 4096  mtime 200
    ///   CASE.txt       50 / 4096            case.txt       50 / 4096
    ///   sub/                                sub/
    ///     inner.bin  3000 / 4096              inner.bin  3000 / 4096
    ///   old/                                fresh/
    ///     a.bin      9000 / 12288             b.bin      6000 / 8192
    /// ```
    fn pair() -> (Tree, NodeId, Tree, NodeId) {
        let mut old = Fixture::new();
        let old_root = old.root;
        old.file(old_root, b"keep.txt", 1_000, 4_096, 10);
        old.file(old_root, b"grow.bin", 1_000, 4_096, 10);
        old.file(old_root, b"shrink.bin", 5_000, 8_192, 10);
        old.file(old_root, b"gone.bin", 2_000, 4_096, 10);
        old.file(old_root, b"touched.txt", 700, 4_096, 100);
        old.file(old_root, b"CASE.txt", 50, 4_096, 10);
        let old_sub = old.dir(old_root, b"sub");
        old.file(old_sub, b"inner.bin", 3_000, 4_096, 10);
        let old_only = old.dir(old_root, b"old");
        old.file(old_only, b"a.bin", 9_000, 12_288, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        new.file(new_root, b"keep.txt", 1_000, 4_096, 10);
        new.file(new_root, b"grow.bin", 9_000, 12_288, 20);
        new.file(new_root, b"shrink.bin", 1_000, 4_096, 20);
        new.file(new_root, b"new.bin", 4_000, 8_192, 20);
        new.file(new_root, b"touched.txt", 700, 4_096, 200);
        new.file(new_root, b"case.txt", 50, 4_096, 10);
        let new_sub = new.dir(new_root, b"sub");
        new.file(new_sub, b"inner.bin", 3_000, 4_096, 10);
        let new_only = new.dir(new_root, b"fresh");
        new.file(new_only, b"b.bin", 6_000, 8_192, 20);
        let (after, after_root) = new.finish();

        (before, before_root, after, after_root)
    }

    fn report() -> DiffReport {
        let (before, before_root, after, after_root) = pair();
        diff_trees(&before, before_root, &after, after_root, options()).expect("both roots exist")
    }

    fn paths(rows: &[DiffEntry]) -> Vec<&str> {
        rows.iter().map(|row| row.path.as_str()).collect()
    }

    #[test]
    fn an_entry_only_in_the_after_scan_is_added_and_carries_its_whole_size() {
        let report = report();
        assert_eq!(report.summary.added.entries, 3, "new.bin, case.txt, fresh/");
        assert_eq!(report.summary.added.logical_delta, 4_000 + 50 + 6_000);
        assert_eq!(report.summary.added.allocated_delta, 8_192 + 4_096 + 8_192);

        let added = paths(&report.added);
        assert!(added.contains(&"root/new.bin"), "{added:?}");
        assert!(added.contains(&"root/fresh"), "{added:?}");
        for row in &report.added {
            assert_eq!(row.change, DiffChange::Added);
            assert_eq!(row.side, DiffSide::After, "an added node lives in the after tree");
            assert_eq!(row.before_logical, 0);
            assert!(row.logical_delta > 0);
        }
    }

    #[test]
    fn an_entry_only_in_the_before_scan_is_removed_and_points_at_the_before_tree() {
        let report = report();
        assert_eq!(report.summary.removed.entries, 3, "gone.bin, CASE.txt, old/");
        assert_eq!(report.summary.removed.logical_delta, -(2_000 + 50 + 9_000));

        let removed = paths(&report.removed);
        assert!(removed.contains(&"root/gone.bin"), "{removed:?}");
        assert!(removed.contains(&"root/old"), "{removed:?}");
        for row in &report.removed {
            assert_eq!(row.change, DiffChange::Removed);
            // The node id names the *before* arena, which is not the published
            // generation; the UI must refuse Reveal and Trash on these.
            assert_eq!(row.side, DiffSide::Before);
            assert_eq!(row.after_logical, 0);
            assert!(row.logical_delta < 0);
        }
    }

    #[test]
    fn a_common_file_that_gained_bytes_is_grown_and_one_that_lost_them_is_shrunk() {
        let report = report();

        assert_eq!(report.summary.grown.entries, 1);
        assert_eq!(report.summary.grown.logical_delta, 8_000);
        let grown = report.grown.first().expect("grow.bin grew");
        assert_eq!(grown.path.as_str(), "root/grow.bin");
        assert_eq!(grown.before_logical, 1_000);
        assert_eq!(grown.after_logical, 9_000);
        assert_eq!(grown.logical_delta, 8_000);
        assert_eq!(grown.allocated_delta, 8_192, "the two metrics are reported separately");

        assert_eq!(report.summary.shrunk.entries, 1);
        assert_eq!(report.summary.shrunk.logical_delta, -4_000);
        let shrunk = report.shrunk.first().expect("shrink.bin shrank");
        assert_eq!(shrunk.path.as_str(), "root/shrink.bin");
        assert_eq!(shrunk.logical_delta, -4_000);
    }

    #[test]
    fn a_case_only_rename_is_an_exact_remove_and_add_never_a_move() {
        let report = report();
        // docs/06-DATA.md: case-only path changes are exact remove+add events.
        // Inferring a rename would need (device, inode), and inode reuse makes
        // that a guess.
        assert!(paths(&report.removed).contains(&"root/CASE.txt"));
        assert!(paths(&report.added).contains(&"root/case.txt"));
    }

    #[test]
    fn the_four_class_deltas_sum_to_the_root_delta_exactly() {
        let report = report();
        let summary = report.summary;

        let logical = summary.added.logical_delta
            + summary.removed.logical_delta
            + summary.grown.logical_delta
            + summary.shrunk.logical_delta;
        assert_eq!(
            logical, summary.logical_delta,
            "every changed byte must be attributed exactly once"
        );

        let allocated = summary.added.allocated_delta
            + summary.removed.allocated_delta
            + summary.grown.allocated_delta
            + summary.shrunk.allocated_delta;
        assert_eq!(allocated, summary.allocated_delta);

        // And the root delta is what the two arenas actually say it is.
        assert_eq!(summary.before_logical, 21_750);
        assert_eq!(summary.after_logical, 24_750);
        assert_eq!(summary.logical_delta, 3_000);
    }

    #[test]
    fn an_added_directory_is_one_row_and_its_contents_are_not_enumerated() {
        let report = report();
        let fresh = report
            .added
            .iter()
            .find(|row| row.path.as_str() == "root/fresh")
            .expect("fresh/ is new");
        assert_eq!(fresh.kind, Kind::Directory);
        assert_eq!(fresh.logical_delta, 6_000, "priced from DirTotals, not by walking");
        assert_eq!(fresh.entries, 2, "the directory plus b.bin");
        assert!(
            !paths(&report.added).contains(&"root/fresh/b.bin"),
            "the walk must not descend into an added subtree"
        );
    }

    #[test]
    fn a_directory_present_in_both_scans_gets_no_row_of_its_own() {
        let report = report();
        // `sub/` is unchanged and `root/` obviously changed, but neither may
        // appear: a matched directory's delta is its children's, and a rollup
        // row would outrank every real change.
        assert!(!paths(&report.grown).contains(&"root/sub"));
        assert!(!paths(&report.grown).contains(&"root"));
        assert!(!paths(&report.shrunk).contains(&"root"));
        assert_eq!(report.summary.descended, 2, "root/ and sub/");
    }

    #[test]
    fn the_same_bytes_with_a_new_mtime_is_metadata_changed_not_grown() {
        let report = report();
        assert_eq!(report.summary.metadata_changed, 1, "touched.txt");
        assert_eq!(report.summary.unchanged, 2, "keep.txt and sub/inner.bin");
        assert!(!paths(&report.grown).contains(&"root/touched.txt"));
        assert!(!paths(&report.shrunk).contains(&"root/touched.txt"));
    }

    #[test]
    fn identical_trees_report_no_changes_at_all() {
        let (before, before_root, _, _) = pair();
        let (after, after_root, _, _) = pair();
        let report = diff_trees(&before, before_root, &after, after_root, options()).expect("roots exist");

        assert_eq!(report.summary.added.entries, 0);
        assert_eq!(report.summary.removed.entries, 0);
        assert_eq!(report.summary.grown.entries, 0);
        assert_eq!(report.summary.shrunk.entries, 0);
        assert_eq!(report.summary.metadata_changed, 0);
        assert_eq!(report.summary.logical_delta, 0);
        assert!(!report.summary.truncated);
    }

    #[test]
    fn a_size_change_the_selected_metric_cannot_see_is_still_reported() {
        // A file rewritten sparse: same `st_size`, fewer blocks. Ranking by
        // logical bytes must not file that as unchanged.
        let mut old = Fixture::new();
        let old_root = old.root;
        old.file(old_root, b"sparse.bin", 100_000, 102_400, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        new.file(new_root, b"sparse.bin", 100_000, 4_096, 10);
        let (after, after_root) = new.finish();

        let report = diff_trees(&before, before_root, &after, after_root, options()).expect("roots exist");
        assert_eq!(report.summary.shrunk.entries, 1);
        assert_eq!(report.summary.shrunk.logical_delta, 0);
        assert_eq!(report.summary.shrunk.allocated_delta, -98_304);
        assert_eq!(report.summary.metadata_changed, 0, "it is a size change, not a touch");
    }

    #[test]
    fn the_selected_metric_decides_the_order_of_the_leaderboard() {
        // `x` moved more logical bytes, `y` moved more allocated bytes. Which
        // one leads is the caller's choice, and the report says which it was.
        let mut old = Fixture::new();
        let old_root = old.root;
        old.file(old_root, b"x.bin", 1_000, 4_096, 10);
        old.file(old_root, b"y.bin", 1_000, 4_096, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        new.file(new_root, b"x.bin", 900_000, 8_192, 10);
        new.file(new_root, b"y.bin", 2_000, 900_000, 10);
        let (after, after_root) = new.finish();

        let by_logical = diff_trees(
            &before,
            before_root,
            &after,
            after_root,
            options().with_metric(DiffMetric::Logical),
        )
        .expect("roots exist");
        assert_eq!(by_logical.metric, DiffMetric::Logical);
        assert_eq!(by_logical.grown.first().expect("two grew").path.as_str(), "root/x.bin");

        let by_allocated = diff_trees(
            &before,
            before_root,
            &after,
            after_root,
            options().with_metric(DiffMetric::Allocated),
        )
        .expect("roots exist");
        assert_eq!(by_allocated.metric, DiffMetric::Allocated);
        assert_eq!(
            by_allocated.grown.first().expect("two grew").path.as_str(),
            "root/y.bin"
        );
    }

    #[test]
    fn the_lists_are_capped_but_the_summary_counts_are_not() {
        let mut old = Fixture::new();
        let old_root = old.root;
        old.file(old_root, b"anchor.bin", 1, 1, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        new.file(new_root, b"anchor.bin", 1, 1, 10);
        for index in 0..50_u64 {
            let name = format!("added-{index:03}.bin");
            new.file(new_root, name.as_bytes(), index + 1, 4_096, 10);
        }
        let (after, after_root) = new.finish();

        let report =
            diff_trees(&before, before_root, &after, after_root, options().with_limit(5)).expect("roots exist");

        assert_eq!(report.summary.added.entries, 50, "the count is the truth");
        assert_eq!(report.added.len(), 5, "the list is a leaderboard");
        assert_eq!(report.limit, 5);
        // Biggest first, and it really is the biggest — not the first five seen.
        assert_eq!(report.added.first().expect("rows").after_logical, 50);
        let magnitudes: Vec<i64> = report.added.iter().map(|row| row.logical_delta).collect();
        assert_eq!(magnitudes, vec![50, 49, 48, 47, 46]);
        // The uncapped total still accounts for every added byte.
        assert_eq!(report.summary.added.logical_delta, (1..=50).sum::<i64>());
    }

    #[test]
    fn a_limit_beyond_the_ceiling_is_clamped_rather_than_honoured() {
        let (before, before_root, after, after_root) = pair();
        let report = diff_trees(
            &before,
            before_root,
            &after,
            after_root,
            options().with_limit(usize::MAX),
        )
        .expect("roots exist");
        assert_eq!(report.limit, u32::try_from(MAX_DIFF_ENTRIES).expect("fits"));
    }

    #[test]
    fn a_kind_change_at_the_same_name_is_counted_and_never_descended_into() {
        let mut old = Fixture::new();
        let old_root = old.root;
        old.file(old_root, b"thing", 100, 4_096, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        let thing = new.dir(new_root, b"thing");
        new.file(thing, b"inside.bin", 50_000, 65_536, 10);
        let (after, after_root) = new.finish();

        let report = diff_trees(&before, before_root, &after, after_root, options()).expect("roots exist");
        assert_eq!(report.summary.kind_changed, 1);
        let grown = report.grown.first().expect("a file became a directory");
        assert_eq!(grown.path.as_str(), "root/thing");
        assert!(grown.kind_changed);
        assert_eq!(grown.kind, Kind::Directory, "the kind reported is the after side");
        assert_eq!(grown.logical_delta, 50_000 - 100);
        assert!(
            !paths(&report.added).contains(&"root/thing/inside.bin"),
            "one side is a leaf, so there is nothing to join against"
        );
        assert_eq!(report.summary.descended, 1, "only the root pair");
    }

    #[test]
    fn two_leaf_roots_still_produce_a_change_rather_than_an_empty_report() {
        let mut old = Fixture::new();
        let old_root = old.root;
        let old_file = old.file(old_root, b"only.bin", 1_000, 4_096, 10);
        let (before, _) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        let new_file = new.file(new_root, b"only.bin", 5_000, 8_192, 20);
        let (after, _) = new.finish();

        let report = diff_trees(&before, old_file, &after, new_file, options()).expect("both are nodes");
        assert_eq!(report.summary.grown.entries, 1);
        assert_eq!(report.summary.logical_delta, 4_000);
        assert_eq!(report.summary.descended, 0, "a leaf has no children to join");
    }

    #[test]
    fn a_virtual_group_root_resolves_to_the_directory_that_owns_it() {
        let (before, before_root, after, after_root) = pair();
        let group = NodeId::virtual_group_of(after_root).expect("taggable");
        let report = diff_trees(&before, before_root, &after, group, options()).expect("the owner exists");
        // Identical to diffing the directory itself: a group is a view over one
        // directory's leaves, not a subtree of its own.
        let direct = diff_trees(&before, before_root, &after, after_root, options()).expect("roots exist");
        assert_eq!(report.summary, direct.summary);
    }

    #[test]
    fn an_unknown_root_is_rejected_rather_than_compared_against_nothing() {
        let (before, before_root, after, after_root) = pair();
        let missing = NodeId::from_index(9_999).expect("in range");

        assert!(matches!(
            diff_trees(&before, missing, &after, after_root, options()),
            Err(QueryError::UnknownNode { node }) if node == missing
        ));
        assert!(matches!(
            diff_trees(&before, before_root, &after, missing, options()),
            Err(QueryError::UnknownNode { node }) if node == missing
        ));
        assert!(diff_trees(&before, NodeId::NONE, &after, after_root, options()).is_err());
    }

    #[test]
    fn the_report_says_which_two_scans_it_compared() {
        let report = report();
        // A diff that does not name its two sides is unreadable, so the labels
        // are a required argument rather than a decoration.
        assert_eq!(report.before.taken_unix_ms, Some(1_000));
        assert_eq!(report.after.taken_unix_ms, Some(2_000));
        assert_eq!(report.before.root.as_str(), "/root");
    }

    #[test]
    fn names_are_joined_as_bytes_so_a_separator_in_a_name_cannot_collide() {
        // Two entries whose concatenated `parent/name` strings would be equal:
        // `a/b` under root, versus a directory `a` holding `b`. The pair join
        // keeps them distinct; a string key would not.
        let mut old = Fixture::new();
        let old_root = old.root;
        let old_dir = old.dir(old_root, b"a");
        old.file(old_dir, b"b", 1_000, 4_096, 10);
        let (before, before_root) = old.finish();

        let mut new = Fixture::new();
        let new_root = new.root;
        new.file(new_root, b"a\nb", 7_000, 8_192, 10);
        let (after, after_root) = new.finish();

        let report = diff_trees(&before, before_root, &after, after_root, options()).expect("roots exist");
        assert_eq!(report.summary.removed.entries, 1, "a/ went away");
        assert_eq!(report.summary.added.entries, 1, "a\\nb arrived");
        assert_eq!(report.summary.grown.entries, 0, "they must not be joined together");
    }
}
