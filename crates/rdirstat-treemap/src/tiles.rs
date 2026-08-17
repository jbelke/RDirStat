//! The struct-of-arrays tile buffer that becomes an Arrow `RecordBatch`.
//!
//! Columns are held separately because that is the shape Arrow wants: building
//! `UInt32Array`/`Float32Array` from a `&[T]` is a memcpy, whereas an
//! array-of-structs would need a transpose per request.

use rdirstat_core::{CategoryId, NodeId};

/// One drawn tile, as the renderer sees it.
///
/// Coordinate meaning depends on the layout kind and is documented once, at the
/// crate root: rectangles in CSS pixels for treemap and icicle,
/// `(start_angle, inner_radius, sweep, thickness)` for sunburst.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    /// The arena node, or a tagged virtual `<Files>` group.
    pub node: NodeId,
    /// Levels below the layout root; the root itself is `0`.
    pub depth: u32,
    /// `x`, or the start angle in radians.
    pub x: f32,
    /// `y`, or the inner radius in CSS pixels.
    pub y: f32,
    /// Width, or the swept angle in radians.
    pub w: f32,
    /// Height, or the ring thickness in CSS pixels.
    pub h: f32,
    /// The content category index; the frontend resolves the colour.
    pub category: CategoryId,
}

/// What one layout run did, for the performance log required by docs/05-UI.md.
///
/// "Candidate node count alone is not evidence that the sub-pixel cutoff kept
/// the rendered set bounded" — so `visited` and `tiles` are both reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayoutStats {
    /// Tiles emitted, i.e. tiles the renderer will draw.
    pub tiles: u32,
    /// Frames popped from the traversal stack. Equal to `tiles` unless the
    /// backstop tripped.
    pub visited: u32,
    /// Child links examined. This is the honest measure of traversal work: it
    /// counts entries considered and discarded by the cutoff.
    pub considered: u64,
    /// Deepest emitted level, relative to the layout root.
    pub max_depth: u32,
    /// Whether the tile backstop stopped the walk early.
    pub truncated: bool,
}

/// Column-oriented tiles, ready for Arrow.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileBuffer {
    node: Vec<u32>,
    depth: Vec<u32>,
    x: Vec<f32>,
    y: Vec<f32>,
    w: Vec<f32>,
    h: Vec<f32>,
    category: Vec<u8>,
    stats: LayoutStats,
}

