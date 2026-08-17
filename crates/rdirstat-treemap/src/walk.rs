//! The single traversal.
//!
//! One request, one pass over the frozen tree. There is no measure phase: the
//! subtree totals the layout needs are already in
//! [`DirTotals`](rdirstat_core::DirTotals), computed once at scan time, so the
//! walk reads them and never re-sums a subtree.
//!
//! All three layout kinds ride this loop. They differ in exactly two places —
//! how a frame's region is partitioned among its children, and how a region
//! becomes `(x, y, w, h)` at emit time. Sunburst does not get its own
//! recursion: it is the icicle's `(depth, offset, extent)` triple read in polar
//! coordinates.

use crate::error::LayoutError;
use crate::geom::{Rect, Slot, slice, sort_drawable_prefix, squarify};
use crate::options::{
    ICICLE_MAX_ROW_PX, ICICLE_ROW_PX, LayoutOptions, SUNBURST_MAX_RING_PX, SUNBURST_RING_PX, SizeMetric,
};
use crate::tiles::{Tile, TileBuffer};
use rdirstat_core::{CategoryId, LayoutKind, MAX_TREE_DEPTH, NodeId, Tree};

/// One entry on the explicit traversal stack.
///
/// Explicit, never recursive: a 4096-deep `node_modules` chain is a real input
/// and a stack overflow is not a diagnosable failure.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// The id written into the `node` column.
    emit: NodeId,
    /// The node whose children this frame subdivides. Differs from `emit` only
    /// for a virtual `<Files>` group, whose children live on its owner.
    source: NodeId,
    /// The category written into the `category` column.
    category: u8,
    /// Levels below the layout root.
    depth: u32,
    /// Treemap: the rectangle. Icicle/sunburst: `x` is the offset and `w` the
    /// extent, in CSS pixels and radians respectively.
    region: Rect,
    /// Whether directory children are skipped, which is how a virtual `<Files>`
    /// group lays out exactly the bytes it claims.
    files_only: bool,
}

/// Kind-specific geometry, resolved once per request.
#[derive(Clone, Copy, Debug)]
struct Plan {
    kind: LayoutKind,
    /// The cutoff, in CSS pixels.
    min_side: f64,
    /// Squarified area cutoff, `min_side²`.
    min_area: f64,
    /// Exclusive depth ceiling.
    depth_cap: u32,
    /// Row height / ring thickness used *during* the walk.
    base_step: f64,
    /// Ceiling on the row height / ring thickness after the fill pass.
    max_step: f64,
    /// Height (icicle) or radius (sunburst) available to the depth axis.
    available: f64,
}

/// Half a turn, for placing the sunburst's zero angle at twelve o'clock.
const QUARTER_TURN: f64 = core::f64::consts::FRAC_PI_2;
/// A full turn in radians.
const FULL_TURN: f64 = core::f64::consts::TAU;

