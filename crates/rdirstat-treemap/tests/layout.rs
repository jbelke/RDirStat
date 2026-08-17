//! Layout behaviour against real frozen trees.
//!
//! Every fixture is built in memory with [`TreeBuilder`]; the one test that
//! touches the filesystem writes its golden Arrow stream into a [`TempDir`] and
//! nothing is ever written outside it.

#![allow(
    clippy::expect_used,
    reason = "clippy.toml sets allow-expect-in-tests, but that only covers #[cfg(test)] modules, \
              not an integration-test crate; every expect here carries a message, and \
              allow-unwrap-in-tests stays false so bare unwrap is still rejected"
)]

use arrow::ipc::reader::StreamReader;
use rdirstat_core::{
    ARROW_META_GENERATION, ARROW_META_SCHEMA_NAME, ArenaError, DirTotals, Kind, LAYOUT_COLUMNS, LayoutKind, Node,
    NodeId, Tree, TreeBuilder, TreeGeneration, Viewport,
};
use rdirstat_treemap::{
    ICICLE_ROW_PX, LayoutOptions, SUNBURST_RING_PX, SizeMetric, Tile, TileBuffer, layout, layout_tiles,
};
use std::collections::HashMap;
use tempfile::TempDir;

const MTIME: i64 = 1_700_000_000;

// ---------------------------------------------------------------- fixtures --

fn push_dir(builder: &mut TreeBuilder, parent: Option<NodeId>, name: &[u8]) -> Result<NodeId, ArenaError> {
    let reference = builder.intern(name)?;
    let node = Node::directory(reference, MTIME);
    let id = match parent {
        Some(parent) => builder.push_child(parent, node)?,
        None => builder.push_node(node)?,
    };
    builder.register_directory(id, DirTotals::EMPTY)?;
    Ok(id)
}

fn push_file(builder: &mut TreeBuilder, parent: NodeId, name: &[u8], bytes: u64) -> Result<NodeId, ArenaError> {
    let reference = builder.intern(name)?;
    let id = builder.push_child(parent, Node::leaf(reference, Kind::File, bytes, bytes, MTIME))?;
    if let Some(totals) = builder.dir_totals_mut(parent) {
        totals.absorb_direct_file(bytes, bytes, MTIME);
    }
    Ok(id)
}

