//! Hierarchy layout — squarified treemap, icicle, sunburst — as one Arrow IPC
//! batch.
//!
//! # Integration seam
//!
//! **This module is the temporary home of `rdirstat-treemap`.** The command
//! layer calls exactly one function, [`build`]. When `crates/rdirstat-treemap`
//! lands, replace the body of [`build`] with a call into it and delete the
//! rest; `commands.rs` does not change.
//!
//! All three layouts share the schema pinned in
//! [`LAYOUT_COLUMNS`](rdirstat_core::LAYOUT_COLUMNS): `x/y/w/h` are rectangle
//! coordinates for the treemap and icicle, and
//! `(start_angle, inner_radius, sweep, thickness)` for the sunburst.
//!
//! The drawn-tile count is bounded by the sub-pixel cutoff, not by the node
//! count: a tile smaller than `min_px` device pixels is neither emitted nor
//! descended into. That is what makes `layout` `O(drawn tiles)` on a 69M-node
//! tree.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, RecordBatch, UInt8Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use rdirstat_core::{
    ARROW_META_GENERATION, ARROW_META_PROTOCOL_VERSION, ARROW_META_SCHEMA_NAME, ARROW_META_SCHEMA_VERSION,
    BinaryResponse, LAYOUT_SCHEMA_NAME, LAYOUT_SCHEMA_VERSION, LayoutKind, MIN_TILE_PX, NodeId, PROTOCOL_VERSION,
    QueryError, Tree, TreeGeneration, Viewport,
};

/// Hard ceiling on emitted tiles.
///
/// A canvas cannot paint more than this within the frame budget, and the
/// response must stay bounded regardless of what `min_px` the caller passes.
pub(crate) const MAX_TILES: usize = 20_000;

/// Icicle row height in CSS pixels.
const ICICLE_ROW_PX: f32 = 18.0;

/// Deepest ring a sunburst draws. Beyond this, angular resolution is gone.
const SUNBURST_MAX_RINGS: u32 = 10;

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// The columnar tile buffer, in `LAYOUT_COLUMNS` order.
#[derive(Debug, Default)]
struct Tiles {
    node: Vec<u32>,
    depth: Vec<u32>,
    x: Vec<f32>,
    y: Vec<f32>,
    w: Vec<f32>,
    h: Vec<f32>,
    category: Vec<u8>,
}

impl Tiles {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            node: Vec::with_capacity(capacity),
            depth: Vec::with_capacity(capacity),
            x: Vec::with_capacity(capacity),
            y: Vec::with_capacity(capacity),
            w: Vec::with_capacity(capacity),
            h: Vec::with_capacity(capacity),
            category: Vec::with_capacity(capacity),
        }
    }

    fn len(&self) -> usize {
        self.node.len()
    }

    fn push(&mut self, tree: &Tree, node: NodeId, depth: u32, rect: Rect) {
        self.node.push(node.raw());
        self.depth.push(depth);
        self.x.push(rect.x);
        self.y.push(rect.y);
        self.w.push(rect.w);
        self.h.push(rect.h);
        self.category
            .push(tree.node(node).map_or(0, |entry| entry.category().get()));
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "geometry is float by definition; a byte count above 2^24 loses precision the canvas could not draw anyway"
)]
const fn as_f32(value: u64) -> f32 {
    value as f32
}

/// The children of `node` that are worth laying out, largest first.
///
/// Zero-byte entries are dropped: they have no area, so they would be pruned by
/// the sub-pixel cutoff on the very next line.
fn weighted_children(tree: &Tree, node: NodeId) -> Vec<(NodeId, u64)> {
    let mut children: Vec<(NodeId, u64)> = tree
        .children(node)
        .map(|child| (child, tree.logical_of(child)))
        .filter(|(_, weight)| *weight > 0)
        .collect();
    // Descending by weight, then by id so the order is total and a redraw of
    // the same tree produces the same picture.
    children.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.raw().cmp(&b.0.raw())));
    children
}