/// Lays out the subtree at `root` and returns the drawn tiles.
///
/// Rows are emitted in **paint order**: a node always precedes every one of its
/// descendants, so a Canvas 2D renderer can draw the buffer front to back and
/// get correct nesting for free.
///
/// # Errors
///
/// [`LayoutError::UnknownNode`] if `root` names neither a live node nor a
/// virtual group whose owning directory exists.
pub fn layout_tiles(tree: &Tree, root: NodeId, options: &LayoutOptions) -> Result<TileBuffer, LayoutError> {
    let plan = Plan::resolve(options);
    let root_frame = root_frame(tree, root, &plan, options.canvas.width(), options.canvas.height())?;

    let mut tiles = TileBuffer::with_capacity(options.max_tiles.min(4_096));
    let mut stack: Vec<Frame> = Vec::with_capacity(64);
    let mut scratch: Vec<Slot> = Vec::new();
    stack.push(root_frame);

    let mut visited = 0_u32;
    let mut considered = 0_u64;
    let mut truncated = false;

    while let Some(frame) = stack.pop() {
        if tiles.len() >= options.max_tiles {
            truncated = true;
            break;
        }
        visited = visited.saturating_add(1);
        tiles.push(emit(&frame, &plan));

        if frame.depth.saturating_add(1) >= plan.depth_cap {
            continue;
        }

        let total = gather(tree, &frame, options.metric, &mut scratch, &mut considered);
        if total <= 0.0 || scratch.is_empty() {
            continue;
        }

        let child_depth = frame.depth.saturating_add(1);
        let cap = plan.child_cap(frame.region, child_depth, scratch.len());
        if cap == 0 {
            continue;
        }
        sort_drawable_prefix(&mut scratch, cap);

        let placed = match plan.kind {
            LayoutKind::Icicle | LayoutKind::Sunburst => slice(
                &mut scratch,
                frame.region.x,
                frame.region.w,
                total,
                plan.min_extent(child_depth),
            ),
            // `Treemap`, plus the `#[non_exhaustive]` tail: an unrecognised kind
            // draws as a treemap rather than as an empty canvas.
            _ => squarify(&mut scratch, frame.region, total, plan.min_area),
        };

        // Pushed smallest-first so the LIFO stack POPS largest-first.
        //
        // `scratch` is sorted descending, so iterating it forwards and pushing
        // would put the smallest child on top of the stack and expand it first.
        // That matters because `max_tiles` truncates whatever has not been
        // reached yet: spending the budget smallest-first meant a 1M-node
        // subtree drew thousands of pinhead tiles from its least significant
        // corner and then ran out before subdividing the blocks the user can
        // actually see. The budget belongs to the largest subtrees, which are
        // the ones occupying enough pixels to be worth resolving.
        //
        // Paint order is unaffected: a parent is emitted when it is popped,
        // before any of its children are pushed, so a node still always precedes
        // its descendants whatever order its siblings are in.
        for slot in scratch.iter().take(placed).rev() {
            if !plan.is_drawable(slot.rect) {
                continue;
            }
            stack.push(Frame {
                emit: NodeId::from_raw(slot.node),
                source: NodeId::from_raw(slot.node),
                category: slot.category,
                depth: child_depth,
                region: slot.rect,
                files_only: false,
            });
        }
    }

    fit_depth_axis(&mut tiles, &plan);
    let stats = tiles.stats_mut();
    stats.visited = visited;
    stats.considered = considered;
    stats.truncated = truncated;

    // The performance log docs/05-UI.md asks for. Candidate count alone is not
    // evidence that the cutoff bounded the drawn set, so both numbers are here.
    tracing::debug!(
        kind = ?options.kind,
        tiles = tiles.len(),
        considered,
        max_depth = tiles.stats().max_depth,
        truncated,
        min_px = options.min_px.get(),
        width = options.canvas.width(),
        height = options.canvas.height(),
        device_pixel_ratio = options.canvas.device_pixel_ratio(),
        "layout"
    );
    Ok(tiles)
}

impl Plan {
    fn resolve(options: &LayoutOptions) -> Self {
        let min_side = options.canvas.min_side(options.min_px);
        let (base_step, max_step, available) = match options.kind {
            LayoutKind::Icicle => (
                f64::from(ICICLE_ROW_PX).max(min_side),
                f64::from(ICICLE_MAX_ROW_PX),
                options.canvas.height(),
            ),
            LayoutKind::Sunburst => (
                f64::from(SUNBURST_RING_PX).max(min_side),
                f64::from(SUNBURST_MAX_RING_PX),
                options.canvas.radius(),
            ),
            _ => (0.0, 0.0, 0.0),
        };
        let depth_cap = match options.kind {
            LayoutKind::Icicle | LayoutKind::Sunburst => steps_available(available, base_step),
            _ => MAX_TREE_DEPTH,
        };
        Self {
            kind: options.kind,
            min_side,
            min_area: min_side * min_side,
            depth_cap,
            base_step,
            max_step: max_step.max(base_step),
            available,
        }
    }