/// ```text
/// root
///   a.bin   1_000
///   b.bin     500
///   media/
///     big.mkv 4_000
///     mid.mkv 2_000
///   empty/
/// ```
fn sample_tree() -> Tree {
    let mut builder = TreeBuilder::new();
    let root = push_dir(&mut builder, None, b"root").expect("root");
    push_file(&mut builder, root, b"a.bin", 1_000).expect("a.bin");
    push_file(&mut builder, root, b"b.bin", 500).expect("b.bin");
    let media = push_dir(&mut builder, Some(root), b"media").expect("media");
    push_file(&mut builder, media, b"big.mkv", 4_000).expect("big.mkv");
    push_file(&mut builder, media, b"mid.mkv", 2_000).expect("mid.mkv");
    push_dir(&mut builder, Some(root), b"empty").expect("empty");
    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

/// One heavy file beside a long tail of one-byte dust.
fn dusty_tree(dust: usize) -> Tree {
    let mut builder = TreeBuilder::new();
    let root = push_dir(&mut builder, None, b"root").expect("root");
    push_file(&mut builder, root, b"huge.iso", 1_000_000_000).expect("huge.iso");
    for index in 0..dust {
        let name = format!("dust-{index}.tmp");
        push_file(&mut builder, root, name.as_bytes(), 1).expect("dust");
    }
    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

/// A single chain `root/d0/d1/.../d{levels-1}/leaf.bin`.
fn deep_tree(levels: usize) -> Tree {
    let mut builder = TreeBuilder::new();
    let root = push_dir(&mut builder, None, b"root").expect("root");
    let mut cursor = root;
    for level in 0..levels {
        let name = format!("d{level}");
        cursor = push_dir(&mut builder, Some(cursor), name.as_bytes()).expect("level");
    }
    push_file(&mut builder, cursor, b"leaf.bin", 4_096).expect("leaf");
    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

fn viewport(width: f32, height: f32, ratio: f32) -> Viewport {
    Viewport {
        width,
        height,
        device_pixel_ratio: ratio,
    }
}

fn options(kind: LayoutKind, width: f32, height: f32, ratio: f32, min_px: f32) -> LayoutOptions {
    LayoutOptions::new(kind, viewport(width, height, ratio), min_px).expect("valid options")
}

fn tiles_of(tree: &Tree, kind: LayoutKind) -> TileBuffer {
    layout_tiles(tree, tree.root(), &options(kind, 800.0, 600.0, 2.0, 3.0)).expect("a layout")
}

fn by_node(tiles: &TileBuffer) -> HashMap<u32, Tile> {
    tiles.iter().map(|tile| (tile.node.raw(), tile)).collect()
}

fn overlaps(left: &Tile, right: &Tile) -> bool {
    let epsilon = 1e-3_f32;
    left.x + left.w > right.x + epsilon
        && right.x + right.w > left.x + epsilon
        && left.y + left.h > right.y + epsilon
        && right.y + right.h > left.y + epsilon
}

// ----------------------------------------------------------------- treemap --

#[test]
fn the_root_tile_covers_the_viewport() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Treemap);
    let root = tiles.tile(0).expect("at least the root");
    assert_eq!(root.node, tree.root());
    assert_eq!(root.depth, 0);
    assert!((root.x).abs() < f32::EPSILON && (root.y).abs() < f32::EPSILON);
    assert!((root.w - 800.0).abs() < 1e-3 && (root.h - 600.0).abs() < 1e-3);
}

#[test]
fn treemap_tile_areas_are_proportional_to_allocated_bytes() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Treemap);
    let index = by_node(&tiles);
    let total = f64::from(u32::try_from(tree.allocated_of(tree.root())).expect("small fixture"));
    let viewport_area = 800.0_f64 * 600.0;

    for node in tree.children(tree.root()) {
        let bytes = tree.allocated_of(node);
        if bytes == 0 {
            continue;
        }
        let tile = index.get(&node.raw()).expect("every non-empty child is drawn");
        let expected = f64::from(u32::try_from(bytes).expect("small fixture")) / total * viewport_area;
        let actual = f64::from(tile.w) * f64::from(tile.h);
        assert!(
            (actual - expected).abs() <= expected * 1e-3,
            "node {node} area {actual} != {expected}"
        );
    }
}

#[test]
fn treemap_children_stay_inside_their_parent_and_do_not_overlap_siblings() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Treemap);
    let index = by_node(&tiles);

    for tile in tiles.iter() {
        if let Some(parent) = tree.parent(tile.node)
            && let Some(parent_tile) = index.get(&parent.raw())
        {
            assert!(tile.x >= parent_tile.x - 1e-3, "{} escapes left", tile.node);
            assert!(tile.y >= parent_tile.y - 1e-3, "{} escapes top", tile.node);
            assert!(
                tile.x + tile.w <= parent_tile.x + parent_tile.w + 1e-3,
                "{} escapes right",
                tile.node
            );
            assert!(
                tile.y + tile.h <= parent_tile.y + parent_tile.h + 1e-3,
                "{} escapes bottom",
                tile.node
            );
        }
    }

    let siblings: Vec<Tile> = tiles.iter().filter(|tile| tile.depth == 1).collect();
    for (position, left) in siblings.iter().enumerate() {
        for right in siblings.iter().skip(position + 1) {
            assert!(!overlaps(left, right), "{} overlaps {}", left.node, right.node);
        }
    }
}

#[test]
fn no_treemap_tile_below_the_root_is_smaller_than_the_cutoff() {
    let tree = sample_tree();
    let min_px = 3.0_f32;
    let ratio = 2.0_f32;
    let tiles = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 800.0, 600.0, ratio, min_px),
    )
    .expect("a layout");
    for tile in tiles.iter().skip(1) {
        assert!(tile.w * ratio >= min_px - 1e-3, "{} is {} px wide", tile.node, tile.w);
        assert!(tile.h * ratio >= min_px - 1e-3, "{} is {} px tall", tile.node, tile.h);
    }
}

