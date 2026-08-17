//! Size bands: "what are the big things", answered without reading a sorted
//! column.
//!
//! ## Why the edges are binary when the rest of the UI is decimal
//!
//! docs/05-UI.md pins displayed sizes to decimal SI, "matching Finder". The
//! band *edges* here are binary anyway, and the disagreement is deliberate.
//!
//! A band edge is not a measurement, it is a **name for a threshold** the user
//! already carries in their head, and that mental model comes from `du -h`,
//! which is 1024-based on both BSD and GNU. On this machine:
//!
//! ```text
//! du -h /Applications   ->  128G
//! this app              ->  138 GB
//! ```
//!
//! Same bytes. `138 × 10⁹ / 1024³ = 128.5`. Neither number is wrong and neither
//! is going to change, so a "50 GB" band defined in SI would sit at 46.6 GiB and
//! quietly disagree with the tool the user is checking against. Defining the
//! edge at exactly 50 GiB means "over 50G in `du`" and "in the ≥50 GiB band"
//! are the same set of files.
//!
//! The cost is that a band edge does not equal the round SI number beside it in
//! the size column. That is paid for by **always labelling both**: every edge
//! renders as `50 GiB (53.7 GB)`. The conversion is shown rather than assumed,
//! which is the only honest way to hold two unit systems on one screen.
//!
//! ## What gets banded
//!
//! Files, by their **allocated** bytes — the bytes the filesystem actually
//! spent, which is what `du` reports and therefore what the user's reference
//! point measures. Both totals are returned per band so a caller can display
//! logical alongside, but membership is allocated.
//!
//! Directories are deliberately **not** banded. A directory's size is its
//! subtree total, so banding both a directory and the files inside it would
//! count the same bytes twice and the band totals would not sum to the subtree
//! total. A band is a partition of the leaves, and it says so.

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::tree::Tree;

/// Band edges in bytes, ascending, each the **inclusive lower bound** of the
/// band above it.
///
/// Binary on purpose; see the module docs. These are the thresholds a person
/// actually reaches for — 5M, 50M, 500M, 5G, 50G in `du -h` — not a log scale
/// chosen for its arithmetic.
pub const SIZE_BAND_EDGES: [u64; 5] = [
    5 << 20,   // 5 MiB      5,242,880          "5.24 MB"
    50 << 20,  // 50 MiB     52,428,800         "52.4 MB"
    500 << 20, // 500 MiB    524,288,000        "524 MB"
    5 << 30,   // 5 GiB      5,368,709,120      "5.37 GB"
    50 << 30,  // 50 GiB     53,687,091,200     "53.7 GB"
];

/// One more band than there are edges: everything below the first edge is its
/// own band, and everything at or above the last edge is another.
pub const SIZE_BAND_COUNT: usize = SIZE_BAND_EDGES.len() + 1;

/// Which band `bytes` falls in, `0` being the smallest.
///
/// Bands are half-open `[lower, upper)`, so a file of exactly 5 MiB is in the
/// `5 MiB` band and not the one below it — the same way `du -h` rounds a file of
/// exactly 5,242,880 bytes to `5.0M`.
#[must_use]
pub fn size_band_of(bytes: u64) -> usize {
    SIZE_BAND_EDGES.partition_point(|edge| *edge <= bytes)
}

/// The inclusive lower edge of `band`, or `0` for the smallest.
#[must_use]
pub fn size_band_lower(band: usize) -> u64 {
    match band.checked_sub(1) {
        None => 0,
        Some(index) => SIZE_BAND_EDGES.get(index).copied().unwrap_or(u64::MAX),
    }
}

/// The exclusive upper edge of `band`, or `None` for the largest.
#[must_use]
pub fn size_band_upper(band: usize) -> Option<u64> {
    SIZE_BAND_EDGES.get(band).copied()
}