/// The aspect-ratio cost of a candidate row, per Bruls et al.
fn worst_ratio(areas: &[f32], side: f32) -> f32 {
    let sum: f32 = areas.iter().sum();
    if sum <= 0.0 || side <= 0.0 {
        return f32::INFINITY;
    }
    let mut smallest = f32::INFINITY;
    let mut largest = 0.0_f32;
    for area in areas {
        smallest = smallest.min(*area);
        largest = largest.max(*area);
    }
    if smallest <= 0.0 {
        return f32::INFINITY;
    }
    let square = sum * sum;
    ((side * side * largest) / square).max(square / (side * side * smallest))
}

/// Squarifies `areas` (already sorted descending, in square pixels) into `rect`.
///
/// Returns one rectangle per input, in the same order.
fn squarify(areas: &[f32], rect: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(areas.len());
    let mut free = rect;
    let mut start = 0_usize;

    while start < areas.len() {
        let side = free.w.min(free.h);
        if side <= 0.0 {
            // No room left; the remainder collapses and will be pruned.
            out.extend(std::iter::repeat_n(
                Rect {
                    x: free.x,
                    y: free.y,
                    w: 0.0,
                    h: 0.0,
                },
                areas.len() - out.len(),
            ));
            return out;
        }

        let mut end = start + 1;
        let mut best = worst_ratio(&areas[start..end], side);
        while end < areas.len() {
            let candidate = worst_ratio(&areas[start..=end], side);
            if candidate > best {
                break;
            }
            best = candidate;
            end += 1;
        }

        let row = &areas[start..end];
        let row_area: f32 = row.iter().sum();
        if free.w >= free.h {
            let thickness = if free.h > 0.0 { row_area / free.h } else { 0.0 };
            let mut cursor = free.y;
            for area in row {
                let height = if thickness > 0.0 { area / thickness } else { 0.0 };
                out.push(Rect {
                    x: free.x,
                    y: cursor,
                    w: thickness,
                    h: height,
                });
                cursor += height;
            }
            free.x += thickness;
            free.w = (free.w - thickness).max(0.0);
        } else {
            let thickness = if free.w > 0.0 { row_area / free.w } else { 0.0 };
            let mut cursor = free.x;
            for area in row {
                let width = if thickness > 0.0 { area / thickness } else { 0.0 };
                out.push(Rect {
                    x: cursor,
                    y: free.y,
                    w: width,
                    h: thickness,
                });
                cursor += width;
            }
            free.y += thickness;
            free.h = (free.h - thickness).max(0.0);
        }
        start = end;
    }
    out
}

fn treemap(tree: &Tree, root: NodeId, canvas: Rect, cutoff_css_px: f32, tiles: &mut Tiles) {
    let mut stack: Vec<(NodeId, Rect, u32)> = vec![(root, canvas, 0)];
    tiles.push(tree, root, 0, canvas);
    while let Some((node, rect, depth)) = stack.pop() {
        if tiles.len() >= MAX_TILES || depth >= rdirstat_core::MAX_TREE_DEPTH {
            continue;
        }
        let children = weighted_children(tree, node);
        if children.is_empty() {
            continue;
        }
        let total: u64 = children.iter().map(|(_, weight)| *weight).sum();
        if total == 0 {
            continue;
        }
        let scale = (rect.w * rect.h) / as_f32(total);
        let areas: Vec<f32> = children.iter().map(|(_, weight)| as_f32(*weight) * scale).collect();
        for ((child, _), child_rect) in children.iter().zip(squarify(&areas, rect)) {
            if child_rect.w.min(child_rect.h) < cutoff_css_px {
                continue;
            }
            if tiles.len() >= MAX_TILES {
                return;
            }
            tiles.push(tree, *child, depth + 1, child_rect);
            if tree.child_count(*child) > 0 {
                stack.push((*child, child_rect, depth + 1));
            }
        }
    }
}

