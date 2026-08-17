//! Types: which content categories occupy a subtree, and which files in one
//! category are the heavy ones.
//!
//! The sibling of [`bands`](crate::bands). Both answer "what is taking up the
//! room" from one traversal of the same subtree; bands partitions the files by
//! *how big each one is*, this partitions them by *what each one is*. The two
//! are deliberately built the same way, because the pair is read side by side
//! and a user who sums either column must reach the same number.
//!
//! ## What gets counted, and why it is only [`Kind::File`](crate::Kind::File)
//!
//! Files. Not directories, and — the part that is not obvious — not symlinks or
//! the special kinds either.
//!
//! Directories are excluded for the arithmetic reason bands states: a
//! directory's size *is* its subtree total, so counting a directory and the
//! files inside it would count the same bytes twice and the rows would no longer
//! partition the subtree.
//!
//! Symlinks are excluded for a different reason, and it is a trade rather than a
//! law. A symlink is a leaf with a few bytes of its own, so including it would
//! not double-count anything, and the compiled taxonomy does define a `Symlink`
//! category — which this report can therefore never show. That was weighed
//! against the alternative and lost: [`bands`](crate::bands) counts
//! `kind().is_file()` and nothing else, these two reports sit one click apart in
//! the same rail, and a grand total that differs between them by the size of the
//! symlink population is a discrepancy the user cannot explain and we cannot
//! defend. Agreeing with the neighbouring report is worth more than reaching a
//! category whose entire byte contribution on a boot volume is a few megabytes
//! of link targets, none of which is reclaimable.
//!
//! If that trade is ever revisited it must be revisited in **both** modules on
//! the same day.
//!
//! ## Why every category is reported but empty ones are not
//!
//! [`size_bands`](crate::bands::size_bands) returns all
//! [`SIZE_BAND_COUNT`](crate::SIZE_BAND_COUNT) rows including the empty ones,
//! because the band edges are defined ten lines above it: the set is closed,
//! tiny, and known, so "there is nothing over 50 GiB here" is an answer worth
//! rendering.
//!
//! Categories are the opposite on every count. The table lives in
//! `rdirstat-classify`, which depends on this crate and not the other way round,
//! so **`rdirstat-core` cannot enumerate the categories a build defines** — it
//! knows only that a [`CategoryId`] is a `u8`. Returning "every category" would
//! mean returning all [`CategoryId::MAX_CATEGORIES`] slots, of which the current
//! table names 25 and the remaining 231 are not even words. So this returns one
//! row per category that has at least one file, ordered largest first, and the
//! frontend resolves each id to a label and a colour through `src/lib/categories.ts`.
//!
//! "Has at least one file" is `files > 0`, not `allocated > 0`: forty thousand
//! empty `.DS_Store` files occupy no space and are still a thing worth seeing on
//! a Types report.
//!
//! ## Ordering
//!
//! Descending by allocated bytes, ties broken by ascending [`CategoryId`]. The
//! backend sorts because the answer is the whole set — there is no paging to
//! make it the frontend's problem — and a total order means the table does not
//! reshuffle when two categories happen to tie.

use serde::{Deserialize, Serialize};

use crate::id::{CategoryId, NodeId};
use crate::tree::Tree;

/// One row of the Types report: everything of one category in the subtree.
///
/// Carries the category *index*, never a label or a colour. docs/05-UI.md is
/// explicit that Rust sends indices and the frontend resolves the rest, which is
/// what keeps the palette themeable and the labels localisable without an IPC
/// round trip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct CategoryRow {
    /// The category, as assigned by `rdirstat-classify`.
    pub category: CategoryId,
    /// Files of this category in the subtree. Includes files that contribute
    /// zero bytes, such as empty files and repeated hard links.
    pub files: u64,
    /// Logical bytes of those files, after hard-link policy.
    pub logical: u64,
    /// Allocated bytes of those files, after hard-link policy. This is the
    /// quantity the rows are ordered by.
    pub allocated: u64,
}