#[test]
fn the_cutoff_bounds_a_wide_directory_to_a_handful_of_tiles() {
    let tree = dusty_tree(5_000);
    let tiles = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 200.0, 200.0, 1.0, 3.0),
    )
    .expect("a layout");
    // Root plus the one file that is not dust. 5,000 candidates, 2 draw calls.
    assert_eq!(tiles.len(), 2, "the sub-pixel cutoff did not bound the drawn set");
    assert!(tiles.stats().considered >= 5_000, "the walk must still see every child");
    assert!(!tiles.stats().truncated);
}

#[test]
fn the_tile_backstop_truncates_rather_than_growing_without_limit() {
    let tree = sample_tree();
    let capped = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0).with_max_tiles(2),
    )
    .expect("a layout");
    assert_eq!(capped.len(), 2);
    assert!(capped.stats().truncated);
}

#[test]
fn rows_are_emitted_in_paint_order_parent_before_descendant() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Treemap);
    let mut row_of: HashMap<u32, usize> = HashMap::new();
    for (row, tile) in tiles.iter().enumerate() {
        row_of.insert(tile.node.raw(), row);
    }
    for (row, tile) in tiles.iter().enumerate() {
        if let Some(parent) = tree.parent(tile.node)
            && let Some(parent_row) = row_of.get(&parent.raw())
        {
            assert!(*parent_row < row, "{} is painted before its parent", tile.node);
        }
    }
}

// ------------------------------------------------------------------ icicle --

#[test]
fn icicle_rows_are_uniform_and_keyed_to_depth() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Icicle);
    let root = tiles.tile(0).expect("a root tile");
    let step = root.h;
    assert!(step >= ICICLE_ROW_PX - 1e-3, "row height {step} collapsed");
    for tile in tiles.iter() {
        assert!((tile.h - step).abs() < 1e-3, "row heights differ");
        #[allow(clippy::cast_precision_loss, reason = "fixture depth is tiny")]
        let expected = tile.depth as f32 * step;
        assert!((tile.y - expected).abs() < 1e-3, "y is not depth x row height");
    }
}

#[test]
fn icicle_siblings_tile_their_parents_extent_without_overlap() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Icicle);
    let index = by_node(&tiles);
    let root = tiles.tile(0).expect("a root tile");
    assert!((root.x).abs() < f32::EPSILON);
    assert!((root.w - 800.0).abs() < 1e-3);

    for tile in tiles.iter() {
        if let Some(parent) = tree.parent(tile.node)
            && let Some(parent_tile) = index.get(&parent.raw())
        {
            assert!(tile.x >= parent_tile.x - 1e-3);
            assert!(tile.x + tile.w <= parent_tile.x + parent_tile.w + 1e-3);
        }
    }

    let mut level: Vec<Tile> = tiles.iter().filter(|tile| tile.depth == 1).collect();
    level.sort_by(|left, right| left.x.total_cmp(&right.x));
    for pair in level.windows(2) {
        if let [left, right] = pair {
            assert!(left.x + left.w <= right.x + 1e-3, "icicle bars overlap");
        }
    }
}

#[test]
fn the_icicle_stops_at_the_row_budget_rather_than_drawing_off_screen() {
    let tree = deep_tree(200);
    let height = 600.0_f32;
    let tiles = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Icicle, 800.0, height, 2.0, 3.0),
    )
    .expect("a layout");
    assert!(tiles.len() < 200, "the depth budget did not bind");
    for tile in tiles.iter() {
        assert!(tile.y + tile.h <= height + 1e-3, "row {} is off screen", tile.depth);
    }
}

// ---------------------------------------------------------------- sunburst --

#[test]
fn the_sunburst_root_is_a_full_turn_starting_at_twelve_oclock() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Sunburst);
    let root = tiles.tile(0).expect("a root tile");
    assert!(
        (root.x - -std::f32::consts::FRAC_PI_2).abs() < 1e-5,
        "start angle {}",
        root.x
    );
    assert!((root.w - std::f32::consts::TAU).abs() < 1e-5, "sweep {}", root.w);
    assert!((root.y).abs() < f32::EPSILON, "the root disc starts at the centre");
    assert!(root.h >= SUNBURST_RING_PX - 1e-3, "ring thickness {}", root.h);
}