fn icicle(tree: &Tree, root: NodeId, canvas: Rect, cutoff_css_px: f32, tiles: &mut Tiles) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a row count derived from a clamped viewport height cannot be negative or exceed u32"
    )]
    let max_depth = ((canvas.h / ICICLE_ROW_PX).floor().max(1.0) as u32).min(rdirstat_core::MAX_TREE_DEPTH);
    let mut stack: Vec<(NodeId, Rect, u32)> = vec![(
        root,
        Rect {
            x: canvas.x,
            y: canvas.y,
            w: canvas.w,
            h: ICICLE_ROW_PX,
        },
        0,
    )];
    tiles.push(
        tree,
        root,
        0,
        Rect {
            x: canvas.x,
            y: canvas.y,
            w: canvas.w,
            h: ICICLE_ROW_PX,
        },
    );
    while let Some((node, rect, depth)) = stack.pop() {
        if depth + 1 >= max_depth || tiles.len() >= MAX_TILES {
            continue;
        }
        let children = weighted_children(tree, node);
        let total: u64 = children.iter().map(|(_, weight)| *weight).sum();
        if total == 0 {
            continue;
        }
        let mut cursor = rect.x;
        for (child, weight) in children {
            let width = rect.w * (as_f32(weight) / as_f32(total));
            let child_rect = Rect {
                x: cursor,
                y: canvas.y + ICICLE_ROW_PX * as_f32(u64::from(depth) + 1),
                w: width,
                h: ICICLE_ROW_PX,
            };
            cursor += width;
            if width < cutoff_css_px {
                continue;
            }
            if tiles.len() >= MAX_TILES {
                return;
            }
            tiles.push(tree, child, depth + 1, child_rect);
            if tree.child_count(child) > 0 {
                stack.push((child, child_rect, depth + 1));
            }
        }
    }
}

fn sunburst(tree: &Tree, root: NodeId, canvas: Rect, cutoff_css_px: f32, tiles: &mut Tiles) {
    use std::f32::consts::TAU;

    let radius = canvas.w.min(canvas.h) / 2.0;
    let ring = radius / as_f32(u64::from(SUNBURST_MAX_RINGS));
    // The root is the centre disc: inner radius 0, a full turn of sweep.
    let root_rect = Rect {
        x: 0.0,
        y: 0.0,
        w: TAU,
        h: ring,
    };
    tiles.push(tree, root, 0, root_rect);
    let mut stack: Vec<(NodeId, Rect, u32)> = vec![(root, root_rect, 0)];

    while let Some((node, arc, depth)) = stack.pop() {
        if depth + 1 >= SUNBURST_MAX_RINGS || tiles.len() >= MAX_TILES {
            continue;
        }
        let children = weighted_children(tree, node);
        let total: u64 = children.iter().map(|(_, weight)| *weight).sum();
        if total == 0 {
            continue;
        }
        let inner = ring * as_f32(u64::from(depth) + 1);
        let mut cursor = arc.x;
        for (child, weight) in children {
            let sweep = arc.w * (as_f32(weight) / as_f32(total));
            let child_arc = Rect {
                x: cursor,
                y: inner,
                w: sweep,
                h: ring,
            };
            cursor += sweep;
            // Arc length at the inner edge is the honest measure of whether a
            // wedge is drawable.
            if inner * sweep < cutoff_css_px {
                continue;
            }
            if tiles.len() >= MAX_TILES {
                return;
            }
            tiles.push(tree, child, depth + 1, child_arc);
            if tree.child_count(child) > 0 {
                stack.push((child, child_arc, depth + 1));
            }
        }
    }
}

