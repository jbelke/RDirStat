//! Directory-only data, kept out of the file nodes that vastly outnumber it.
//!
//! Roughly 4% of entries are directories, so putting subtree totals on
//! [`Node`](crate::Node) would pay for them 25 times over. Instead a sorted
//! `Vec<NodeId>` names the directories and a parallel `Vec<DirTotals>` holds
//! their data; the two vectors are the same length and the same order.
//!
//! Lookup is a binary search. A node-count-sized `NodeId -> DirId` array is
//! **prohibited** unless a benchmark shows the search is a bottleneck and the
//! memory table in docs/01-ARCHITECTURE.md#memory-budget is amended first.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArenaError;
use crate::id::{DirId, NodeId};

/// Aggregated data for one directory.
///
/// Exactly 56 bytes; with the 4-byte [`NodeId`] beside it that is 60 bytes per
/// directory, which is the 0.15 GiB line in the memory budget at the 69M/4%
/// profile. Adding a field here is a budget change, not a detail.
///
/// Counts are `u32`: the design profile is 69M entries and the arena ceiling is
/// `2^31 - 1` nodes, so a `u64` would buy nothing but 16 more bytes per
/// directory. Every accumulator saturates rather than wrapping.
///
/// **Logical and allocated stay separate here and everywhere.** They answer
/// different questions and APFS makes them genuinely disagree; nothing in this
/// crate adds one to the other or reconciles them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[repr(C)]
pub struct DirTotals {
    /// Logical bytes in this subtree, after hard-link policy.
    pub logical: u64,
    /// Allocated bytes in this subtree, after hard-link policy.
    pub allocated: u64,
    /// Logical bytes of the files *directly* in this directory. Feeds the
    /// virtual `<Files>` group, which is a view, not an arena node.
    pub direct_logical: u64,
    /// Allocated bytes of the files directly in this directory.
    pub direct_allocated: u64,
    /// Newest modification time anywhere in this subtree, in Unix seconds.
    /// [`i64::MIN`] means "nothing observed".
    pub latest_mtime: i64,
    /// Entries observed in this subtree, including entries that aggregation
    /// omitted. Compared against `retained_nodes`, this is what stops an
    /// aggregated scan from looking like a complete inventory.
    pub observed_entries: u32,
    /// Nodes actually retained in this subtree.
    pub retained_nodes: u32,
    /// Files directly in this directory, for the virtual group's row count.
    pub direct_files: u32,
    /// Directories in this subtree that could not be read.
    pub unreadable: u32,
}

const _: () = assert!(core::mem::size_of::<DirTotals>() == 56);
const _: () = assert!(core::mem::align_of::<DirTotals>() == 8);

impl DirTotals {
    /// Totals for a directory with nothing in it yet.
    pub const EMPTY: Self = Self {
        logical: 0,
        allocated: 0,
        direct_logical: 0,
        direct_allocated: 0,
        latest_mtime: i64::MIN,
        observed_entries: 0,
        retained_nodes: 0,
        direct_files: 0,
        unreadable: 0,
    };

    /// Folds `child`'s subtree into this directory's subtree.
    ///
    /// Direct-file fields are deliberately not folded: they describe this
    /// directory's own files only.
    pub fn absorb_subtree(&mut self, child: &Self) {
        self.logical = self.logical.saturating_add(child.logical);
        self.allocated = self.allocated.saturating_add(child.allocated);
        self.observed_entries = self.observed_entries.saturating_add(child.observed_entries);
        self.retained_nodes = self.retained_nodes.saturating_add(child.retained_nodes);
        self.unreadable = self.unreadable.saturating_add(child.unreadable);
        self.latest_mtime = self.latest_mtime.max(child.latest_mtime);
    }

    /// Folds one directly contained file into this directory.
    ///
    /// `logical`/`allocated` are the *contributed* values, so a repeated hard
    /// link is passed as zero (see
    /// [`Node::contributed_size`](crate::Node::contributed_size)).
    pub fn absorb_direct_file(&mut self, logical: u64, allocated: u64, mtime: i64) {
        self.direct_logical = self.direct_logical.saturating_add(logical);
        self.direct_allocated = self.direct_allocated.saturating_add(allocated);
        self.direct_files = self.direct_files.saturating_add(1);
        self.logical = self.logical.saturating_add(logical);
        self.allocated = self.allocated.saturating_add(allocated);
        self.latest_mtime = self.latest_mtime.max(mtime);
    }

