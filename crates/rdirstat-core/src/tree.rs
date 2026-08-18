//! The append-only arena and the frozen tree it becomes.
//!
//! One [`TreeBuilder`] owns the arena and is the only thing that assigns ids,
//! links children, and pushes names. Reader workers hand it batches; they never
//! touch the arena. That is the whole concurrency design: **no
//! `Arc<Mutex<Tree>>`, no per-node atomic, no unbounded result channel.**
//!
//! [`TreeBuilder::finish`] validates and freezes into a [`Tree`], which is
//! immutable, `Send + Sync`, and published inside an `Arc<CompletedScan>`.
//! Commands clone the `Arc` and hold no application lock while computing.

use core::fmt;

use crate::dirs::{DirIndex, DirTotals};
use crate::error::{ArenaError, QueryError};
use crate::id::{DirId, NodeId};
use crate::name::{NameBlob, NameRef};
use crate::node::Node;

/// Ceiling on parent-chain length.
///
/// Recursion depth is not trusted anywhere in this crate: every traversal is
/// iterative and every parent walk is bounded, so a corrupt snapshot with a
/// cycle fails closed instead of overflowing the stack. An 8-bit depth cap
/// would not be safe for a hostile tree, which is why this is a `u32`.
pub const MAX_TREE_DEPTH: u32 = 4096;

/// A frozen, immutable arena.
///
/// Everything about it is read-only after [`TreeBuilder::finish`]. Structural
/// invariants, checked once at freeze and relied on thereafter:
///
/// 1. every [`NameRef`] resolves inside the blob;
/// 2. every non-`NONE` link names a live slot;
/// 3. the directory index is strictly ascending and every entry is a
///    [`Kind::Directory`];
/// 4. the parent chain from any node reaches the root within
///    [`MAX_TREE_DEPTH`] steps.
#[derive(Clone, PartialEq, Eq)]
pub struct Tree {
    nodes: Vec<Node>,
    names: NameBlob,
    dirs: DirIndex,
    root: NodeId,
}