fn schema(generation: TreeGeneration) -> Schema {
    let fields = vec![
        Field::new("node", DataType::UInt32, false),
        Field::new("depth", DataType::UInt32, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("w", DataType::Float32, false),
        Field::new("h", DataType::Float32, false),
        Field::new("category", DataType::UInt8, false),
    ];
    let metadata = [
        (ARROW_META_PROTOCOL_VERSION.to_owned(), PROTOCOL_VERSION.to_string()),
        (ARROW_META_GENERATION.to_owned(), generation.get().to_string()),
        (ARROW_META_SCHEMA_NAME.to_owned(), LAYOUT_SCHEMA_NAME.to_owned()),
        (ARROW_META_SCHEMA_VERSION.to_owned(), LAYOUT_SCHEMA_VERSION.to_string()),
    ]
    .into_iter()
    .collect();
    Schema::new_with_metadata(fields, metadata)
}

/// Computes a layout and serializes it as an Arrow IPC stream.
///
/// `min_px` is a **device**-pixel cutoff; it is converted to CSS pixels with
/// `viewport.device_pixel_ratio` before any comparison, so a Retina display
/// draws more tiles rather than the same tiles at half the resolution.
///
/// # Errors
///
/// [`QueryError::UnknownNode`] if `root` is not in the tree,
/// [`QueryError::VirtualGroup`] if it is a `<Files>` group (a group has no
/// subtree to lay out), or [`QueryError::Internal`] if Arrow serialization
/// fails.
pub(crate) fn build(
    tree: &Tree,
    generation: TreeGeneration,
    root: NodeId,
    kind: LayoutKind,
    viewport: Viewport,
    min_px: f32,
) -> Result<BinaryResponse, QueryError> {
    if root.is_virtual_group() {
        return Err(QueryError::VirtualGroup { node: root });
    }
    if !tree.contains(root) {
        return Err(QueryError::UnknownNode { node: root });
    }

    let dpr = if viewport.device_pixel_ratio.is_finite() && viewport.device_pixel_ratio > 0.0 {
        viewport.device_pixel_ratio
    } else {
        1.0
    };
    let width = viewport.width.max(0.0);
    let height = viewport.height.max(0.0);
    let cutoff_device_px = if min_px.is_finite() && min_px > 0.0 {
        min_px
    } else {
        MIN_TILE_PX
    };
    let cutoff_css_px = cutoff_device_px / dpr;
    let canvas = Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height,
    };

    let mut tiles = Tiles::with_capacity(1_024);
    if width > 0.0 && height > 0.0 {
        match kind {
            LayoutKind::Icicle => icicle(tree, root, canvas, cutoff_css_px, &mut tiles),
            LayoutKind::Sunburst => sunburst(tree, root, canvas, cutoff_css_px, &mut tiles),
            // `Treemap` and any future variant: squarified rectangles are the
            // documented default.
            _ => treemap(tree, root, canvas, cutoff_css_px, &mut tiles),
        }
    }

    let schema = Arc::new(schema(generation));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(tiles.node)),
        Arc::new(UInt32Array::from(tiles.depth)),
        Arc::new(Float32Array::from(tiles.x)),
        Arc::new(Float32Array::from(tiles.y)),
        Arc::new(Float32Array::from(tiles.w)),
        Arc::new(Float32Array::from(tiles.h)),
        Arc::new(UInt8Array::from(tiles.category)),
    ];
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns)
        .map_err(|error| QueryError::Internal(format!("layout batch: {error}")))?;

    let mut bytes = Vec::with_capacity(4_096);
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &schema)
            .map_err(|error| QueryError::Internal(format!("arrow writer: {error}")))?;
        writer
            .write(&batch)
            .map_err(|error| QueryError::Internal(format!("arrow write: {error}")))?;
        writer
            .finish()
            .map_err(|error| QueryError::Internal(format!("arrow finish: {error}")))?;
    }
    Ok(BinaryResponse::new(generation, LAYOUT_SCHEMA_VERSION, bytes))
}

#[cfg(test)]
mod tests {
    use arrow::ipc::reader::StreamReader;
    use rdirstat_core::{DirTotals, LAYOUT_COLUMNS, Node, TreeBuilder};

    use super::*;

    fn viewport() -> Viewport {
        Viewport {
            width: 1_000.0,
            height: 600.0,
            device_pixel_ratio: 2.0,
        }
    }