impl TileBuffer {
    /// An empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            node: Vec::new(),
            depth: Vec::new(),
            x: Vec::new(),
            y: Vec::new(),
            w: Vec::new(),
            h: Vec::new(),
            category: Vec::new(),
            stats: LayoutStats {
                tiles: 0,
                visited: 0,
                considered: 0,
                max_depth: 0,
                truncated: false,
            },
        }
    }

    /// A buffer with room for `tiles` rows, so the traversal does not reallocate.
    #[must_use]
    pub fn with_capacity(tiles: usize) -> Self {
        Self {
            node: Vec::with_capacity(tiles),
            depth: Vec::with_capacity(tiles),
            x: Vec::with_capacity(tiles),
            y: Vec::with_capacity(tiles),
            w: Vec::with_capacity(tiles),
            h: Vec::with_capacity(tiles),
            category: Vec::with_capacity(tiles),
            stats: LayoutStats::default(),
        }
    }

    pub(crate) fn push(&mut self, tile: Tile) {
        self.node.push(tile.node.raw());
        self.depth.push(tile.depth);
        self.x.push(tile.x);
        self.y.push(tile.y);
        self.w.push(tile.w);
        self.h.push(tile.h);
        self.category.push(tile.category.get());
        self.stats.tiles = self.stats.tiles.saturating_add(1);
        self.stats.max_depth = self.stats.max_depth.max(tile.depth);
    }

    pub(crate) fn stats_mut(&mut self) -> &mut LayoutStats {
        &mut self.stats
    }

    /// Rewrites the depth axis of every row from its depth.
    ///
    /// Icicle and sunburst place tiles on `(offset, extent)` during the walk and
    /// derive the depth axis afterwards, once the deepest surviving level is
    /// known. That is a pass over the *output*, not a second tree traversal.
    pub(crate) fn set_depth_axis(&mut self, step: f32) {
        for (index, depth) in self.depth.iter().enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "depth is bounded by MAX_TREE_DEPTH = 4096, exactly representable in f32"
            )]
            let offset = *depth as f32 * step;
            if let Some(slot) = self.y.get_mut(index) {
                *slot = offset;
            }
            if let Some(slot) = self.h.get_mut(index) {
                *slot = step;
            }
        }
    }

    /// Rows emitted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.node.len()
    }

    /// Whether nothing was drawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node.is_empty()
    }

    /// What the run did.
    #[must_use]
    pub const fn stats(&self) -> LayoutStats {
        self.stats
    }

    /// The `node` column, as raw [`NodeId`] bits.
    #[must_use]
    pub fn nodes(&self) -> &[u32] {
        &self.node
    }

    /// The `depth` column.
    #[must_use]
    pub fn depths(&self) -> &[u32] {
        &self.depth
    }

    /// The `x` column.
    #[must_use]
    pub fn xs(&self) -> &[f32] {
        &self.x
    }

    /// The `y` column.
    #[must_use]
    pub fn ys(&self) -> &[f32] {
        &self.y
    }

    /// The `w` column.
    #[must_use]
    pub fn ws(&self) -> &[f32] {
        &self.w
    }

    /// The `h` column.
    #[must_use]
    pub fn hs(&self) -> &[f32] {
        &self.h
    }

    /// The `category` column.
    #[must_use]
    pub fn categories(&self) -> &[u8] {
        &self.category
    }

    /// Row `index`, reassembled.
    #[must_use]
    pub fn tile(&self, index: usize) -> Option<Tile> {
        Some(Tile {
            node: NodeId::from_raw(*self.node.get(index)?),
            depth: *self.depth.get(index)?,
            x: *self.x.get(index)?,
            y: *self.y.get(index)?,
            w: *self.w.get(index)?,
            h: *self.h.get(index)?,
            category: CategoryId::from_raw(*self.category.get(index)?),
        })
    }

    /// Every row, in paint order.
    pub fn iter(&self) -> impl Iterator<Item = Tile> + '_ {
        (0..self.len()).filter_map(|index| self.tile(index))
    }
}

#[cfg(test)]
mod tests {
    use super::{Tile, TileBuffer};
    use rdirstat_core::{CategoryId, NodeId};

    fn tile(depth: u32) -> Tile {
        Tile {
            node: NodeId::ROOT,
            depth,
            x: 1.0,
            y: 0.0,
            w: 2.0,
            h: 0.0,
            category: CategoryId::UNCATEGORIZED,
        }
    }

    #[test]
    fn pushing_tracks_count_and_depth() {
        let mut buffer = TileBuffer::new();
        assert!(buffer.is_empty());
        buffer.push(tile(0));
        buffer.push(tile(3));
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.stats().tiles, 2);
        assert_eq!(buffer.stats().max_depth, 3);
        assert_eq!(buffer.nodes(), &[NodeId::ROOT.raw(), NodeId::ROOT.raw()]);
    }

    #[test]
    fn the_depth_axis_pass_derives_y_and_h_from_depth() {
        let mut buffer = TileBuffer::new();
        buffer.push(tile(0));
        buffer.push(tile(2));
        buffer.set_depth_axis(10.0);
        let first = buffer.tile(0).expect("row 0");
        let second = buffer.tile(1).expect("row 1");
        assert!((first.y - 0.0).abs() < f32::EPSILON);
        assert!((first.h - 10.0).abs() < f32::EPSILON);
        assert!((second.y - 20.0).abs() < f32::EPSILON);
        assert!((second.h - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn out_of_range_rows_are_none_rather_than_a_panic() {
        let buffer = TileBuffer::new();
        assert!(buffer.tile(0).is_none());
        assert_eq!(buffer.iter().count(), 0);
    }
}