impl Tree {
    /// The scan root.
    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Number of retained nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Bytes of heap this arena holds, for the concurrent-scan budget.
    ///
    /// The three allocations that scale with the tree: the node array, the name
    /// blob, and the directory index. Everything else here is a handful of
    /// scalars.
    ///
    /// **A floor, and deliberately named as an estimate.** It counts what this
    /// crate allocated and cannot see allocator fragmentation, the pages the
    /// scanner touched and freed, or anything `src-tauri` layered on top. The
    /// admission control in `src-tauri` reserves headroom precisely because
    /// this number is a floor rather than a measurement — a scan that was
    /// admitted on the strength of an exact-looking figure and then pushed the
    /// process past its RSS gate is the failure this is meant to prevent, so it
    /// must not read as more precise than it is.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let nodes = self.nodes.capacity().saturating_mul(core::mem::size_of::<Node>());
        let total = nodes
            .saturating_add(self.names.heap_bytes())
            .saturating_add(self.dirs.heap_bytes());
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    /// Whether the tree holds no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Number of directories, i.e. rows in the side tables.
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.dirs.len()
    }

    /// The node at `id`, or `None` for [`NodeId::NONE`], a virtual group, or an
    /// id that is out of range.
    ///
    /// This is the only way to reach a node, so an id arriving from IPC cannot
    /// index out of bounds.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.slot()?)
    }

    /// The name bytes for `id`, exactly as the filesystem produced them.
    #[must_use]
    pub fn name_bytes(&self, id: NodeId) -> Option<&[u8]> {
        self.names.bytes(self.node(id)?.name)
    }

    /// The name blob, for snapshot writing and for resolving a [`NameRef`]
    /// obtained elsewhere.
    #[must_use]
    pub const fn names(&self) -> &NameBlob {
        &self.names
    }

    /// Every node, for snapshot writing and whole-tree passes such as Parquet
    /// projection. Not an IPC payload.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The directory index and its parallel totals.
    #[must_use]
    pub const fn dirs(&self) -> &DirIndex {
        &self.dirs
    }

    /// Subtree totals for a directory, or for the directory that owns a virtual
    /// group.
    #[must_use]
    pub fn dir_totals(&self, id: NodeId) -> Option<&DirTotals> {
        let directory = id.group_owner().unwrap_or(id);
        self.dirs.totals_of(directory)
    }

    /// The parent of `id`, or `None` at the root.
    ///
    /// A virtual group's parent is the directory that owns it.
    #[must_use]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        if let Some(owner) = id.group_owner() {
            return Some(owner);
        }
        let parent = self.node(id)?.parent;
        if parent.is_none() { None } else { Some(parent) }
    }

    /// The children of `id`, in insertion order.
    ///
    /// Insertion order is **not** a UI contract; `children` sorts and pages.
    /// A virtual group has no children.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Children<'_> {
        let first = match self.node(id) {
            Some(node) => node.first_child,
            None => NodeId::NONE,
        };
        Children {
            tree: self,
            next: first,
            remaining: self.nodes.len(),
        }
    }

    /// Number of direct children of `id`.
    #[must_use]
    pub fn child_count(&self, id: NodeId) -> u32 {
        u32::try_from(self.children(id).count()).unwrap_or(u32::MAX)
    }

    /// Whether `id` names something this tree can answer questions about.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        if let Some(owner) = id.group_owner() {
            return self.dirs.totals_of(owner).is_some();
        }
        self.node(id).is_some()
    }

    /// The tagged virtual `<Files>` group for a directory, if it has files of
    /// its own.
    ///
    /// The group is a *view*: no arena node is allocated for it, which is worth
    /// roughly one synthetic node per directory at the design profile.
    #[must_use]
    pub fn virtual_group(&self, directory: NodeId) -> Option<NodeId> {
        let totals = self.dirs.totals_of(directory)?;
        if totals.has_direct_files() {
            NodeId::virtual_group_of(directory)
        } else {
            None
        }
    }

    /// Appends the full path of `id` to `out` as filesystem bytes, rooted at
    /// the scan root's own name.
    ///
    /// Paths are reconstructed rather than stored: 69M stored paths would not
    /// fit the memory budget, and a stored path would go stale anyway.
    ///
    /// # Errors
    ///
    /// - [`QueryError::UnknownNode`] if `id` is not in this tree.
    /// - [`QueryError::VirtualGroup`] if `id` is a virtual group, which has no
    ///   path of its own.
    /// - [`QueryError::PathTooDeep`] if the parent chain exceeds
    ///   [`MAX_TREE_DEPTH`], i.e. the arena is cyclic.
    pub fn path_bytes(&self, id: NodeId, out: &mut Vec<u8>) -> Result<(), QueryError> {
        if id.is_virtual_group() {
            return Err(QueryError::VirtualGroup { node: id });
        }
        if self.node(id).is_none() {
            return Err(QueryError::UnknownNode { node: id });
        }

        let mut chain: Vec<NodeId> = Vec::new();
        let mut cursor = id;
        let mut depth = 0_u32;
        loop {
            chain.push(cursor);
            let Some(node) = self.node(cursor) else {
                return Err(QueryError::UnknownNode { node: cursor });
            };
            if node.parent.is_none() {
                break;
            }
            cursor = node.parent;
            depth += 1;
            if depth > MAX_TREE_DEPTH {
                return Err(QueryError::PathTooDeep {
                    node: id,
                    limit: MAX_TREE_DEPTH,
                });
            }
        }

        for (position, step) in chain.iter().rev().enumerate() {
            let Some(node) = self.node(*step) else {
                return Err(QueryError::UnknownNode { node: *step });
            };
            let Some(bytes) = self.names.bytes(node.name) else {
                return Err(QueryError::Corrupt(ArenaError::UnknownNode { node: *step }));
            };
            if position > 0 && !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(bytes);
        }
        Ok(())
    }

    /// Depth of `id` below the root; the root itself is zero.
    ///
    /// # Errors
    ///
    /// [`QueryError::UnknownNode`] or [`QueryError::PathTooDeep`], on the same
    /// conditions as [`Tree::path_bytes`].
    pub fn depth(&self, id: NodeId) -> Result<u32, QueryError> {
        let start = id.group_owner().unwrap_or(id);
        let extra = u32::from(id.is_virtual_group());
        let mut cursor = start;
        let mut depth = 0_u32;
        loop {
            let Some(node) = self.node(cursor) else {
                return Err(QueryError::UnknownNode { node: cursor });
            };
            if node.parent.is_none() {
                return Ok(depth + extra);
            }
            cursor = node.parent;
            depth += 1;
            if depth > MAX_TREE_DEPTH {
                return Err(QueryError::PathTooDeep {
                    node: id,
                    limit: MAX_TREE_DEPTH,
                });
            }
        }
    }

    /// Logical bytes attributed to `id`: its subtree total for a directory, its
    /// contributed size for a leaf, its direct-file bytes for a virtual group.
    #[must_use]
    pub fn logical_of(&self, id: NodeId) -> u64 {
        if let Some(owner) = id.group_owner() {
            return self.dirs.totals_of(owner).map_or(0, |totals| totals.direct_logical);
        }
        match self.dirs.totals_of(id) {
            Some(totals) => totals.logical,
            None => self.node(id).map_or(0, |node| node.contributed_size()),
        }
    }

    /// Allocated bytes attributed to `id`, on the same basis as
    /// [`Tree::logical_of`]. Kept strictly separate from the logical value.
    #[must_use]
    pub fn allocated_of(&self, id: NodeId) -> u64 {
        if let Some(owner) = id.group_owner() {
            return self.dirs.totals_of(owner).map_or(0, |totals| totals.direct_allocated);
        }
        match self.dirs.totals_of(id) {
            Some(totals) => totals.allocated,
            None => self.node(id).map_or(0, |node| node.contributed_alloc()),
        }
    }

    /// Rebuilds a tree from validated snapshot parts.
    ///
    /// # Errors
    ///
    /// Every [`ArenaError`] that [`TreeBuilder::finish`] can produce. A
    /// `*.rdstat` file is untrusted input: nothing here is exposed to a command
    /// until this returns `Ok`.
    pub fn from_parts(nodes: Vec<Node>, names: NameBlob, dirs: DirIndex, root: NodeId) -> Result<Self, ArenaError> {
        let tree = Self {
            nodes,
            names,
            dirs,
            root,
        };
        tree.validate()?;
        Ok(tree)
    }

    /// Rechecks every structural invariant.
    ///
    /// Run at freeze and after a snapshot load. Linear in node count, so it is
    /// not something a command does per request.
    ///
    /// # Errors
    ///
    /// The [`ArenaError`] naming the first invariant that failed.
    pub fn validate(&self) -> Result<(), ArenaError> {
        if self.nodes.is_empty() {
            return Ok(());
        }
        if self.root.slot().is_none_or(|slot| slot >= self.nodes.len()) {
            return Err(ArenaError::UnknownNode { node: self.root });
        }

        for (index, node) in self.nodes.iter().enumerate() {
            let id = NodeId::from_index(u32::try_from(index).unwrap_or(u32::MAX)).ok_or(ArenaError::NodeCeiling {
                ceiling: crate::MAX_NODE_INDEX,
            })?;
            if !self.names.contains(node.name) {
                return Err(ArenaError::UnknownNode { node: id });
            }
            for link in [node.parent, node.first_child, node.next_sibling] {
                if !link.is_none() && self.node(link).is_none() {
                    return Err(ArenaError::UnknownNode { node: link });
                }
            }
            if !node.first_child.is_none() && !node.kind().is_directory() {
                return Err(ArenaError::NotADirectory { parent: id });
            }
        }

        for (id, _) in self.dirs.iter() {
            match self.node(id) {
                Some(node) if node.kind().is_directory() => {}
                Some(_) => return Err(ArenaError::NotADirectory { parent: id }),
                None => return Err(ArenaError::UnknownNode { node: id }),
            }
        }

        // Bounded parent walk from every node: catches a cycle that the link
        // checks above cannot see.
        for index in 0..self.nodes.len() {
            let Some(start) = u32::try_from(index).ok().and_then(NodeId::from_index) else {
                return Err(ArenaError::NodeCeiling {
                    ceiling: crate::MAX_NODE_INDEX,
                });
            };
            let mut cursor = start;
            let mut depth = 0_u32;
            while let Some(node) = self.node(cursor) {
                if node.parent.is_none() {
                    break;
                }
                cursor = node.parent;
                depth += 1;
                if depth > MAX_TREE_DEPTH {
                    return Err(ArenaError::CycleOrDepth {
                        node: start,
                        limit: MAX_TREE_DEPTH,
                    });
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Tree {
    /// Summary only. A derived `Debug` over 69M nodes is a way to hang a
    /// process inside a log statement.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("nodes", &self.nodes.len())
            .field("directories", &self.dirs.len())
            .field("name_bytes", &self.names.len())
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Iterator over the direct children of a node.
///
/// Bounded by the arena size, so a corrupt sibling cycle terminates instead of
/// looping forever.
#[derive(Debug)]
pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeId,
    remaining: usize,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.remaining == 0 || self.next.is_none() {
            return None;
        }
        let current = self.next;
        let node = self.tree.node(current)?;
        self.next = node.next_sibling;
        self.remaining -= 1;
        Some(current)
    }
}

/// The single-writer arena builder.
///
/// The only component that assigns ids, links children, interns names, and
/// registers directories. It is deliberately **not** `Sync`-shared: one thread
/// owns it for the whole scan.
#[derive(Default)]
pub struct TreeBuilder {
    nodes: Vec<Node>,
    names: NameBlob,
    dirs: DirIndex,
    root: NodeId,
}

impl TreeBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            names: NameBlob::new(),
            dirs: DirIndex::new(),
            root: NodeId::NONE,
        }
    }

    /// A builder with explicit capacity for `nodes` entries, `name_bytes` of
    /// names, and `directories` directory rows.
    ///
    /// Capacity is always explicit at this scale: unbounded doubling at 3 GiB
    /// briefly needs 4.5 GiB, which is how a scan dies at 95% complete
    /// (docs/01-ARCHITECTURE.md#memory-budget).
    #[must_use]
    pub fn with_capacity(nodes: usize, name_bytes: usize, directories: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(nodes),
            names: NameBlob::with_capacity(name_bytes),
            dirs: DirIndex::with_capacity(directories),
            root: NodeId::NONE,
        }
    }

    /// Interns a name and returns its packed reference.
    ///
    /// # Errors
    ///
    /// [`ArenaError::NameTooLong`] or [`ArenaError::NameOffsetOverflow`].
    pub fn intern(&mut self, name: &[u8]) -> Result<NameRef, ArenaError> {
        self.names.push(name)
    }

    /// Appends `node` and returns its id.
    ///
    /// Does not link it: use [`TreeBuilder::push_child`] for that. A directory
    /// pushed here must also be registered with
    /// [`TreeBuilder::register_directory`].
    ///
    /// # Errors
    ///
    /// [`ArenaError::NodeCeiling`] once the arena holds
    /// [`MAX_NODE_INDEX`](crate::MAX_NODE_INDEX) + 1 nodes.
    pub fn push_node(&mut self, node: Node) -> Result<NodeId, ArenaError> {
        let index = u32::try_from(self.nodes.len()).map_err(|_| ArenaError::NodeCeiling {
            ceiling: crate::MAX_NODE_INDEX,
        })?;
        let id = NodeId::from_index(index).ok_or(ArenaError::NodeCeiling {
            ceiling: crate::MAX_NODE_INDEX,
        })?;
        self.nodes.push(node);
        if self.root.is_none() {
            self.root = id;
        }
        Ok(id)
    }

    /// Appends `node` and links it as a child of `parent`.
    ///
    /// Insertion is at the head of the sibling list, which is `O(1)` and does
    /// not require walking to the tail on the hot path. Child order is not a UI
    /// contract.
    ///
    /// # Errors
    ///
    /// - [`ArenaError::UnknownNode`] if `parent` is not in the arena.
    /// - [`ArenaError::NotADirectory`] if `parent` is not a
    ///   [`Kind::Directory`].
    /// - [`ArenaError::NodeCeiling`] if the arena is full.
    pub fn push_child(&mut self, parent: NodeId, mut node: Node) -> Result<NodeId, ArenaError> {
        let parent_slot = parent.slot().ok_or(ArenaError::UnknownNode { node: parent })?;
        let parent_node = self
            .nodes
            .get(parent_slot)
            .ok_or(ArenaError::UnknownNode { node: parent })?;
        if !parent_node.kind().is_directory() {
            return Err(ArenaError::NotADirectory { parent });
        }
        node.parent = parent;
        node.next_sibling = parent_node.first_child;
        let id = self.push_node(node)?;
        if let Some(parent_node) = self.nodes.get_mut(parent_slot) {
            parent_node.first_child = id;
        }
        Ok(id)
    }

    /// Registers a directory in the side tables.
    ///
    /// Must be called in ascending id order, which the builder satisfies
    /// naturally because it assigns ids sequentially.
    ///
    /// # Errors
    ///
    /// - [`ArenaError::UnknownNode`] if `id` is not in the arena.
    /// - [`ArenaError::NotADirectory`] if the node is not a directory.
    /// - [`ArenaError::DirIndexOutOfOrder`] if `id` is not greater than the
    ///   previous registration.
    pub fn register_directory(&mut self, id: NodeId, totals: DirTotals) -> Result<DirId, ArenaError> {
        let slot = id.slot().ok_or(ArenaError::UnknownNode { node: id })?;
        let node = self.nodes.get(slot).ok_or(ArenaError::UnknownNode { node: id })?;
        if !node.kind().is_directory() {
            return Err(ArenaError::NotADirectory { parent: id });
        }
        self.dirs.push(id, totals)
    }

    /// The node at `id`, if it exists.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.slot()?)
    }

    /// Mutable access to the node at `id`, for late flag updates such as
    /// [`flags::UNREADABLE`] once a directory read fails.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let slot = id.slot()?;
        self.nodes.get_mut(slot)
    }

    /// Mutable access to a directory's totals, for accumulating direct-file
    /// data as entries arrive.
    pub fn dir_totals_mut(&mut self, id: NodeId) -> Option<&mut DirTotals> {
        self.dirs.totals_of_mut(id)
    }

    /// The scan root — the first node pushed.
    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Nodes pushed so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether nothing has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Directories registered so far.
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.dirs.len()
    }

    /// Bytes of names interned so far, and the capacity behind them. Published
    /// in the progress snapshot so the memory projection is measured, not
    /// guessed.
    #[must_use]
    pub fn name_bytes(&self) -> (usize, usize) {
        (self.names.len(), self.names.capacity())
    }

    /// Rolls subtree totals up to the root.
    ///
    /// An **iterative** post-order pass: an explicit stack, never recursion,
    /// because a 4096-deep `node_modules` chain is a real input and a stack
    /// overflow is not a diagnosable failure.
    ///
    /// Directories are visited in descending id order. The builder assigns ids
    /// in discovery order, so a child always has a higher id than its parent
    /// and one descending sweep is a valid post-order. Leaf contributions
    /// (including the direct-file totals) must already be in place; this pass
    /// only folds child directories into their parents.
    ///
    /// A node flagged [`flags::HARD_LINK_REPEAT`] contributes zero bytes, per
    /// [`Node::contributed_size`]. That policy lives in exactly one place.
    ///
    /// # Errors
    ///
    /// [`ArenaError::UnknownNode`] if the directory index names a node that is
    /// not in the arena.
    pub fn rollup(&mut self) -> Result<(), ArenaError> {
        for position in (0..self.dirs.len()).rev() {
            let dir = DirId::from_index(u32::try_from(position).unwrap_or(u32::MAX));
            let Some(id) = self.dirs.node_at(dir) else {
                return Err(ArenaError::UnknownNode { node: NodeId::NONE });
            };
            let Some(node) = self.node(id) else {
                return Err(ArenaError::UnknownNode { node: id });
            };
            let parent = node.parent;
            if parent.is_none() {
                continue;
            }
            let Some(totals) = self.dirs.totals_at(dir).copied() else {
                return Err(ArenaError::UnknownNode { node: id });
            };
            if let Some(parent_totals) = self.dirs.totals_of_mut(parent) {
                parent_totals.absorb_subtree(&totals);
                parent_totals.observed_entries = parent_totals.observed_entries.saturating_add(1);
                parent_totals.retained_nodes = parent_totals.retained_nodes.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Validates and freezes the arena.
    ///
    /// Releases spare capacity first, because the 5% arena slack in the memory
    /// budget is a scan-time allowance, not a resident cost.
    ///
    /// # Errors
    ///
    /// Whatever [`Tree::validate`] rejects.
    pub fn finish(mut self) -> Result<Tree, ArenaError> {
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
        self.dirs.shrink_to_fit();
        let tree = Tree {
            nodes: self.nodes,
            names: self.names,
            dirs: self.dirs,
            root: self.root,
        };
        tree.validate()?;
        Ok(tree)
    }
}

impl fmt::Debug for TreeBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TreeBuilder")
            .field("nodes", &self.nodes.len())
            .field("directories", &self.dirs.len())
            .field("name_bytes", &self.names.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::CategoryId;
    use crate::node::{Kind, flags};

    /// root/
    ///   media/            (dir)
    ///     clip.mkv        4 KiB logical, 8 KiB allocated
    ///     dupe.mkv        same content, repeated hard link
    ///   readme.txt        100 B
    fn fixture() -> (TreeBuilder, NodeId, NodeId) {
        let mut builder = TreeBuilder::new();

        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 10)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        let media_name = builder.intern(b"media").expect("interns");
        let media = builder
            .push_child(root, Node::directory(media_name, 20))
            .expect("links");
        builder.register_directory(media, DirTotals::EMPTY).expect("registers");

        let clip_name = builder.intern(b"clip.mkv").expect("interns");
        let clip = Node::leaf(clip_name, Kind::File, 4096, 8192, 30)
            .with_flags(flags::HARD_LINK)
            .with_category(CategoryId::from_raw(3));
        builder.push_child(media, clip).expect("links");
        if let Some(totals) = builder.dir_totals_mut(media) {
            totals.absorb_direct_file(clip.contributed_size(), clip.contributed_alloc(), 30);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        let dupe_name = builder.intern(b"dupe.mkv").expect("interns");
        let dupe =
            Node::leaf(dupe_name, Kind::File, 4096, 8192, 30).with_flags(flags::HARD_LINK | flags::HARD_LINK_REPEAT);
        builder.push_child(media, dupe).expect("links");
        if let Some(totals) = builder.dir_totals_mut(media) {
            totals.absorb_direct_file(dupe.contributed_size(), dupe.contributed_alloc(), 30);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        let readme_name = builder.intern(b"readme.txt").expect("interns");
        let readme = Node::leaf(readme_name, Kind::File, 100, 4096, 40);
        builder.push_child(root, readme).expect("links");
        if let Some(totals) = builder.dir_totals_mut(root) {
            totals.absorb_direct_file(readme.contributed_size(), readme.contributed_alloc(), 40);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        (builder, root, media)
    }

    #[test]
    fn rollup_counts_hard_linked_content_once() {
        let (mut builder, root, media) = fixture();
        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");

        let media_totals = *tree.dir_totals(media).expect("media is a directory");
        assert_eq!(media_totals.logical, 4096, "the repeat contributes zero");
        assert_eq!(media_totals.allocated, 8192);
        assert_eq!(media_totals.direct_files, 2, "but both entries stay visible");

        let root_totals = *tree.dir_totals(root).expect("root is a directory");
        assert_eq!(root_totals.logical, 4096 + 100);
        assert_eq!(root_totals.allocated, 8192 + 4096);
        assert_eq!(root_totals.latest_mtime(), Some(40));
    }

    #[test]
    fn rollup_totals_equal_a_naive_sum_over_contributions() {
        let (mut builder, root, _) = fixture();
        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");

        let naive_logical: u64 = tree.nodes().iter().map(|node| node.contributed_size()).sum();
        let naive_allocated: u64 = tree.nodes().iter().map(|node| node.contributed_alloc()).sum();
        let root_totals = tree.dir_totals(root).expect("root is a directory");
        assert_eq!(root_totals.logical, naive_logical);
        assert_eq!(root_totals.allocated, naive_allocated);
    }

    #[test]
    fn paths_are_reconstructed_from_parent_links() {
        let (mut builder, _, media) = fixture();
        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");

        let mut out = Vec::new();
        tree.path_bytes(media, &mut out).expect("has a path");
        assert_eq!(out, b"root/media");

        let clip = tree.children(media).next().expect("has children");
        out.clear();
        tree.path_bytes(clip, &mut out).expect("has a path");
        assert!(out.starts_with(b"root/media/"), "{:?}", String::from_utf8_lossy(&out));
        assert_eq!(tree.depth(clip).expect("depth"), 2);
    }

    #[test]
    fn a_virtual_group_is_a_view_with_no_arena_node() {
        let (mut builder, root, media) = fixture();
        builder.rollup().expect("rolls up");
        let node_count = builder.len();
        let tree = builder.finish().expect("valid");

        assert_eq!(tree.len(), node_count, "no synthetic node was allocated");
        let group = tree.virtual_group(media).expect("media has direct files");
        assert!(group.is_virtual_group());
        assert!(tree.contains(group));
        assert_eq!(tree.parent(group), Some(media));
        assert_eq!(tree.children(group).count(), 0);
        assert_eq!(tree.logical_of(group), 4096, "the group reports direct-file bytes only");
        assert!(tree.logical_of(root) > tree.logical_of(NodeId::virtual_group_of(root).expect("taggable")));
    }

    #[test]
    fn a_group_has_no_path_of_its_own() {
        let (mut builder, _, media) = fixture();
        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");
        let group = tree.virtual_group(media).expect("has direct files");
        let mut out = Vec::new();
        assert!(matches!(
            tree.path_bytes(group, &mut out),
            Err(QueryError::VirtualGroup { .. })
        ));
    }

    #[test]
    fn unknown_and_reserved_ids_are_rejected_not_indexed() {
        let (mut builder, _, _) = fixture();
        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");

        assert!(tree.node(NodeId::NONE).is_none());
        assert!(tree.node(NodeId::from_raw(u32::MAX)).is_none());
        assert!(tree.node(NodeId::from_index(9_999).expect("in range")).is_none());
        assert!(!tree.contains(NodeId::from_index(9_999).expect("in range")));
        let mut out = Vec::new();
        assert!(matches!(
            tree.path_bytes(NodeId::from_index(9_999).expect("in range"), &mut out),
            Err(QueryError::UnknownNode { .. })
        ));
    }

    #[test]
    fn a_file_cannot_become_a_parent() {
        let mut builder = TreeBuilder::new();
        let name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(name, 0)).expect("pushes");
        let leaf_name = builder.intern(b"a.txt").expect("interns");
        let leaf = builder
            .push_child(root, Node::leaf(leaf_name, Kind::File, 1, 1, 0))
            .expect("links");
        let orphan = builder.intern(b"b.txt").expect("interns");
        assert!(matches!(
            builder.push_child(leaf, Node::leaf(orphan, Kind::File, 1, 1, 0)),
            Err(ArenaError::NotADirectory { .. })
        ));
    }

    #[test]
    fn an_empty_tree_is_valid() {
        let tree = TreeBuilder::new().finish().expect("valid");
        assert!(tree.is_empty());
        assert_eq!(tree.root(), NodeId::NONE);
        assert_eq!(tree.children(NodeId::NONE).count(), 0);
    }
}
