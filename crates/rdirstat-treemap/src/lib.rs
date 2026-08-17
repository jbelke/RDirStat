//! # `rdirstat-treemap` — three hierarchy layouts over one traversal
//!
//! Turns a frozen [`Tree`] into the Arrow `layout` batch the canvas draws:
//! columns `node, depth, x, y, w, h, category`, none nullable, with the tree
//! generation and the schema version in the Arrow schema metadata.
//!
//! ## The sub-pixel cutoff is the whole design
//!
//! `min_px` (default [`rdirstat_core::MIN_TILE_PX`], 3.0 **device** pixels) is
//! not a polish knob. It is the only thing standing between a 69M-entry volume
//! and an unbounded draw call. A tile whose smallest drawn dimension falls below
//! the cutoff is neither emitted nor recursed into, so the drawn set is bounded
//! by the viewport rather than by the tree:
//!
//! | Layout | Bound on drawn tiles |
//! | --- | --- |
//! | Treemap | viewport area in device px / `min_px²` |
//! | Icicle | rows x (viewport width in device px / `min_px`) |
//! | Sunburst | rings x (ring circumference in device px / `min_px`) |
//!
//! docs/05-UI.md is explicit that "candidate node count alone is not evidence
//! that the sub-pixel cutoff kept the rendered set bounded", so every run also
//! reports [`LayoutStats`]: tiles emitted, frames visited, and child links
//! considered.
//!
//! ## One traversal, three readings
//!
//! There is no measure pass. Subtree bytes already live in
//! [`DirTotals`](rdirstat_core::DirTotals) from scan time, so a request is a
//! single iterative, stack-based descent. The kinds diverge in exactly two
//! places — how a region is partitioned among children, and how a region reads
//! as `(x, y, w, h)`:
//!
//! - **Treemap** — squarified rows (Bruls/Huizing/van Wijk). `x, y, w, h` is a
//!   rectangle in **CSS pixels**, origin at the viewport's top left.
//! - **Icicle** — the `(depth, offset, extent)` triple as stacked horizontal
//!   bars. `x` is the left edge and `w` the width in CSS pixels; `y` is
//!   `depth × row height` and `h` is the row height.
//! - **Sunburst** — the *same* triple in polar coordinates. Not a third
//!   algorithm: `x` is the start angle in radians, `w` the swept angle, `y` the
//!   inner radius in CSS pixels, and `h` the ring thickness.
//!
//! ### Sunburst coordinate frame — the renderer must match this
//!
//! - Centre is `(viewport.width / 2, viewport.height / 2)` in CSS pixels.
//! - Radii are CSS pixels from that centre; the outermost ring never exceeds
//!   `min(width, height) / 2`.
//! - Angles are radians in the Canvas 2D convention (`0` at three o'clock,
//!   increasing clockwise on screen). The root arc starts at `-π/2`, so the
//!   first child begins at twelve o'clock.
//! - Draw an arc with
//!   `ctx.arc(cx, cy, y + h, x, x + w)` / `ctx.arc(cx, cy, y, x + w, x, true)`.
//!
//! ## Paint order
//!
//! Rows are emitted in pre-order: **a node always precedes every one of its
//! descendants**. Drawing the buffer front to back therefore gives correct
//! nesting with no sorting in the renderer. Sibling order is by descending
//! bytes with an ascending [`NodeId`] tie-break, which is what makes the output
//! deterministic — arena sibling order is head-insertion order and explicitly
//! not a UI contract.
//!
//! ## Determinism
//!
//! For a fixed `(tree, kind, viewport, min_px, metric)` the emitted bytes are
//! identical, run to run and process to process. There is no hashing, no
//! iteration over a hash map, and no reliance on arena order. That is what makes
//! a resize idempotent and a golden fixture possible.
//!
//! ## What this crate deliberately does not do
//!
//! Flat fills only. Cushion shading is a renderer enhancement deferred by
//! docs/05-UI.md behind a flat renderer that meets the budget, and it must
//! consume this same geometry rather than shipping a full-frame RGBA image over
//! IPC. Colour is never sent: the `category` column is an index, and the
//! frontend resolves it against the CSS vars.
//!
//! ## Usage
//!
//! ```
//! use rdirstat_core::{LayoutKind, NodeId, Tree, TreeBuilder, Viewport};
//! use rdirstat_treemap::layout;
//!
//! let tree: Tree = TreeBuilder::new().finish()?;
//! let viewport = Viewport { width: 800.0, height: 600.0, device_pixel_ratio: 2.0 };
//! // An empty tree has no root node, so this is the honest error path.
//! assert!(layout(&tree, Default::default(), NodeId::ROOT, LayoutKind::Treemap, viewport, 3.0).is_err());
//! # Ok::<(), rdirstat_core::ArenaError>(())
//! ```