    /// Whether this directory has files of its own, i.e. whether a virtual
    /// `<Files>` group should be synthesized for it. A file-only directory may
    /// still elide the group in the UI.
    #[must_use]
    pub const fn has_direct_files(self) -> bool {
        self.direct_files > 0
    }

    /// Whether aggregation or an unreadable path means `retained_nodes`
    /// understates `observed_entries`.
    #[must_use]
    pub const fn is_partial(self) -> bool {
        self.retained_nodes < self.observed_entries || self.unreadable > 0
    }

    /// Newest modification time, or `None` if nothing was observed.
    #[must_use]
    pub const fn latest_mtime(self) -> Option<i64> {
        if self.latest_mtime == i64::MIN {
            None
        } else {
            Some(self.latest_mtime)
        }
    }
}

/// The sorted directory index and its parallel totals.
///
/// The invariant that makes the binary search valid: `ids` is strictly
/// ascending and `ids.len() == totals.len()`. [`DirIndex::push`] is the only
/// way to grow it and it enforces both.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct DirIndex {
    ids: Vec<NodeId>,
    totals: Vec<DirTotals>,
}

impl DirIndex {
    /// An empty index.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ids: Vec::new(),
            totals: Vec::new(),
        }
    }

    /// An empty index sized for `capacity` directories.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ids: Vec::with_capacity(capacity),
            totals: Vec::with_capacity(capacity),
        }
    }

    /// Appends a directory.
    ///
    /// # Errors
    ///
    /// [`ArenaError::DirIndexOutOfOrder`] if `node` is not strictly greater
    /// than the previous entry, or is not a real arena slot. The builder
    /// assigns ids in ascending order, so this only fires on a bug or on a
    /// corrupt snapshot.
    pub fn push(&mut self, node: NodeId, totals: DirTotals) -> Result<DirId, ArenaError> {
        if !node.is_node() {
            return Err(ArenaError::DirIndexOutOfOrder { node });
        }
        if let Some(&last) = self.ids.last()
            && node <= last
        {
            return Err(ArenaError::DirIndexOutOfOrder { node });
        }
        let index = u32::try_from(self.ids.len()).map_err(|_| ArenaError::DirIndexOutOfOrder { node })?;
        self.ids.push(node);
        self.totals.push(totals);
        Ok(DirId::from_index(index))
    }

    /// Resolves a node to its directory-table position by binary search.
    ///
    /// Returns `None` for a file, a virtual group, or an unknown id.
    #[must_use]
    pub fn dir_id(&self, node: NodeId) -> Option<DirId> {
        let position = self.ids.binary_search(&node).ok()?;
        u32::try_from(position).ok().map(DirId::from_index)
    }

    /// Totals for a directory node, or `None` if it is not a directory.
    #[must_use]
    pub fn totals_of(&self, node: NodeId) -> Option<&DirTotals> {
        self.totals.get(self.dir_id(node)?.slot())
    }

    /// Mutable totals for a directory node. Used by the rollup pass, before the
    /// tree is frozen.
    pub fn totals_of_mut(&mut self, node: NodeId) -> Option<&mut DirTotals> {
        let slot = self.dir_id(node)?.slot();
        self.totals.get_mut(slot)
    }

    /// Totals by table position, skipping the binary search.
    #[must_use]
    pub fn totals_at(&self, dir: DirId) -> Option<&DirTotals> {
        self.totals.get(dir.slot())
    }

    /// Mutable totals by table position.
    pub fn totals_at_mut(&mut self, dir: DirId) -> Option<&mut DirTotals> {
        self.totals.get_mut(dir.slot())
    }

    /// The node id at a table position.
    #[must_use]
    pub fn node_at(&self, dir: DirId) -> Option<NodeId> {
        self.ids.get(dir.slot()).copied()
    }

    /// Number of directories.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The sorted node ids, for snapshot writing.
    #[must_use]
    pub fn ids(&self) -> &[NodeId] {
        &self.ids
    }

    /// The parallel totals, for snapshot writing.
    #[must_use]
    pub fn totals(&self) -> &[DirTotals] {
        &self.totals
    }

    /// Every `(node, totals)` pair in ascending node order.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &DirTotals)> {
        self.ids.iter().copied().zip(self.totals.iter())
    }

    /// Releases unused capacity once the tree is frozen.
    pub fn shrink_to_fit(&mut self) {
        self.ids.shrink_to_fit();
        self.totals.shrink_to_fit();
    }

    /// Rebuilds an index from snapshot data.
    ///
    /// # Errors
    ///
    /// [`ArenaError::DirIndexOutOfOrder`] if the two vectors differ in length
    /// or `ids` is not strictly ascending. A snapshot is validated here before
    /// any node reaches a command.
    pub fn from_parts(ids: Vec<NodeId>, totals: Vec<DirTotals>) -> Result<Self, ArenaError> {
        if ids.len() != totals.len() {
            return Err(ArenaError::DirIndexOutOfOrder { node: NodeId::NONE });
        }
        for pair in ids.windows(2) {
            if let [previous, next] = pair
                && next <= previous
            {
                return Err(ArenaError::DirIndexOutOfOrder { node: *next });
            }
        }
        if let Some(bad) = ids.iter().find(|id| !id.is_node()) {
            return Err(ArenaError::DirIndexOutOfOrder { node: *bad });
        }
        Ok(Self { ids, totals })
    }
}