    /// The smallest extent a child at `depth` may have and still be drawn.
    ///
    /// Pixels for the icicle; radians for the sunburst, where the cutoff is an
    /// **arc length** at the ring's inner radius. That is the honest polar
    /// reading of "three device pixels": an angular sliver near the centre is
    /// invisible even when the same sliver would be wide in the icicle.
    fn min_extent(self, depth: u32) -> f64 {
        match self.kind {
            LayoutKind::Sunburst => {
                let radius = f64::from(depth) * self.base_step;
                if radius <= 0.0 {
                    FULL_TURN
                } else {
                    self.min_side / radius
                }
            }
            _ => self.min_side,
        }
    }

    /// An upper bound on how many children of a region can possibly be drawn.
    ///
    /// This is what keeps a 500k-entry directory from costing a 500k-element
    /// sort: everything past the bound is discarded by a linear selection.
    fn child_cap(self, region: Rect, depth: u32, len: usize) -> usize {
        let raw = match self.kind {
            LayoutKind::Treemap => {
                if self.min_area <= 0.0 {
                    return len;
                }
                region.area() / self.min_area
            }
            LayoutKind::Icicle | LayoutKind::Sunburst => {
                let min_extent = self.min_extent(depth);
                if min_extent <= 0.0 {
                    return len;
                }
                region.w / min_extent
            }
            _ => return len,
        };
        bound(raw, len)
    }

    /// Whether a placed rectangle clears the cutoff on both axes.
    ///
    /// [`slice`] already enforces the extent, so this only bites on the
    /// squarified path, where a row can be wide and paper-thin.
    fn is_drawable(self, rect: Rect) -> bool {
        match self.kind {
            LayoutKind::Treemap => rect.w >= self.min_side && rect.h >= self.min_side,
            _ => rect.w > 0.0,
        }
    }
}

/// Builds the frame for the layout root.
fn root_frame(tree: &Tree, root: NodeId, plan: &Plan, width: f64, height: f64) -> Result<Frame, LayoutError> {
    let region = match plan.kind {
        LayoutKind::Icicle => Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: 0.0,
        },
        LayoutKind::Sunburst => Rect {
            x: -QUARTER_TURN,
            y: 0.0,
            w: FULL_TURN,
            h: 0.0,
        },
        _ => Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        },
    };

    if let Some(owner) = root.group_owner() {
        // A virtual `<Files>` group has no arena node. It lays out exactly the
        // direct files of its owner, which is precisely the byte total the tree
        // table shows against that row.
        if tree.dir_totals(owner).is_none() {
            return Err(LayoutError::UnknownNode { node: root });
        }
        return Ok(Frame {
            emit: root,
            source: owner,
            category: CategoryId::UNCATEGORIZED.get(),
            depth: 0,
            region,
            files_only: true,
        });
    }

    let node = tree.node(root).ok_or(LayoutError::UnknownNode { node: root })?;
    Ok(Frame {
        emit: root,
        source: root,
        category: node.category().get(),
        depth: 0,
        region,
        files_only: false,
    })
}