/// One row of the size-band histogram.
///
/// Carries the edges rather than a rendered label so that both front ends
/// format them with their own `format_iec`/`format_si`, instead of the backend
/// shipping display text that then cannot be re-styled or localised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct SizeBandRow {
    /// Band index, `0` smallest. Stable for a given [`SIZE_BAND_EDGES`].
    pub band: u8,
    /// Inclusive lower edge in bytes; `0` for the smallest band.
    pub lower_bytes: u64,
    /// Exclusive upper edge in bytes; `None` for the largest band.
    pub upper_bytes: Option<u64>,
    /// Files in this band.
    pub files: u64,
    /// Logical bytes of those files, after hard-link policy.
    pub logical: u64,
    /// Allocated bytes of those files, after hard-link policy. This is the
    /// quantity band membership is decided on.
    pub allocated: u64,
}

/// Buckets every file in the subtree at `root` into [`SIZE_BAND_COUNT`] bands.
///
/// Always returns exactly `SIZE_BAND_COUNT` rows, including empty ones: a band
/// that disappears when it has no members makes the table jump around as the
/// user drills, and "there is nothing over 50 GiB here" is an answer worth
/// showing rather than an absence to be inferred.
///
/// Returns `None` if `root` is not a node in this tree.
///
/// Iterative, never recursive — the same reason every other walk in this crate
/// is: a 4096-deep chain is a real input and a stack overflow is not a
/// diagnosable failure.
#[must_use]
pub fn size_bands(tree: &Tree, root: NodeId) -> Option<Vec<SizeBandRow>> {
    // A virtual `<Files>` group has no arena node of its own; band its owner.
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;

    let mut rows: Vec<SizeBandRow> = (0..SIZE_BAND_COUNT)
        .map(|band| SizeBandRow {
            band: u8::try_from(band).unwrap_or(u8::MAX),
            lower_bytes: size_band_lower(band),
            upper_bytes: size_band_upper(band),
            files: 0,
            logical: 0,
            allocated: 0,
        })
        .collect();

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
            let allocated = node.contributed_alloc();
            let band = size_band_of(allocated);
            if let Some(row) = rows.get_mut(band) {
                row.files = row.files.saturating_add(1);
                row.allocated = row.allocated.saturating_add(allocated);
                row.logical = row.logical.saturating_add(node.contributed_size());
            }
        }

        stack.extend(tree.children(id));
    }

    Some(rows)
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

    #[test]
    fn the_edges_are_binary_because_du_is_binary() {
        // The numbers a user reads off `du -h`. If these ever become SI, "over
        // 50G in du" and "in the top band" stop describing the same files.
        assert_eq!(SIZE_BAND_EDGES[0], 5 * 1024 * 1024);
        assert_eq!(SIZE_BAND_EDGES[2], 500 * 1024 * 1024);
        assert_eq!(SIZE_BAND_EDGES[4], 50 * 1024 * 1024 * 1024);
        assert_eq!(SIZE_BAND_EDGES[4], 53_687_091_200);
    }

    #[test]
    fn bands_are_half_open_upwards() {
        assert_eq!(size_band_of(0), 0);
        assert_eq!(size_band_of(SIZE_BAND_EDGES[0] - 1), 0);
        // Exactly on an edge belongs to the band above, matching `du -h`
        // rounding 5,242,880 bytes to "5.0M".
        assert_eq!(size_band_of(SIZE_BAND_EDGES[0]), 1);
        assert_eq!(size_band_of(SIZE_BAND_EDGES[4]), 5);
        assert_eq!(size_band_of(u64::MAX), SIZE_BAND_COUNT - 1);
    }

    #[test]
    fn every_band_has_an_edge_pair_and_only_the_last_is_open() {
        assert_eq!(size_band_lower(0), 0);
        for band in 0..SIZE_BAND_COUNT - 1 {
            let upper = size_band_upper(band).expect("a bounded band");
            assert_eq!(upper, size_band_lower(band + 1), "band {band} leaves a gap");
        }
        assert_eq!(size_band_upper(SIZE_BAND_COUNT - 1), None);
    }

    /// ```text
    /// root
    ///   tiny.txt      1 KiB        -> band 0
    ///   small.bin     6 MiB        -> band 1
    ///   sub/
    ///     medium.bin  600 MiB      -> band 3
    ///     huge.bin    60 GiB       -> band 5
    ///     clone.bin   60 GiB, hard-link repeat -> contributes 0, band 0
    /// ```
    fn fixture() -> (Tree, NodeId) {
        let mut builder = TreeBuilder::new();
        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 0)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        let mut file = |parent: NodeId, name: &[u8], bytes: u64, repeat: bool| {
            let reference = builder.intern(name).expect("interns");
            let mut node = Node::leaf(reference, Kind::File, bytes, bytes, 0);
            if repeat {
                node = node.with_flags(flags::HARD_LINK_REPEAT);
            }
            builder.push_child(parent, node).expect("links");
        };

        file(root, b"tiny.txt", 1 << 10, false);
        file(root, b"small.bin", 6 << 20, false);

        let sub_name = builder.intern(b"sub").expect("interns");
        let sub = builder
            .push_child(root, Node::directory(sub_name, 0))
            .expect("links");
        builder.register_directory(sub, DirTotals::EMPTY).expect("registers");

        let mut file = |parent: NodeId, name: &[u8], bytes: u64, repeat: bool| {
            let reference = builder.intern(name).expect("interns");
            let mut node = Node::leaf(reference, Kind::File, bytes, bytes, 0);
            if repeat {
                node = node.with_flags(flags::HARD_LINK_REPEAT);
            }
            builder.push_child(parent, node).expect("links");
        };
        file(sub, b"medium.bin", 600 << 20, false);
        file(sub, b"huge.bin", 60 << 30, false);
        file(sub, b"clone.bin", 60 << 30, true);

        (builder.finish().expect("valid"), root)
    }

    #[test]
    fn files_land_in_the_band_their_allocated_bytes_name() {
        let (tree, root) = fixture();
        let rows = size_bands(&tree, root).expect("a root");

        assert_eq!(rows.len(), SIZE_BAND_COUNT);
        assert_eq!(rows[1].files, 1, "6 MiB belongs to the 5 MiB band");
        assert_eq!(rows[3].files, 1, "600 MiB belongs to the 500 MiB band");
        assert_eq!(rows[5].files, 1, "60 GiB belongs to the 50 GiB band");
        assert_eq!(rows[2].files, 0, "nothing is 50-500 MiB here");
    }

    #[test]
    fn a_hard_link_repeat_contributes_no_bytes_and_no_size() {
        let (tree, root) = fixture();
        let rows = size_bands(&tree, root).expect("a root");

        // clone.bin is a second name for content already counted, so it lands in
        // band 0 on a contributed size of zero rather than inflating the top
        // band to 120 GiB.
        assert_eq!(rows[5].files, 1, "the repeat must not join the top band");
        assert_eq!(rows[5].allocated, 60 << 30);
        assert_eq!(rows[0].files, 2, "tiny.txt plus the zero-contribution repeat");
    }

    #[test]
    fn band_totals_sum_to_the_subtree_and_directories_are_not_counted_twice() {
        let (tree, root) = fixture();
        let rows = size_bands(&tree, root).expect("a root");

        let banded: u64 = rows.iter().map(|row| row.allocated).sum();
        let expected = (1 << 10) + (6 << 20) + (600 << 20) + (60_u64 << 30);
        assert_eq!(
            banded, expected,
            "band totals must partition the leaves exactly once"
        );
    }

    #[test]
    fn every_band_is_reported_even_when_empty() {
        let (tree, root) = fixture();
        let rows = size_bands(&tree, root).expect("a root");
        // A band that vanishes when empty makes the table jump as the user
        // drills, and "nothing over 50 GiB here" is an answer.
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(usize::from(row.band), index);
        }
    }

    #[test]
    fn an_unknown_node_has_no_bands() {
        let (tree, _) = fixture();
        assert!(size_bands(&tree, NodeId::from_raw(9_999)).is_none());
    }
}
