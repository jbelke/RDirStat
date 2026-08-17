//! Filtering a layout by content category, with the areas re-proportioned.
//!
//! ## Why this needs a pass of its own
//!
//! Dimming filtered-out tiles is a paint-time decision and costs nothing. Making
//! them *stop occupying space* is not, because the number a treemap needs — how
//! many bytes of a directory's subtree match the filter — does not exist
//! anywhere. [`DirTotals`](rdirstat_core::DirTotals) stores the subtree total
//! and nothing per category, which is the right trade for a scanner (a
//! per-category breakdown on every directory would be 25 counters × 1.2M
//! directories) and leaves a filtered layout with nothing to read.
//!
//! So it is computed, once per filtered request, by [`FilteredWeights::build`].
//! An unfiltered layout never calls this and is byte-identical to before.
//!
//! ## The traversal, and why it is in this order
//!
//! A directory's filtered weight is its matching direct files plus its child
//! directories' filtered weights, so children must resolve first. Two passes:
//!
//! 1. collect the subtree's directories in **pre-order**, which guarantees a
//!    parent appears before every one of its descendants;
//! 2. walk that list **in reverse**, so every descendant is already resolved
//!    when its parent is reached.
//!
//! The pre-order list is what supplies the ordering guarantee. The tempting
//! shortcut — iterate the arena backwards, since a child's `NodeId` exceeds its
//! parent's — relies on an invariant the builder happens to satisfy but
//! [`Tree`] does not validate on load, and a snapshot is untrusted input. One
//! extra `Vec` is a small price for not depending on that.
//!
//! Cost is `O(subtree nodes)` in time and `O(subtree directories)` in memory —
//! at the 12M-node profile, a `Vec<u64>` over ~1.2M directories rather than
//! anything sized by the node count.

use rdirstat_core::{NodeId, Tree};

use crate::options::SizeMetric;

/// A set of content categories, as a 256-bit mask.
///
/// A `CategoryId` is a `u8`, so the whole domain fits in four words. Membership
/// is two shifts and an `and` — this is tested once per child link on the hot
/// path, and a `HashSet` there would cost more than the filtering saves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CategorySet {
    bits: [u64; 4],
}

impl CategorySet {
    /// The empty set. Matches nothing.
    pub const EMPTY: Self = Self { bits: [0; 4] };

    /// Builds a set from category ids. Duplicates and repeats are harmless.
    #[must_use]
    pub fn from_ids(ids: &[u8]) -> Self {
        let mut set = Self::EMPTY;
        for &id in ids {
            set.insert(id);
        }
        set
    }

    /// Adds one category.
    pub const fn insert(&mut self, category: u8) {
        let word = (category >> 6) as usize;
        let bit = category & 0b0011_1111;
        // `word` is 0..=3 by construction: a u8 shifted right by six.
        self.bits[word] |= 1_u64 << bit;
    }

    /// Whether `category` is in the set.
    #[must_use]
    pub const fn contains(self, category: u8) -> bool {
        let word = (category >> 6) as usize;
        let bit = category & 0b0011_1111;
        self.bits[word] & (1_u64 << bit) != 0
    }

    /// Whether the set matches nothing at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits[0] == 0 && self.bits[1] == 0 && self.bits[2] == 0 && self.bits[3] == 0
    }
}

/// Per-directory subtree bytes, counting only files whose category matches.
///
/// Public so a caller can compute it once and reuse it: it depends on
/// `(tree, root, metric, set)` and **not** on the viewport, so every layout in a
/// window resize can share one. That is the difference between paying the
/// `O(subtree)` pass once and paying it on every drag step.
#[derive(Debug)]
pub struct FilteredWeights {
    /// Indexed by `DirId::slot()`. Directories outside the walked subtree stay
    /// zero, which is the correct answer for them: they contribute no area to
    /// a layout rooted elsewhere.
    dirs: Vec<u64>,
    /// Kept so leaves can be tested without threading the set separately.
    set: CategorySet,
}

impl FilteredWeights {
    /// Computes filtered subtree bytes for every directory under `root`.
    #[must_use]
    pub fn build(tree: &Tree, root: NodeId, metric: SizeMetric, set: CategorySet) -> Self {
        let mut dirs = vec![0_u64; tree.dirs().len()];

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

        // Pass 2 — in reverse, so every child directory is already resolved.
        for &dir in order.iter().rev() {
            let mut total = 0_u64;
            for child in tree.children(dir) {
                let Some(node) = tree.node(child) else { continue };
                if node.kind().is_directory() {
                    if let Some(slot) = tree.dirs().dir_id(child).map(rdirstat_core::DirId::slot) {
                        total = total.saturating_add(dirs.get(slot).copied().unwrap_or(0));
                    }
                } else if set.contains(node.category().get()) {
                    // `contributed_*` already zeroes a repeated hard link, so
                    // the policy is not re-implemented here.
                    total = total.saturating_add(match metric {
                        SizeMetric::Allocated => node.contributed_alloc(),
                        SizeMetric::Logical => node.contributed_size(),
                    });
                }
            }
            if let Some(slot) = tree.dirs().dir_id(dir).map(rdirstat_core::DirId::slot)
                && let Some(cell) = dirs.get_mut(slot)
            {
                *cell = total;
            }
        }

        Self { dirs, set }
    }

    /// Whether a leaf's category survives the filter.
    pub(crate) const fn matches(&self, category: u8) -> bool {
        self.set.contains(category)
    }

    /// The category set these weights were built for.
    ///
    /// A cache needs this to answer "does the stored entry still apply", and
    /// deriving it from the entry rather than storing it separately is what
    /// stops the two drifting apart.
    #[must_use]
    pub const fn set(&self) -> CategorySet {
        self.set
    }

    /// Filtered bytes for a directory, or `0` if it is not one.
    pub(crate) fn directory(&self, tree: &Tree, node: NodeId) -> u64 {
        tree.dirs()
            .dir_id(node)
            .and_then(|id| self.dirs.get(id.slot()).copied())
            .unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;

    #[test]
    fn a_set_holds_the_whole_category_domain() {
        let mut set = CategorySet::EMPTY;
        for id in 0..=u8::MAX {
            assert!(!set.contains(id));
            set.insert(id);
            assert!(set.contains(id), "category {id} did not survive insertion");
        }
        assert!(!set.is_empty());
    }

    #[test]
    fn a_set_separates_the_four_words() {
        // 63/64 and 127/128 straddle word boundaries, which is where an
        // off-by-one in the shift would show up and nowhere else.
        for boundary in [63_u8, 64, 127, 128, 191, 192, 255] {
            let set = CategorySet::from_ids(&[boundary]);
            assert!(set.contains(boundary));
            for other in [0_u8, 1, 62, 65, 126, 129, 254] {
                if other != boundary {
                    assert!(!set.contains(other), "{boundary} leaked into {other}");
                }
            }
        }
    }

    #[test]
    fn an_empty_set_matches_nothing() {
        let set = CategorySet::EMPTY;
        assert!(set.is_empty());
        for id in [0_u8, 1, 7, 24, 255] {
            assert!(!set.contains(id));
        }
    }

    #[test]
    fn duplicate_ids_are_harmless() {
        let set = CategorySet::from_ids(&[3, 3, 3, 7]);
        assert!(set.contains(3));
        assert!(set.contains(7));
        assert!(!set.contains(4));
    }
}
