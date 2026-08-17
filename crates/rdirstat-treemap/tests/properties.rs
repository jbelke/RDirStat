//! Layout invariants under randomised inputs.
//!
//! The interesting failures are not "does the sample fixture work" but "is there
//! a size distribution or a viewport shape that puts a tile off screen, on top
//! of its sibling, or under the cutoff". These properties are asserted for every
//! layout kind.

#![allow(
    clippy::expect_used,
    reason = "clippy.toml sets allow-expect-in-tests, but that only covers #[cfg(test)] modules, \
              not an integration-test crate; every expect here carries a message"
)]

use proptest::prelude::{Just, ProptestConfig, Strategy, prop_oneof};
use proptest::{prop_assert, proptest};
use rdirstat_core::{DirTotals, Kind, LayoutKind, Node, NodeId, Tree, TreeBuilder, Viewport};
use rdirstat_treemap::{LayoutOptions, Tile, TileBuffer, layout_tiles};

const MTIME: i64 = 1_700_000_000;

/// One directory of files, plus one subdirectory holding the second half. Two
/// levels is enough to exercise nesting without making shrinking slow.
fn tree_of(sizes: &[u64]) -> Tree {
    let mut builder = TreeBuilder::new();
    let root_name = builder.intern(b"root").expect("root name");
    let root = builder.push_node(Node::directory(root_name, MTIME)).expect("root");
    builder.register_directory(root, DirTotals::EMPTY).expect("root totals");

    let split = sizes.len() / 2;
    let nested_name = builder.intern(b"nested").expect("nested name");
    let nested = builder
        .push_child(root, Node::directory(nested_name, MTIME))
        .expect("nested");
    builder
        .register_directory(nested, DirTotals::EMPTY)
        .expect("nested totals");

    for (index, bytes) in sizes.iter().enumerate() {
        let parent = if index < split { root } else { nested };
        let name = format!("f-{index:04}.bin");
        let reference = builder.intern(name.as_bytes()).expect("file name");
        builder
            .push_child(parent, Node::leaf(reference, Kind::File, *bytes, *bytes, MTIME))
            .expect("file");
        if let Some(totals) = builder.dir_totals_mut(parent) {
            totals.absorb_direct_file(*bytes, *bytes, MTIME);
        }
    }

    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

fn kinds() -> impl Strategy<Value = LayoutKind> {
    prop_oneof![
        Just(LayoutKind::Treemap),
        Just(LayoutKind::Icicle),
        Just(LayoutKind::Sunburst),
    ]
}

fn run(tree: &Tree, kind: LayoutKind, width: f32, height: f32, ratio: f32, min_px: f32) -> TileBuffer {
    let viewport = Viewport {
        width,
        height,
        device_pixel_ratio: ratio,
    };
    let options = LayoutOptions::new(kind, viewport, min_px).expect("a valid viewport");
    layout_tiles(tree, tree.root(), &options).expect("a layout")
}

fn overlap(left: &Tile, right: &Tile) -> bool {
    let epsilon = 1e-3_f32;
    left.x + left.w > right.x + epsilon
        && right.x + right.w > left.x + epsilon
        && left.y + left.h > right.y + epsilon
        && right.y + right.h > left.y + epsilon
}

/// Proptest's defaults — including `PROPTEST_CASES` — with one change: **no**
/// failure-persistence file. The default writes a `.proptest-regressions`
/// sibling, and this crate's tests are not allowed to create files outside a
/// `TempDir`. A shrunk counterexample still prints in full.
fn config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// No tile below the root is ever smaller than the cutoff on the axes that
    /// layout kind controls. This is the property the whole crate exists for.
    #[test]
    fn no_tile_is_below_the_cutoff(
        sizes in proptest::collection::vec(0_u64..1_000_000_000, 1..64),
        kind in kinds(),
        width in 64.0_f32..4_000.0,
        height in 64.0_f32..4_000.0,
        ratio in prop_oneof![Just(1.0_f32), Just(2.0_f32), Just(3.0_f32)],
        min_px in 1.0_f32..12.0,
    ) {
        let tree = tree_of(&sizes);
        let tiles = run(&tree, kind, width, height, ratio, min_px);
        for tile in tiles.iter().skip(1) {
            match kind {
                LayoutKind::Treemap => {
                    prop_assert!(tile.w * ratio >= min_px - 1e-2, "width {}", tile.w);
                    prop_assert!(tile.h * ratio >= min_px - 1e-2, "height {}", tile.h);
                }
                LayoutKind::Icicle => {
                    prop_assert!(tile.w * ratio >= min_px - 1e-2, "bar {}", tile.w);
                    prop_assert!(tile.h > 0.0, "row height {}", tile.h);
                }
                _ => {
                    // Arc length at the ring's inner radius.
                    prop_assert!(tile.w * tile.y * ratio >= min_px - 1e-1, "arc {}", tile.w * tile.y);
                    prop_assert!(tile.h > 0.0, "ring {}", tile.h);
                }
            }
        }
    }

    /// Nothing is drawn outside the surface the caller measured.
    #[test]
    fn every_tile_stays_inside_the_viewport(
        sizes in proptest::collection::vec(1_u64..1_000_000_000, 1..48),
        kind in kinds(),
        width in 64.0_f32..4_000.0,
        height in 64.0_f32..4_000.0,
    ) {
        let tree = tree_of(&sizes);
        let tiles = run(&tree, kind, width, height, 2.0, 3.0);
        for tile in tiles.iter() {
            if kind == LayoutKind::Sunburst {
                let radius = width.min(height) / 2.0;
                prop_assert!(tile.y >= -1e-3, "inner radius {}", tile.y);
                prop_assert!(tile.y + tile.h <= radius + 1e-2, "outer radius {}", tile.y + tile.h);
                prop_assert!(tile.x >= -std::f32::consts::FRAC_PI_2 - 1e-4, "start {}", tile.x);
                prop_assert!(
                    tile.x + tile.w <= -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU + 1e-4,
                    "end {}",
                    tile.x + tile.w
                );
            } else {
                prop_assert!(tile.x >= -1e-3 && tile.y >= -1e-3, "origin {} {}", tile.x, tile.y);
                prop_assert!(tile.x + tile.w <= width + 1e-2, "right {}", tile.x + tile.w);
                prop_assert!(tile.y + tile.h <= height + 1e-2, "bottom {}", tile.y + tile.h);
            }
        }
    }

    /// Siblings partition their parent; they never paint over each other.
    #[test]
    fn siblings_never_overlap(
        sizes in proptest::collection::vec(1_u64..1_000_000, 2..40),
        width in 200.0_f32..2_000.0,
        height in 200.0_f32..2_000.0,
    ) {
        let tree = tree_of(&sizes);
        let tiles = run(&tree, LayoutKind::Treemap, width, height, 2.0, 3.0);
        let level: Vec<Tile> = tiles.iter().filter(|tile| tile.depth == 1).collect();
        for (position, left) in level.iter().enumerate() {
            for right in level.iter().skip(position + 1) {
                prop_assert!(!overlap(left, right), "{} overlaps {}", left.node, right.node);
            }
        }
    }

    /// The same inputs always produce the same buffer — that is what makes a
    /// resize idempotent and a golden fixture possible.
    #[test]
    fn layouts_are_reproducible(
        sizes in proptest::collection::vec(1_u64..1_000_000_000, 1..48),
        kind in kinds(),
        width in 64.0_f32..4_000.0,
        height in 64.0_f32..4_000.0,
    ) {
        let tree = tree_of(&sizes);
        let first = run(&tree, kind, width, height, 2.0, 3.0);
        let second = run(&tree, kind, width, height, 2.0, 3.0);
        prop_assert!(first == second);
    }

    /// The tile count is bounded by the viewport, not by the tree. A larger
    /// directory in the same viewport cannot produce more tiles than the cutoff
    /// admits.
    #[test]
    fn the_drawn_set_is_bounded_by_the_viewport(
        count in 1_usize..600,
        width in 100.0_f32..1_200.0,
        height in 100.0_f32..1_200.0,
        min_px in 2.0_f32..8.0,
    ) {
        let ratio = 2.0_f32;
        let sizes: Vec<u64> = (0..count).map(|index| u64::try_from(index).unwrap_or(0) + 1).collect();
        let tree = tree_of(&sizes);
        let tiles = run(&tree, LayoutKind::Treemap, width, height, ratio, min_px);
        let device_area = f64::from(width * ratio) * f64::from(height * ratio);
        let ceiling = device_area / f64::from(min_px * min_px);
        prop_assert!(
            f64::from(u32::try_from(tiles.len()).unwrap_or(u32::MAX)) <= ceiling + 1.0,
            "{} tiles exceeds the {ceiling} the cutoff allows",
            tiles.len()
        );
    }

    /// A node whose weight is zero never gets a tile: zero bytes is zero area,
    /// which is how a repeated hard link stays out of the picture.
    #[test]
    fn zero_byte_children_are_never_drawn(
        heavy in 1_000_000_u64..1_000_000_000,
        zeros in 1_usize..16,
    ) {
        let mut sizes = vec![heavy];
        sizes.extend(core::iter::repeat_n(0_u64, zeros));
        let tree = tree_of(&sizes);
        let tiles = run(&tree, LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0);
        for tile in tiles.iter() {
            if let Some(node) = tree.node(tile.node)
                && !node.kind().is_directory()
            {
                prop_assert!(node.contributed_alloc() > 0, "{} has no bytes", tile.node);
            }
        }
        prop_assert!(tiles.tile(0).map(|tile| tile.node) == Some(NodeId::ROOT));
    }
}