#[test]
fn sunburst_rings_are_concentric_and_bounded_by_the_inscribed_circle() {
    let tree = sample_tree();
    let width = 800.0_f32;
    let height = 600.0_f32;
    let tiles = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Sunburst, width, height, 2.0, 3.0),
    )
    .expect("a layout");
    let radius = width.min(height) / 2.0;
    let step = tiles.tile(0).expect("a root tile").h;
    for tile in tiles.iter() {
        #[allow(clippy::cast_precision_loss, reason = "fixture depth is tiny")]
        let expected = tile.depth as f32 * step;
        assert!((tile.y - expected).abs() < 1e-3, "inner radius is not depth x ring");
        assert!(tile.y + tile.h <= radius + 1e-3, "ring escapes the inscribed circle");
    }
}

#[test]
fn sunburst_child_sweeps_stay_inside_the_parent_arc() {
    let tree = sample_tree();
    let tiles = tiles_of(&tree, LayoutKind::Sunburst);
    let index = by_node(&tiles);
    for tile in tiles.iter() {
        if let Some(parent) = tree.parent(tile.node)
            && let Some(parent_tile) = index.get(&parent.raw())
        {
            assert!(tile.x >= parent_tile.x - 1e-5, "{} starts before its parent", tile.node);
            assert!(
                tile.x + tile.w <= parent_tile.x + parent_tile.w + 1e-5,
                "{} sweeps past its parent",
                tile.node
            );
        }
    }
}

#[test]
fn the_sunburst_is_the_icicle_triple_reordered_not_a_different_traversal() {
    // Same tree, same viewport: the two layouts must draw the same *set* of
    // nodes whenever no arc is lost to angular resolution. A shallow fixture in
    // a large viewport is exactly that case.
    let tree = sample_tree();
    let icicle = tiles_of(&tree, LayoutKind::Icicle);
    let sunburst = tiles_of(&tree, LayoutKind::Sunburst);
    let mut left: Vec<(u32, u32)> = icicle.iter().map(|tile| (tile.node.raw(), tile.depth)).collect();
    let mut right: Vec<(u32, u32)> = sunburst.iter().map(|tile| (tile.node.raw(), tile.depth)).collect();
    left.sort_unstable();
    right.sort_unstable();
    assert_eq!(left, right);
}

// ----------------------------------------------------------------- shared ---

#[test]
fn every_layout_kind_is_deterministic_for_the_same_inputs() {
    let tree = sample_tree();
    for kind in [LayoutKind::Treemap, LayoutKind::Icicle, LayoutKind::Sunburst] {
        let first = layout(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            kind,
            viewport(800.0, 600.0, 2.0),
            3.0,
        )
        .expect("a response");
        let second = layout(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            kind,
            viewport(800.0, 600.0, 2.0),
            3.0,
        )
        .expect("a response");
        assert_eq!(first.bytes, second.bytes, "{kind:?} is not byte-for-byte stable");
    }
}

#[test]
fn geometry_does_not_depend_on_arena_sibling_order() {
    // Insertion order is head-first and explicitly not a UI contract, so the
    // same directory built in the opposite order must lay out identically.
    fn build(ascending: bool) -> Tree {
        let sizes: [(&[u8], u64); 3] = [(b"x.bin", 4_000), (b"y.bin", 2_000), (b"z.bin", 1_000)];
        let mut builder = TreeBuilder::new();
        let root = push_dir(&mut builder, None, b"root").expect("root");
        if ascending {
            for (name, bytes) in sizes {
                push_file(&mut builder, root, name, bytes).expect("file");
            }
        } else {
            for (name, bytes) in sizes.iter().rev() {
                push_file(&mut builder, root, name, *bytes).expect("file");
            }
        }
        builder.rollup().expect("rollup");
        builder.finish().expect("finish")
    }

    let forward = tiles_of(&build(true), LayoutKind::Treemap);
    let backward = tiles_of(&build(false), LayoutKind::Treemap);
    assert_eq!(forward.len(), backward.len());
    assert_eq!(forward.xs(), backward.xs());
    assert_eq!(forward.ys(), backward.ys());
    assert_eq!(forward.ws(), backward.ws());
    assert_eq!(forward.hs(), backward.hs());
}

