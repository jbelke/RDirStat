//! What a folder is *made of*, so a directory tile has a colour worth drawing.
//!
//! ## Why this exists
//!
//! The scanner categorises **files**. A directory has no content type of its
//! own, so it carries [`CategoryId::UNCATEGORIZED`] and the frontend paints it
//! the neutral grey. In a treemap that is invisible — the directory rectangles
//! are underneath their children, and what you see is leaves. In the icicle and
//! the sunburst it is the whole picture: those two draw a bounded number of
//! *levels*, so on a large disk every ring is a directory, every ring is
//! uncategorized, and the chart comes out a uniform grey disc that says nothing
//! the tree table did not already say.
//!
//! So a directory borrows a category: **the category of the largest thing
//! inside it**.
//!
//! ## Why "largest thing", not "largest total"
//!
//! The truthful answer to "what is this folder made of" is the category holding
//! the most bytes across the subtree, and it is the one thing this cannot
//! afford. It needs a running total per category per directory — 25 counters
//! against 1.2M directories at the design profile, which is 240 MB of `u64` for
//! a colour hint.
//!
//! Taking the single heaviest child instead costs one byte and one `u64` per
//! directory, and it answers a question people actually ask: *the biggest thing
//! in here is a video*. A directory child contributes its own resolved answer,
//! so the colour propagates up from whichever leaf is genuinely heaviest — a
//! 40 GB disk image eight levels down still colours every ring above it.
//!
//! The two answers differ only when many small files of one category outweigh
//! one large file of another. That is a real case and the wrong colour is a
//! *hint* rather than a claim: the tooltip and the details panel still report
//! the node's own category, and the table still says `Uncategorized`.
//!
//! ## Cost
//!
//! `O(subtree nodes)` time and `O(subtree directories)` memory, in the same two
//! passes [`FilteredWeights`](crate::FilteredWeights) uses and for the same
//! reason: a parent's answer needs its children's, and the arena is not
//! guaranteed to be ordered parent-before-child by anything a snapshot loader
//! validates.

use rdirstat_core::{CategoryId, DirId, NodeId, Tree};

use crate::options::SizeMetric;

/// Per-directory "category of the heaviest thing inside", by `DirId::slot()`.
#[derive(Debug)]
pub struct DominantCategories {
    /// The borrowed category. `UNCATEGORIZED` for a directory whose subtree
    /// holds no categorized file — which is the honest answer, not a failure.
    dirs: Vec<u8>,
    /// The weight that won, kept so a parent can compare children without
    /// re-reading the tree.
    weights: Vec<u64>,
    metric: SizeMetric,
}

impl DominantCategories {
    /// Resolves every directory under `root`.
    #[must_use]
    pub fn build(tree: &Tree, root: NodeId, metric: SizeMetric) -> Self {
        let slots = tree.dirs().len();
        let mut dirs = vec![CategoryId::UNCATEGORIZED.get(); slots];
        let mut weights = vec![0_u64; slots];

        // Pass 1 — directories in pre-order, parents before descendants.
        let mut order: Vec<NodeId> = Vec::new();
        let mut stack = vec![root];
        // The arena is acyclic by `Tree`'s freeze-time validation; this bounds a
        // tree that somehow escaped it rather than an expected shape.
        let mut budget = tree.len().saturating_mul(2).saturating_add(16);
        while let Some(id) = stack.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let Some(node) = tree.node(id) else { continue };
            if !node.kind().is_directory() {
                continue;
            }
            order.push(id);
            stack.extend(tree.children(id));
        }

        // Pass 2 — in reverse, so every child directory already has an answer.
        for &dir in order.iter().rev() {
            let mut best_weight = 0_u64;
            let mut best_category = CategoryId::UNCATEGORIZED.get();

            for child in tree.children(dir) {
                let Some(node) = tree.node(child) else { continue };
                let (weight, category) = if node.kind().is_directory() {
                    let Some(slot) = tree.dirs().dir_id(child).map(DirId::slot) else {
                        continue;
                    };
                    (
                        weights.get(slot).copied().unwrap_or(0),
                        dirs.get(slot).copied().unwrap_or(CategoryId::UNCATEGORIZED.get()),
                    )
                } else {
                    // `contributed_*` already zeroes a repeated hard link, so
                    // that policy is not re-implemented here.
                    let bytes = match metric {
                        SizeMetric::Allocated => node.contributed_alloc(),
                        SizeMetric::Logical => node.contributed_size(),
                    };
                    (bytes, node.category().get())
                };

                // A tie keeps the first winner rather than the last, so the
                // colour of a folder does not depend on child iteration order
                // changing between builds.
                if weight > best_weight && category != CategoryId::UNCATEGORIZED.get() {
                    best_weight = weight;
                    best_category = category;
                }
            }

            if let Some(slot) = tree.dirs().dir_id(dir).map(DirId::slot) {
                if let Some(cell) = dirs.get_mut(slot) {
                    *cell = best_category;
                }
                if let Some(cell) = weights.get_mut(slot) {
                    *cell = best_weight;
                }
            }
        }

        Self { dirs, weights, metric }
    }

    /// The metric these were built for, so a cache can tell whether they apply.
    #[must_use]
    pub const fn metric(&self) -> SizeMetric {
        self.metric
    }

    /// The borrowed category for a directory, or `UNCATEGORIZED` for anything
    /// that is not one — which is exactly what a leaf should keep.
    #[must_use]
    pub fn of(&self, tree: &Tree, node: NodeId) -> u8 {
        tree.dirs()
            .dir_id(node)
            .and_then(|id| self.dirs.get(id.slot()).copied())
            .unwrap_or(CategoryId::UNCATEGORIZED.get())
    }

    /// The weight that decided [`of`](Self::of). Exposed for tests, which is
    /// the only way to assert the propagation without re-implementing it.
    #[must_use]
    pub fn weight_of(&self, tree: &Tree, node: NodeId) -> u64 {
        tree.dirs()
            .dir_id(node)
            .and_then(|id| self.weights.get(id.slot()).copied())
            .unwrap_or(0)
    }
}