#![forbid(unsafe_code)]

mod error;
mod filter;
mod geom;
mod ipc;
mod options;
mod tiles;
mod walk;

pub use crate::error::LayoutError;
pub use crate::filter::{CategorySet, FilteredWeights};
pub use crate::ipc::{LAYOUT_COLUMN_TYPES, layout_schema, tiles_to_batch, tiles_to_ipc, tiles_to_response};
pub use crate::options::{
    Canvas, DEFAULT_MAX_TILES, ICICLE_MAX_ROW_PX, ICICLE_ROW_PX, LayoutOptions, MAX_DEVICE_PIXEL_RATIO, MAX_MIN_PX,
    MAX_VIEWPORT_PX, MinPx, SUNBURST_MAX_RING_PX, SUNBURST_RING_PX, SizeMetric, TREEMAP_DEPTH_CAP,
};
pub use crate::tiles::{LayoutStats, Tile, TileBuffer};
pub use crate::walk::{layout_tiles, layout_tiles_with};

use rdirstat_core::{BinaryResponse, LayoutKind, NodeId, QueryError, Tree, TreeGeneration, Viewport};

/// The `layout` command, end to end.
///
/// Mirrors the frozen signature
/// `layout(generation, root, kind, viewport, min_px) -> Result<BinaryResponse, QueryError>`:
/// `src-tauri` resolves `generation` to a [`Tree`] and calls straight through.
/// Area is driven by **allocated** bytes, matching docs/05-UI.md's default for
/// tables and charts; use [`layout_with`] for logical.
///
/// ## An unmeasured canvas is a state, not an error
///
/// A React canvas reports `0 x 0` on its first render, before the resize
/// observer fires. That is not a bug worth an error toast, so a **finite**
/// zero-or-negative width or height returns a well-formed, zero-row batch: the
/// renderer draws nothing and asks again once it has been measured. A
/// *non-finite* dimension is still a real bug and still fails.
///
/// `min_px` is not softened the same way. Substituting a different cutoff would
/// make the recorded `min_px` disagree with the geometry actually drawn, and
/// docs/05-UI.md wants that measurement to be trustworthy.
///
/// # Errors
///
/// - [`QueryError::UnknownNode`] if `root` is not in this tree.
/// - [`QueryError::Internal`] for a non-finite or out-of-range viewport, a
///   non-positive `min_px`, or an Arrow encoding failure.
pub fn layout(
    tree: &Tree,
    generation: TreeGeneration,
    root: NodeId,
    kind: LayoutKind,
    viewport: Viewport,
    min_px: f32,
) -> Result<BinaryResponse, QueryError> {
    if !tree.contains(root) {
        return Err(QueryError::UnknownNode { node: root });
    }
    let unmeasured =
        viewport.width.is_finite() && viewport.height.is_finite() && (viewport.width <= 0.0 || viewport.height <= 0.0);
    if unmeasured {
        return Ok(tiles_to_response(&TileBuffer::new(), generation)?);
    }
    let options = LayoutOptions::new(kind, viewport, min_px)?;
    Ok(layout_with(tree, generation, root, &options)?)
}

/// [`layout`] with the options spelled out — the size metric and the tile
/// backstop in particular.
///
/// # Errors
///
/// [`LayoutError::UnknownNode`] or [`LayoutError::Arrow`].
pub fn layout_with(
    tree: &Tree,
    generation: TreeGeneration,
    root: NodeId,
    options: &LayoutOptions,
) -> Result<BinaryResponse, LayoutError> {
    let tiles = layout_tiles(tree, root, options)?;
    tiles_to_response(&tiles, generation)
}