    /// A root with three directories, each holding one file.
    fn tree() -> Tree {
        let mut builder = TreeBuilder::new();
        let root_name = builder.intern(b"/root").expect("intern");
        let root = builder.push_node(Node::directory(root_name, 0)).expect("root");
        builder.register_directory(root, DirTotals::EMPTY).expect("register");
        for (index, size) in [(0_u32, 600_u64), (1, 300), (2, 100)] {
            let dir_name = builder.intern(format!("dir{index}").as_bytes()).expect("intern");
            let dir = builder.push_child(root, Node::directory(dir_name, 0)).expect("dir");
            builder.register_directory(dir, DirTotals::EMPTY).expect("register");
            let file_name = builder.intern(format!("file{index}.bin").as_bytes()).expect("intern");
            let file = Node::leaf(file_name, rdirstat_core::Kind::File, size * 1_000, size * 1_000, 0);
            builder.push_child(dir, file).expect("file");
            if let Some(totals) = builder.dir_totals_mut(dir) {
                totals.observed_entries += 1;
                totals.retained_nodes += 1;
                totals.absorb_direct_file(size * 1_000, size * 1_000, 0);
            }
        }
        builder.rollup().expect("rollup");
        builder.finish().expect("finish")
    }

    fn read_back(response: &BinaryResponse) -> (Schema, RecordBatch) {
        let reader = StreamReader::try_new(std::io::Cursor::new(response.bytes.clone()), None).expect("reader");
        let schema = reader.schema();
        let batch = reader.into_iter().next().expect("one batch").expect("valid batch");
        ((*schema).clone(), batch)
    }

    #[test]
    fn the_schema_is_exactly_the_pinned_contract() {
        let tree = tree();
        let response = build(
            &tree,
            TreeGeneration::from_raw(5),
            tree.root(),
            LayoutKind::Treemap,
            viewport(),
            MIN_TILE_PX,
        )
        .expect("layout");
        let (schema, _) = read_back(&response);

        let names: Vec<&str> = schema.fields().iter().map(|field| field.name().as_str()).collect();
        assert_eq!(names, LAYOUT_COLUMNS.to_vec());
        assert_eq!(schema.field(0).data_type(), &DataType::UInt32);
        assert_eq!(schema.field(1).data_type(), &DataType::UInt32);
        assert_eq!(schema.field(2).data_type(), &DataType::Float32);
        assert_eq!(schema.field(6).data_type(), &DataType::UInt8);
        assert!(schema.fields().iter().all(|field| !field.is_nullable()));
    }

    #[test]
    fn generation_and_protocol_travel_as_schema_metadata() {
        let tree = tree();
        let response = build(
            &tree,
            TreeGeneration::from_raw(5),
            tree.root(),
            LayoutKind::Treemap,
            viewport(),
            MIN_TILE_PX,
        )
        .expect("layout");
        let (schema, _) = read_back(&response);
        assert_eq!(
            schema.metadata().get(ARROW_META_GENERATION).map(String::as_str),
            Some("5")
        );
        assert_eq!(
            schema.metadata().get(ARROW_META_SCHEMA_NAME).map(String::as_str),
            Some(LAYOUT_SCHEMA_NAME)
        );
        assert_eq!(
            schema.metadata().get(ARROW_META_PROTOCOL_VERSION).map(String::as_str),
            Some("1")
        );
        assert_eq!(response.generation, TreeGeneration::from_raw(5));
        assert_eq!(response.schema_version, LAYOUT_SCHEMA_VERSION);
    }

    #[test]
    fn every_layout_kind_produces_rows_for_the_same_tree() {
        let tree = tree();
        for kind in [LayoutKind::Treemap, LayoutKind::Icicle, LayoutKind::Sunburst] {
            let response =
                build(&tree, TreeGeneration::FIRST, tree.root(), kind, viewport(), MIN_TILE_PX).expect("layout");
            let (_, batch) = read_back(&response);
            assert!(batch.num_rows() > 1, "{kind:?} produced only the root tile");
            assert_eq!(batch.num_columns(), 7);
        }
    }