#[test]
fn logical_and_allocated_are_separate_layout_inputs() {
    // A sparse file: 1 GB logical, 4 KiB allocated. The two metrics must not be
    // blended, and picking one must visibly change the picture.
    let mut builder = TreeBuilder::new();
    let root = push_dir(&mut builder, None, b"root").expect("root");
    let sparse = builder.intern(b"sparse.img").expect("name");
    let node = builder
        .push_child(
            root,
            Node::leaf(sparse, Kind::File, 1_000_000_000, 4_096, MTIME).with_flags(rdirstat_core::flags::SPARSE),
        )
        .expect("sparse file");
    if let Some(totals) = builder.dir_totals_mut(root) {
        totals.absorb_direct_file(1_000_000_000, 4_096, MTIME);
    }
    let dense = push_file(&mut builder, root, b"dense.bin", 4_096).expect("dense");
    builder.rollup().expect("rollup");
    let tree = builder.finish().expect("finish");

    let allocated = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0).with_metric(SizeMetric::Allocated),
    )
    .expect("a layout");
    let logical = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0).with_metric(SizeMetric::Logical),
    )
    .expect("a layout");

    let area =
        |tiles: &TileBuffer, id: NodeId| -> Option<f32> { by_node(tiles).get(&id.raw()).map(|tile| tile.w * tile.h) };

    // Allocated: the sparse file is the same 4 KiB on disk as the dense one, so
    // the two tiles are the same size.
    let sparse_area = area(&allocated, node).expect("sparse is drawn under allocated");
    let dense_area = area(&allocated, dense).expect("dense is drawn under allocated");
    let ratio = sparse_area / dense_area;
    assert!((ratio - 1.0).abs() < 0.05, "allocated areas differ, ratio {ratio}");

    // Logical: the sparse file claims a gigabyte, swamps the picture, and pushes
    // its 4 KiB neighbour under the sub-pixel cutoff.
    assert!(area(&logical, node).is_some(), "sparse must be drawn under logical");
    assert!(
        area(&logical, dense).is_none(),
        "4 KiB beside 1 GB is sub-pixel and must not be drawn"
    );
}

#[test]
fn a_virtual_files_group_lays_out_only_the_direct_files_of_its_owner() {
    let tree = sample_tree();
    let group = tree.virtual_group(tree.root()).expect("root has direct files");
    let tiles = layout_tiles(&tree, group, &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0)).expect("a layout");
    assert_eq!(tiles.tile(0).map(|tile| tile.node), Some(group));
    for tile in tiles.iter().skip(1) {
        let node = tree.node(tile.node).expect("a real node");
        assert!(!node.kind().is_directory(), "a group must not contain directories");
        assert_eq!(tree.parent(tile.node), Some(tree.root()));
    }
    assert_eq!(tiles.len(), 3, "the group plus its two files");
}

#[test]
fn an_unknown_root_is_an_error_not_an_empty_canvas() {
    let tree = sample_tree();
    let missing = NodeId::from_index(9_999).expect("a valid index");
    let error = layout(
        &tree,
        TreeGeneration::FIRST,
        missing,
        LayoutKind::Treemap,
        viewport(800.0, 600.0, 2.0),
        3.0,
    )
    .expect_err("an unknown node must fail");
    assert!(
        matches!(error, rdirstat_core::QueryError::UnknownNode { .. }),
        "{error:?}"
    );
}

#[test]
fn an_unmeasured_canvas_yields_an_empty_batch_rather_than_an_error() {
    // A React canvas reports 0 x 0 before its resize observer fires. That must
    // not surface as an error to the user.
    let tree = sample_tree();
    for size in [viewport(0.0, 600.0, 2.0), viewport(800.0, 0.0, 2.0)] {
        let response = layout(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Treemap,
            size,
            3.0,
        )
        .expect("an unmeasured canvas is a state, not an error");
        let reader = StreamReader::try_new(response.bytes.as_slice(), None).expect("a readable stream");
        assert_eq!(reader.schema().fields().len(), 7);
        let rows: usize = reader.map(|batch| batch.expect("a decodable batch").num_rows()).sum();
        assert_eq!(rows, 0);
    }
}

#[test]
fn a_nonsense_viewport_or_cutoff_is_rejected() {
    let tree = sample_tree();
    // NaN is a bug, not an unmeasured canvas.
    assert!(
        layout(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Treemap,
            viewport(f32::NAN, 600.0, 2.0),
            3.0,
        )
        .is_err()
    );
    // A zero cutoff would unbound the drawn set, so it is rejected rather than
    // silently replaced with a different one.
    assert!(
        layout(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Treemap,
            viewport(800.0, 600.0, 2.0),
            0.0,
        )
        .is_err()
    );
}