impl fmt::Debug for DirIndex {
    /// Summary only: printing 2.76M directories inside a log statement is not a
    /// diagnostic.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirIndex")
            .field("directories", &self.ids.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: u32) -> NodeId {
        NodeId::from_index(index).expect("in range")
    }

    #[test]
    fn totals_row_matches_the_memory_budget() {
        assert_eq!(core::mem::size_of::<DirTotals>(), 56);
    }

    #[test]
    fn lookup_finds_directories_and_rejects_everything_else() {
        let mut index = DirIndex::new();
        index.push(node(0), DirTotals::EMPTY).expect("ascending");
        index.push(node(7), DirTotals::EMPTY).expect("ascending");
        index.push(node(9), DirTotals::EMPTY).expect("ascending");

        assert_eq!(index.dir_id(node(7)).map(DirId::index), Some(1));
        assert_eq!(index.dir_id(node(8)), None, "a file id is not in the table");
        assert_eq!(index.dir_id(NodeId::NONE), None);
        let group = NodeId::virtual_group_of(node(7)).expect("taggable");
        assert_eq!(index.dir_id(group), None, "a virtual group has no row of its own");
    }

    #[test]
    fn out_of_order_push_is_rejected_so_binary_search_stays_valid() {
        let mut index = DirIndex::new();
        index.push(node(5), DirTotals::EMPTY).expect("ascending");
        assert!(index.push(node(5), DirTotals::EMPTY).is_err());
        assert!(index.push(node(4), DirTotals::EMPTY).is_err());
        assert!(index.push(NodeId::NONE, DirTotals::EMPTY).is_err());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn absorbing_keeps_logical_and_allocated_separate() {
        let mut parent = DirTotals::EMPTY;
        parent.absorb_direct_file(1_000, 4_096, 100);
        parent.absorb_direct_file(0, 0, 50);

        let mut child = DirTotals::EMPTY;
        child.absorb_direct_file(2_000, 8_192, 300);
        parent.absorb_subtree(&child);

        assert_eq!(parent.logical, 3_000);
        assert_eq!(parent.allocated, 12_288);
        assert_eq!(parent.direct_logical, 1_000, "the child's bytes are not direct");
        assert_eq!(parent.direct_files, 2);
        assert_eq!(parent.latest_mtime(), Some(300));
    }

    #[test]
    fn empty_totals_report_no_observation_time() {
        assert_eq!(DirTotals::EMPTY.latest_mtime(), None);
        assert!(!DirTotals::EMPTY.has_direct_files());
        assert!(!DirTotals::EMPTY.is_partial());
    }

    #[test]
    fn snapshot_parts_are_validated_before_use() {
        assert!(DirIndex::from_parts(vec![node(1), node(0)], vec![DirTotals::EMPTY; 2]).is_err());
        assert!(DirIndex::from_parts(vec![node(0)], vec![]).is_err());
        assert!(DirIndex::from_parts(vec![node(0), node(1)], vec![DirTotals::EMPTY; 2]).is_ok());
    }
}