    #[test]
    fn treemap_tiles_stay_inside_the_viewport_and_do_not_overlap_in_area() {
        let tree = tree();
        let response = build(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Treemap,
            viewport(),
            MIN_TILE_PX,
        )
        .expect("layout");
        let (_, batch) = read_back(&response);
        let x = batch.column(2).as_any().downcast_ref::<Float32Array>().expect("x");
        let y = batch.column(3).as_any().downcast_ref::<Float32Array>().expect("y");
        let w = batch.column(4).as_any().downcast_ref::<Float32Array>().expect("w");
        let h = batch.column(5).as_any().downcast_ref::<Float32Array>().expect("h");
        let depth = batch.column(1).as_any().downcast_ref::<UInt32Array>().expect("depth");

        let mut first_level_area = 0.0_f32;
        for index in 0..batch.num_rows() {
            assert!(x.value(index) >= -0.01 && y.value(index) >= -0.01);
            assert!(x.value(index) + w.value(index) <= 1_000.5, "row {index} escapes right");
            assert!(y.value(index) + h.value(index) <= 600.5, "row {index} escapes bottom");
            if depth.value(index) == 1 {
                first_level_area += w.value(index) * h.value(index);
            }
        }
        // The three top-level directories tile the canvas.
        assert!(
            (first_level_area - 1_000.0 * 600.0).abs() < 5_000.0,
            "depth-1 tiles should cover the canvas, got {first_level_area}"
        );
    }

    #[test]
    fn a_bigger_cutoff_draws_fewer_tiles() {
        let tree = tree();
        let counts: Vec<usize> = [3.0_f32, 400.0]
            .into_iter()
            .map(|min_px| {
                let response = build(
                    &tree,
                    TreeGeneration::FIRST,
                    tree.root(),
                    LayoutKind::Treemap,
                    viewport(),
                    min_px,
                )
                .expect("layout");
                read_back(&response).1.num_rows()
            })
            .collect();
        assert!(counts[0] > counts[1], "the sub-pixel cutoff must bound the drawn set");
    }

    #[test]
    fn a_zero_sized_viewport_yields_an_empty_batch_not_an_error() {
        let tree = tree();
        let response = build(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Treemap,
            Viewport {
                width: 0.0,
                height: 0.0,
                device_pixel_ratio: 2.0,
            },
            MIN_TILE_PX,
        )
        .expect("layout");
        let (_, batch) = read_back(&response);
        assert_eq!(batch.num_rows(), 0);
    }

    #[test]
    fn a_virtual_group_has_no_layout() {
        let tree = tree();
        let group = NodeId::virtual_group_of(NodeId::from_raw(1)).expect("a group id");
        assert_eq!(
            build(
                &tree,
                TreeGeneration::FIRST,
                group,
                LayoutKind::Treemap,
                viewport(),
                MIN_TILE_PX
            )
            .expect_err("this call must be rejected"),
            QueryError::VirtualGroup { node: group }
        );
    }

    #[test]
    fn an_unknown_root_is_rejected() {
        let tree = tree();
        let missing = NodeId::from_raw(9_999);
        assert_eq!(
            build(
                &tree,
                TreeGeneration::FIRST,
                missing,
                LayoutKind::Treemap,
                viewport(),
                MIN_TILE_PX
            )
            .expect_err("this call must be rejected"),
            QueryError::UnknownNode { node: missing }
        );
    }

    #[test]
    fn squarify_conserves_area() {
        let areas = vec![600.0_f32, 300.0, 100.0];
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 10.0,
        };
        let rects = squarify(&areas, rect);
        assert_eq!(rects.len(), 3);
        let total: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total - 1_000.0).abs() < 1.0, "area drifted to {total}");
    }

    #[test]
    fn sunburst_sweeps_sum_to_a_full_turn_at_the_first_ring() {
        let tree = tree();
        let response = build(
            &tree,
            TreeGeneration::FIRST,
            tree.root(),
            LayoutKind::Sunburst,
            viewport(),
            0.1,
        )
        .expect("layout");
        let (_, batch) = read_back(&response);
        let depth = batch.column(1).as_any().downcast_ref::<UInt32Array>().expect("depth");
        let sweep = batch.column(4).as_any().downcast_ref::<Float32Array>().expect("w");
        let total: f32 = (0..batch.num_rows())
            .filter(|index| depth.value(*index) == 1)
            .map(|index| sweep.value(index))
            .sum();
        assert!(
            (total - std::f32::consts::TAU).abs() < 0.01,
            "ring 1 must be a full turn, got {total}"
        );
    }
}