#[test]
fn a_leaf_root_draws_exactly_one_tile() {
    let tree = sample_tree();
    let leaf = tree
        .children(tree.root())
        .find(|id| tree.node(*id).is_some_and(|node| node.kind().is_file()))
        .expect("a file child");
    let tiles = layout_tiles(&tree, leaf, &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 3.0)).expect("a layout");
    assert_eq!(tiles.len(), 1);
}

// -------------------------------------------------------------------- ipc ---

#[test]
fn the_response_round_trips_through_a_file_on_disk() {
    let temp = TempDir::new().expect("a temp dir");
    let path = temp.path().join("layout.arrows");

    let tree = sample_tree();
    let generation = TreeGeneration::from_raw(17);
    let response = layout(
        &tree,
        generation,
        tree.root(),
        LayoutKind::Treemap,
        viewport(800.0, 600.0, 2.0),
        3.0,
    )
    .expect("a response");
    let expected_rows = tiles_of(&tree, LayoutKind::Treemap).len();

    std::fs::write(&path, response.into_bytes()).expect("write the golden stream");
    let file = std::fs::File::open(&path).expect("reopen the golden stream");
    let reader = StreamReader::try_new(file, None).expect("a readable stream");
    let schema = reader.schema();

    let names: Vec<&str> = schema.fields().iter().map(|field| field.name().as_str()).collect();
    assert_eq!(names, LAYOUT_COLUMNS.to_vec());
    assert_eq!(
        schema.metadata().get(ARROW_META_GENERATION).map(String::as_str),
        Some("17")
    );
    assert_eq!(
        schema.metadata().get(ARROW_META_SCHEMA_NAME).map(String::as_str),
        Some("layout")
    );

    let rows: usize = reader.map(|batch| batch.expect("a decodable batch").num_rows()).sum();
    assert_eq!(rows, expected_rows);
}

// ------------------------------------------------------------------ scale ---

/// A deterministic, heavy-tailed pseudo-random source. No `rand` dependency:
/// the fixture must be reproducible byte for byte across machines.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    /// Sizes spanning eight orders of magnitude, which is what a real volume
    /// looks like and what makes the cutoff bite.
    fn size(&mut self) -> u64 {
        let magnitude = self.next() % 9;
        let base = self.next() % 1_000;
        (base + 1) * 10_u64.pow(u32::try_from(magnitude).unwrap_or(0))
    }
}