/// Running per-category counters. Private: the wire shape is [`CategoryRow`],
/// and this exists only so the walk can index a flat array by `u8`.
#[derive(Clone, Copy, Debug)]
struct Running {
    files: u64,
    logical: u64,
    allocated: u64,
}

impl Running {
    const ZERO: Self = Self {
        files: 0,
        logical: 0,
        allocated: 0,
    };
}

/// Totals every file in the subtree at `root` by content category.
///
/// One row per category that has at least one file, descending by allocated
/// bytes. A category with no files is absent rather than zero — see the module
/// docs for why this differs from [`size_bands`](crate::bands::size_bands).
///
/// Returns `None` if `root` is not a node in this tree.
///
/// Iterative, never recursive. [`MAX_TREE_DEPTH`](crate::MAX_TREE_DEPTH) is
/// 4096 and a chain that deep is a real input on a real disk; a stack overflow
/// is a process abort, not a diagnosable failure.
#[must_use]
pub fn category_totals(tree: &Tree, root: NodeId) -> Option<Vec<CategoryRow>> {
    // A virtual `<Files>` group has no arena node of its own; total its owner.
    // Same concession bands makes, for the same reason: the group is a view of a
    // directory, so the only sane subtree to walk is that directory's.
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;

    // A flat array rather than a map: the key is a `u8`, so the whole domain is
    // 256 slots and 6 KiB of stack. A `HashMap` would hash on every one of tens
    // of millions of files to address a space that fits in a cache line count.
    let mut running = [Running::ZERO; CategoryId::MAX_CATEGORIES];

    let mut stack = vec![start];
    // The arena is finite and acyclic by `Tree`'s own freeze-time validation, so
    // this bound is a backstop against a tree that somehow escaped it rather
    // than an expected limit.
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);

    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;

        let Some(node) = tree.node(id) else { continue };

        if node.kind().is_file() {
            // `contributed_*` is the one place hard-link policy lives. A
            // repeated hard link answers zero here and is still counted as a
            // file, because the file exists and the user can see it.
            if let Some(slot) = running.get_mut(usize::from(node.category)) {
                slot.files = slot.files.saturating_add(1);
                slot.logical = slot.logical.saturating_add(node.contributed_size());
                slot.allocated = slot.allocated.saturating_add(node.contributed_alloc());
            }
        }

        stack.extend(tree.children(id));
    }

    let mut rows: Vec<CategoryRow> = running
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.files > 0)
        .map(|(index, slot)| CategoryRow {
            // `index` is bounded by the array length, which is
            // `MAX_CATEGORIES == 256`, so the conversion cannot lose anything.
            // It is still written as a fallible conversion rather than an `as`
            // cast: this crate denies `cast_possible_truncation` precisely so
            // that a future change to that constant fails loudly.
            category: CategoryId::from_raw(u8::try_from(index).unwrap_or(u8::MAX)),
            files: slot.files,
            logical: slot.logical,
            allocated: slot.allocated,
        })
        .collect();

    // Heaviest first, then by id. Total, so redraws are stable.
    rows.sort_unstable_by(|a, b| b.allocated.cmp(&a.allocated).then_with(|| a.category.cmp(&b.category)));
    Some(rows)
}

/// One file inside a category, for the breakdown.
///
/// Carries its resolved path because the whole point of expanding a category is
/// to find out *which* files are in it; a list of node ids would make the caller
/// issue one `path_of` per row.
///
/// There is deliberately no `category` field. It would be the same value on
/// every row of the list — it is the argument that produced the list — and a
/// constant column is not data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct CategoryEntry {
    /// The arena node, so a row can be selected, revealed, or trashed.
    pub node: NodeId,
    /// Full path, escaped for display. Never action authority: Reveal and Trash
    /// reconstruct the path in Rust (see [`DisplayPath`](crate::DisplayPath)).
    pub path: crate::wire::DisplayPath,
    /// Allocated bytes, after hard-link policy. The quantity this list is
    /// ordered by.
    pub allocated: u64,
    /// Logical bytes, after hard-link policy.
    pub logical: u64,
    /// Modification time in whole Unix seconds.
    pub mtime: i64,
}