/// Collects the drawable children of `frame` into `scratch` and returns their
/// total weight.
///
/// `scratch` is owned by the caller and reused across every frame, so the walk
/// allocates once and then stays in the same buffer — no allocation in the hot
/// loop.
fn gather(tree: &Tree, frame: &Frame, metric: SizeMetric, scratch: &mut Vec<Slot>, considered: &mut u64) -> f64 {
    scratch.clear();
    let mut total = 0.0_f64;
    for child in tree.children(frame.source) {
        *considered = considered.saturating_add(1);
        let Some(node) = tree.node(child) else {
            continue;
        };
        let directory = node.kind().is_directory();
        if frame.files_only && directory {
            continue;
        }
        // `Tree::{allocated_of, logical_of}` binary-search `DirIndex`, which is
        // the right answer for a directory and pure overhead for a leaf. Files
        // dominate the child links at the design profile, so the leaf branch
        // reads the node it already has in hand. Both branches route through
        // core's `contributed_*`, so the hard-link policy still lives in exactly
        // one place.
        let bytes = match (directory, metric) {
            (true, SizeMetric::Allocated) => tree.allocated_of(child),
            (true, _) => tree.logical_of(child),
            (false, SizeMetric::Allocated) => node.contributed_alloc(),
            (false, _) => node.contributed_size(),
        };
        if bytes == 0 {
            // Zero bytes is zero area. A repeated hard link lands here, because
            // `Node::contributed_*` already zeroed it — that policy is not
            // re-implemented anywhere in this crate.
            continue;
        }
        let weight = to_weight(bytes);
        total += weight;
        scratch.push(Slot::new(child.raw(), weight, node.category().get()));
    }
    total
}

/// Turns a frame into the row the renderer draws.
fn emit(frame: &Frame, plan: &Plan) -> Tile {
    // `y` and `h` are written by `fit_depth_axis` for the sliced kinds; the
    // treemap already has all four.
    let (x, y, w, h) = match plan.kind {
        LayoutKind::Icicle | LayoutKind::Sunburst => (frame.region.x, 0.0, frame.region.w, 0.0),
        _ => (frame.region.x, frame.region.y, frame.region.w, frame.region.h),
    };
    Tile {
        node: frame.emit,
        depth: frame.depth,
        x: narrow(x),
        y: narrow(y),
        w: narrow(w),
        h: narrow(h),
        category: CategoryId::from_raw(frame.category),
    }
}

/// Expands the depth axis so the drawn levels fill the viewport.
///
/// A pass over the emitted rows, not a second traversal of the tree. It can only
/// make rows taller (and sunburst rings fatter, hence arcs longer), so every
/// tile that cleared the cutoff during the walk still clears it afterwards.
fn fit_depth_axis(tiles: &mut TileBuffer, plan: &Plan) {
    match plan.kind {
        LayoutKind::Icicle | LayoutKind::Sunburst => {}
        _ => return,
    }
    if tiles.is_empty() {
        return;
    }
    let levels = f64::from(tiles.stats().max_depth.saturating_add(1));
    let step = (plan.available / levels).min(plan.max_step).max(plan.base_step);
    tiles.set_depth_axis(narrow(step));
}

/// How many equal steps of `step` fit into `available`, at least one and never
/// past [`MAX_TREE_DEPTH`].
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the quotient is clamped into [1, MAX_TREE_DEPTH] before the cast, so it is exact"
)]
fn steps_available(available: f64, step: f64) -> u32 {
    if step <= 0.0 || !available.is_finite() {
        return 1;
    }
    let quotient = (available / step).floor();
    if !quotient.is_finite() || quotient < 1.0 {
        return 1;
    }
    if quotient >= f64::from(MAX_TREE_DEPTH) {
        return MAX_TREE_DEPTH;
    }
    quotient as u32
}

/// Clamps a real-valued bound into `[0, len]`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "value is compared against len as f64 before the cast, so the result is within [0, len]"
)]
fn bound(value: f64, len: usize) -> usize {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let ceiling = len as f64;
    if value >= ceiling {
        return len;
    }
    (value as usize).saturating_add(1).min(len)
}

/// Byte counts are exact up to 2^53, which is 9 PB — comfortably past any
/// volume this tool scans, and the value is an *area weight*, not a displayed
/// number.
#[allow(
    clippy::cast_precision_loss,
    reason = "an area weight, never a displayed byte count; exact below 2^53"
)]
fn to_weight(bytes: u64) -> f64 {
    bytes as f64
}

/// Coordinates cross the wire as `f32`; the traversal keeps `f64` so offsets do
/// not drift as they accumulate across a level.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the Arrow layout schema pins x/y/w/h to Float32; the walk itself is f64"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}