/// `dirs` subdirectories under the root, each holding `files` files.
fn wide_tree(dirs: usize, files: usize) -> Tree {
    let mut rng = Lcg(0x5EED_1234_ABCD_0001);
    let mut builder = TreeBuilder::with_capacity(dirs * (files + 1) + 1, 32 * dirs * (files + 1), dirs + 1);
    let root = push_dir(&mut builder, None, b"root").expect("root");
    for dir_index in 0..dirs {
        let dir_name = format!("dir-{dir_index:05}");
        let dir = push_dir(&mut builder, Some(root), dir_name.as_bytes()).expect("a directory");
        for file_index in 0..files {
            let file_name = format!("f-{dir_index:05}-{file_index:05}.bin");
            push_file(&mut builder, dir, file_name.as_bytes(), rng.size()).expect("a file");
        }
    }
    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

#[test]
fn the_cutoff_bounds_every_layout_kind_on_a_fifty_thousand_node_tree() {
    let tree = wide_tree(200, 250);
    assert!(tree.len() > 50_000, "fixture is {} nodes", tree.len());

    let min_px = 3.0_f32;
    let ratio = 2.0_f32;
    for kind in [LayoutKind::Treemap, LayoutKind::Icicle, LayoutKind::Sunburst] {
        let tiles =
            layout_tiles(&tree, tree.root(), &options(kind, 1_600.0, 1_000.0, ratio, min_px)).expect("a layout");
        let stats = tiles.stats();

        assert!(!stats.truncated, "{kind:?} hit the backstop instead of the cutoff");
        assert!(
            tiles.len() < 20_000,
            "{kind:?} drew {} tiles from {} nodes",
            tiles.len(),
            tree.len()
        );
        // The point of the cutoff: work is proportional to the viewport, not to
        // the tree. Far more entries are considered than are ever drawn.
        assert!(
            stats.considered > u64::try_from(tiles.len()).expect("tile count fits") * 2,
            "{kind:?} considered {} to draw {}",
            stats.considered,
            tiles.len()
        );

        for tile in tiles.iter().skip(1) {
            match kind {
                LayoutKind::Treemap => {
                    assert!(tile.w * ratio >= min_px - 1e-3, "{kind:?} drew a {}px tile", tile.w);
                    assert!(tile.h * ratio >= min_px - 1e-3, "{kind:?} drew a {}px tile", tile.h);
                }
                LayoutKind::Icicle => {
                    assert!(tile.w * ratio >= min_px - 1e-3, "{kind:?} drew a {}px bar", tile.w);
                }
                _ => {
                    let arc = tile.w * tile.y;
                    assert!(arc * ratio >= min_px - 1e-2, "{kind:?} drew a {arc}px arc");
                }
            }
        }
    }
}

// --------------------------------------------------- largest-subtree budget --

/// `root` with one heavy subtree and one full of dust, both needing subdivision.
///
/// ```text
/// root
///   heavy/   3 files of 4 MB      (12 MB, ~92% of the tree)
///   dust/    `dust` files of 1 KB
/// ```
fn lopsided_tree(dust: usize) -> Tree {
    let mut builder = TreeBuilder::new();
    let root = push_dir(&mut builder, None, b"root").expect("root");

    let heavy = push_dir(&mut builder, Some(root), b"heavy").expect("heavy");
    for index in 0..3_u32 {
        push_file(&mut builder, heavy, format!("heavy{index}.bin").as_bytes(), 4 << 20).expect("heavy file");
    }

    let dusty = push_dir(&mut builder, Some(root), b"dust").expect("dust");
    for index in 0..dust {
        push_file(&mut builder, dusty, format!("d{index:05}.bin").as_bytes(), 1 << 10).expect("dust file");
    }

    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

/// Regression: the tile budget belongs to the largest subtrees.
///
/// Children are laid out sorted descending, then pushed onto a LIFO stack. Push
/// them forwards and the stack pops the SMALLEST first, so `max_tiles` was spent
/// resolving the least significant corner of the tree while the blocks big enough
/// to see were left flat. On a real 1M-node scan of /Applications that rendered
/// as a field of pinhead tiles with no large blocks at all.
#[test]
fn the_tile_budget_is_spent_on_the_largest_subtree_first() {
    let tree = lopsided_tree(400);
    // Far below `dust`'s 400 children, so the budget must be rationed and the
    // walk's order decides who gets it.
    let options = options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 0.5).with_max_tiles(24);
    let tiles = layout_tiles(&tree, tree.root(), &options).expect("a layout");

    let names: Vec<&[u8]> = tiles.iter().filter_map(|tile| tree.name_bytes(tile.node)).collect();
    let heavy_files = names.iter().filter(|name| name.starts_with(b"heavy")).count();

    assert!(
        heavy_files > 0,
        "the heaviest subtree got no tiles at all; the budget went to the dust ({} tiles: {:?})",
        tiles.len(),
        names
            .iter()
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect::<Vec<_>>()
    );
}

/// The user-visible half of the same property: a parent's children are emitted
/// biggest first, so the buffer reads large-to-small rather than arbitrarily.
#[test]
fn treemap_siblings_are_emitted_largest_first() {
    let tree = lopsided_tree(8);
    let tiles = layout_tiles(
        &tree,
        tree.root(),
        &options(LayoutKind::Treemap, 800.0, 600.0, 2.0, 0.5),
    )
    .expect("a layout");

    // The two depth-1 tiles are `heavy` and `dust`; `heavy` is ~92% of the tree
    // and must come first.
    let depth_one: Vec<&[u8]> = tiles
        .iter()
        .filter(|tile| tile.depth == 1)
        .filter_map(|tile| tree.name_bytes(tile.node))
        .collect();

    assert_eq!(
        depth_one.first().map(|name| String::from_utf8_lossy(name).into_owned()),
        Some("heavy".to_owned()),
        "the heaviest child must be emitted first, got {:?}",
        depth_one
            .iter()
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect::<Vec<_>>()
    );
}