/// The largest files of one category in the subtree, biggest first.
///
/// Bounded by `limit` on purpose, exactly as
/// [`size_band_entries`](crate::bands::size_band_entries) is. A single category
/// can hold tens of millions of files — `Cache` and `Object / Generated` both do
/// on a developer's boot volume — so this is a **leaderboard**, not an
/// enumeration. The caller already knows the true count from
/// [`category_totals`]; a full listing of ten million paths is not a UI, it is a
/// file dump.
///
/// Returns `None` if `root` is not a node in this tree. An empty vector is the
/// answer for a category with nothing in it, which is not an error: a category
/// that a build does not define and a category with no files here are the same
/// answer to the caller.
#[must_use]
pub fn category_entries(tree: &Tree, root: NodeId, category: CategoryId, limit: usize) -> Option<Vec<CategoryEntry>> {
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;
    if limit == 0 {
        return Some(Vec::new());
    }

    // Collect (allocated, node) for matching files, keeping only the heaviest
    // `limit`. A full sort of ten million entries to show two hundred and fifty
    // would be the expensive way to answer the same question.
    let mut best: Vec<(u64, NodeId)> = Vec::with_capacity(limit.min(1024));
    let mut floor = 0_u64;

    let mut stack = vec![start];
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);
    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(node) = tree.node(id) else { continue };

        if node.kind().is_file() && node.category() == category {
            let allocated = node.contributed_alloc();
            if best.len() < limit || allocated > floor {
                best.push((allocated, id));
                // Sorting on every insertion past the cap keeps the vector
                // bounded without a heap; `limit` is small (hundreds), so this
                // is cheaper than it looks and never allocates unboundedly.
                if best.len() > limit {
                    best.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    best.truncate(limit);
                    floor = best.last().map_or(0, |entry| entry.0);
                }
            }
        }

        stack.extend(tree.children(id));
    }

    best.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    best.truncate(limit);

    let mut out = Vec::with_capacity(best.len());
    let mut scratch = Vec::new();
    for (allocated, id) in best {
        let Some(node) = tree.node(id) else { continue };
        scratch.clear();
        // A path that cannot be reconstructed is skipped rather than shown
        // blank: a row naming no file is worse than a shorter list.
        if tree.path_bytes(id, &mut scratch).is_err() {
            continue;
        }
        out.push(CategoryEntry {
            node: id,
            path: crate::wire::DisplayPath::from_bytes(&scratch),
            allocated,
            logical: node.contributed_size(),
            mtime: node.mtime,
        });
    }
    Some(out)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;
    use crate::dirs::DirTotals;
    use crate::node::{Kind, Node, flags};
    use crate::tree::TreeBuilder;

    /// Raw ids from `rdirstat-classify`'s compiled table. Written as literals
    /// because `rdirstat-core` cannot depend on `classify` — the dependency runs
    /// the other way — and because the property under test is arithmetic on a
    /// `u8`, not the meaning of any particular id.
    const VIDEO: CategoryId = CategoryId::from_raw(11);
    const DOCUMENT: CategoryId = CategoryId::from_raw(13);
    const SOURCE: CategoryId = CategoryId::from_raw(14);
    const SYMLINK: CategoryId = CategoryId::from_raw(1);

    /// ```text
    /// root
    ///   notes.md       1 KiB    document
    ///   link           64 B     symlink   (Kind::Symlink — not counted)
    ///   sub/                              (a directory — not counted)
    ///     film.mov     60 GiB   video
    ///     clone.mov    60 GiB   video, hard-link repeat -> contributes 0
    ///     main.rs      4 KiB    source
    ///     lib.rs       2 KiB    source
    /// ```
    fn fixture() -> (Tree, NodeId) {
        let mut builder = TreeBuilder::new();
        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 0)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        let leaf = |builder: &mut TreeBuilder, parent: NodeId, name: &[u8], kind, bytes, category, repeat| {
            let reference = builder.intern(name).expect("interns");
            let mut node = Node::leaf(reference, kind, bytes, bytes, 0).with_category(category);
            if repeat {
                node = node.with_flags(flags::HARD_LINK | flags::HARD_LINK_REPEAT);
            }
            builder.push_child(parent, node).expect("links")
        };

        leaf(&mut builder, root, b"notes.md", Kind::File, 1 << 10, DOCUMENT, false);
        leaf(&mut builder, root, b"link", Kind::Symlink, 64, SYMLINK, false);

        let sub_name = builder.intern(b"sub").expect("interns");
        let sub = builder.push_child(root, Node::directory(sub_name, 0)).expect("links");
        builder.register_directory(sub, DirTotals::EMPTY).expect("registers");

        leaf(&mut builder, sub, b"film.mov", Kind::File, 60 << 30, VIDEO, false);
        leaf(&mut builder, sub, b"clone.mov", Kind::File, 60 << 30, VIDEO, true);
        leaf(&mut builder, sub, b"main.rs", Kind::File, 4 << 10, SOURCE, false);
        leaf(&mut builder, sub, b"lib.rs", Kind::File, 2 << 10, SOURCE, false);

        (builder.finish().expect("valid"), root)
    }

    fn row_for(rows: &[CategoryRow], category: CategoryId) -> Option<CategoryRow> {
        rows.iter().copied().find(|row| row.category == category)
    }

    #[test]
    fn each_file_is_totalled_under_the_category_it_carries() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        let source = row_for(&rows, SOURCE).expect("two .rs files");
        assert_eq!(source.files, 2);
        assert_eq!(source.allocated, (4 << 10) + (2 << 10));
        assert_eq!(source.logical, (4 << 10) + (2 << 10));

        let document = row_for(&rows, DOCUMENT).expect("one .md file");
        assert_eq!(document.files, 1);
        assert_eq!(document.allocated, 1 << 10);
    }

    #[test]
    fn category_totals_sum_to_the_subtree_and_directories_are_not_counted_twice() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        // `sub` is a directory whose subtree total is 60 GiB + 6 KiB. If a
        // directory were counted alongside the files inside it, this sum would
        // be roughly double.
        let totalled: u64 = rows.iter().map(|row| row.allocated).sum();
        let expected = (1 << 10) + (60_u64 << 30) + (4 << 10) + (2 << 10);
        assert_eq!(totalled, expected, "category rows must partition the files exactly once");
        assert_eq!(rows.iter().map(|row| row.files).sum::<u64>(), 5);
    }

    #[test]
    fn a_hard_link_repeat_is_listed_as_a_file_but_contributes_no_bytes() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        let video = row_for(&rows, VIDEO).expect("two .mov files");
        // Both films are visible entries, but the second name for content
        // already counted must not double the video total to 120 GiB.
        assert_eq!(video.files, 2);
        assert_eq!(video.allocated, 60 << 30);
        assert_eq!(video.logical, 60 << 30);
    }

    #[test]
    fn a_symlink_is_not_counted_because_the_size_report_does_not_count_one_either() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        // Deliberate, and documented in the module header: the Types report and
        // the Sizes report must agree on their grand total, and `bands` counts
        // `Kind::File` and nothing else. Changing this changes both modules.
        assert_eq!(row_for(&rows, SYMLINK), None);
    }

    #[test]
    fn categories_with_no_files_are_absent_rather_than_present_as_zero_rows() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        // Three categories have files. The remaining 253 `CategoryId` values are
        // not rows: this crate does not own the category table and cannot say
        // which of them a build even names.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.files > 0));
    }

    #[test]
    fn rows_are_ordered_by_allocated_bytes_with_a_total_tie_break() {
        let (tree, root) = fixture();
        let rows = category_totals(&tree, root).expect("a root");

        assert_eq!(rows.first().map(|row| row.category), Some(VIDEO), "60 GiB leads");
        for pair in rows.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            assert!(
                left.allocated > right.allocated || (left.allocated == right.allocated && left.category < right.category),
                "ordering must be total so the table does not reshuffle on redraw"
            );
        }
    }

    #[test]
    fn the_leaderboard_lists_only_that_category_largest_first() {
        let (tree, root) = fixture();
        let entries = category_entries(&tree, root, SOURCE, 10).expect("a root");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].allocated, 4 << 10);
        assert_eq!(entries[1].allocated, 2 << 10);
        // Full reconstructed paths, rooted at the scan root's own name — the
        // point of returning a path at all is that the row names a real file.
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["root/sub/main.rs", "root/sub/lib.rs"]);
    }

    #[test]
    fn the_leaderboard_is_a_bounded_head_not_an_enumeration() {
        let (tree, root) = fixture();
        assert_eq!(category_entries(&tree, root, SOURCE, 1).expect("a root").len(), 1);
        // Zero is "ask for nothing", not "ask for everything".
        assert!(category_entries(&tree, root, SOURCE, 0).expect("a root").is_empty());
    }

    #[test]
    fn a_hard_link_repeat_sinks_to_the_bottom_of_its_leaderboard() {
        let (tree, root) = fixture();
        let entries = category_entries(&tree, root, VIDEO, 10).expect("a root");

        // Both entries appear — the file is on disk under both names — but the
        // repeat is ranked on its contribution, which is zero, so the name that
        // actually accounts for the bytes is the one the user sees first.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].allocated, 60 << 30);
        assert!(entries[0].path.as_str().ends_with("film.mov"), "{:?}", entries[0].path);
        assert_eq!(entries[1].allocated, 0);
    }

    #[test]
    fn a_category_with_nothing_in_it_is_an_empty_list_rather_than_a_failure() {
        let (tree, root) = fixture();
        let entries = category_entries(&tree, root, CategoryId::from_raw(200), 10).expect("a root");
        assert!(entries.is_empty());
    }

    #[test]
    fn a_virtual_files_group_is_totalled_as_the_directory_that_owns_it() {
        let (tree, root) = fixture();
        // Tagged directly rather than via `Tree::virtual_group`, which also
        // requires the fixture's `DirTotals` to claim direct files. The property
        // under test is that a tagged id resolves to its owner, not that this
        // particular directory offers a group.
        let group = NodeId::virtual_group_of(root).expect("root is an arena slot");

        // A group is a view, not an arena node, so the only subtree it can name
        // is its owner's. Answering `None` instead would make the Types route go
        // blank the moment a `<Files>` row is selected.
        assert_eq!(category_totals(&tree, group), category_totals(&tree, root));
        assert_eq!(
            category_entries(&tree, group, SOURCE, 10),
            category_entries(&tree, root, SOURCE, 10)
        );
    }

    #[test]
    fn an_unknown_node_has_no_totals_and_no_entries() {
        let (tree, _) = fixture();
        let ghost = NodeId::from_raw(9_999);
        assert!(category_totals(&tree, ghost).is_none());
        assert!(category_entries(&tree, ghost, SOURCE, 10).is_none());
    }

    #[test]
    fn a_chain_as_deep_as_the_arena_allows_is_walked_to_the_bottom() {
        // `MAX_TREE_DEPTH` is 4096 and directory chains that deep exist on real
        // disks (node_modules, DerivedData). This fixture is the whole reason
        // both walks above use an explicit stack: a recursive version does not
        // return a wrong answer here, it aborts the process.
        const CHAIN: usize = 4096;

        let mut builder = TreeBuilder::with_capacity(CHAIN + 1, CHAIN * 8, CHAIN);
        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 0)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        let mut deepest = root;
        for _ in 1..CHAIN {
            let name = builder.intern(b"d").expect("interns");
            deepest = builder
                .push_child(deepest, Node::directory(name, 0))
                .expect("links");
            builder.register_directory(deepest, DirTotals::EMPTY).expect("registers");
        }
        let bottom_name = builder.intern(b"bottom.mov").expect("interns");
        builder
            .push_child(
                deepest,
                Node::leaf(bottom_name, Kind::File, 7, 7, 0).with_category(VIDEO),
            )
            .expect("links");

        let tree = builder.finish().expect("valid");
        let rows = category_totals(&tree, root).expect("a root");
        assert_eq!(row_for(&rows, VIDEO).map(|row| row.files), Some(1));
        assert_eq!(category_entries(&tree, root, VIDEO, 4).expect("a root").len(), 1);
    }
}
