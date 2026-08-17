//! Pure geometry: the squarified row algorithm and the sliced (icicle /
//! sunburst) partition.
//!
//! Everything here is `f64` in and `f64` out. Nothing here reads a `Tree`,
//! which is what makes the aspect-ratio and sum-preservation properties
//! testable without a fixture.

/// An axis-aligned rectangle in CSS pixels.
///
/// The sliced layouts reuse it with `x` as the offset along the parent's extent
/// and `w` as the extent itself; `y` and `h` are written by the depth-axis pass
/// once the deepest surviving level is known.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

impl Rect {
    pub(crate) const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    pub(crate) fn area(self) -> f64 {
        self.w * self.h
    }

    /// The shorter edge — the one a squarified row is laid along.
    pub(crate) fn short_side(self) -> f64 {
        self.w.min(self.h)
    }
}

/// One candidate child, reused across frames so the traversal allocates once.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    /// Raw [`rdirstat_core::NodeId`] bits.
    pub(crate) node: u32,
    /// Bytes under this child, in the selected metric.
    pub(crate) weight: f64,
    /// The category index, carried so the emit path needs no second lookup.
    pub(crate) category: u8,
    /// Where the partition put it.
    pub(crate) rect: Rect,
}

impl Slot {
    pub(crate) const fn new(node: u32, weight: f64, category: u8) -> Self {
        Self {
            node,
            weight,
            category,
            rect: Rect::ZERO,
        }
    }
}

/// Orders candidates by descending weight, breaking ties on ascending node id.
///
/// The tie-break is what makes the whole crate deterministic: sibling order in
/// the arena is head-insertion order and explicitly *not* a UI contract, so a
/// layout that depended on it would not be reproducible.
pub(crate) fn order_desc(left: &Slot, right: &Slot) -> core::cmp::Ordering {
    right
        .weight
        .partial_cmp(&left.weight)
        .unwrap_or(core::cmp::Ordering::Equal)
        .then_with(|| left.node.cmp(&right.node))
}

/// Sorts `slots` descending, sorting only the prefix that can possibly be drawn.
///
/// A directory with 500k entries in a 400x300 rect can show at most a few
/// thousand tiles; sorting the whole list to discard 99% of it is the difference
/// between a responsive resize and a stall. `cap` is the caller's bound on drawn
/// children, computed from the region area and the cutoff.
pub(crate) fn sort_drawable_prefix(slots: &mut Vec<Slot>, cap: usize) {
    if cap < slots.len() {
        slots.select_nth_unstable_by(cap, order_desc);
        slots.truncate(cap);
    }
    slots.sort_unstable_by(order_desc);
}

/// The worst (largest) aspect ratio in a squarified row.
///
/// `max` and `min` are the largest and smallest tile *areas* in the row, `sum`
/// the row's total area, and `side` the length of the edge the row is laid
/// along. Straight out of Bruls, Huizing and van Wijk, "Squarified Treemaps".
fn worst_ratio(max: f64, min: f64, sum: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 || min <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = sum * sum;
    let w2 = side * side;
    ((w2 * max) / s2).max(s2 / (w2 * min))
}

/// Lays `slots` out inside `region` as a squarified treemap.
///
/// `slots` must already be sorted by descending weight. Returns the number of
/// leading slots that received a rectangle: the tail is deliberately left
/// unplaced once tiles fall below `min_area`, and the parent's fill shows
/// through the region they would have occupied.
///
/// `total` is the sum of **all** original children's weights, not just the ones
/// in `slots` — truncating the tail must not inflate the survivors.
pub(crate) fn squarify(slots: &mut [Slot], region: Rect, total: f64, min_area: f64) -> usize {
    if slots.is_empty() || total <= 0.0 || region.w <= 0.0 || region.h <= 0.0 {
        return 0;
    }
    let scale = region.area() / total;
    let count = slots.len();
    let mut free = region;
    let mut placed = 0_usize;

    while placed < count {
        let first_area = slots.get(placed).map_or(0.0, |slot| slot.weight * scale);
        if first_area < min_area {
            break;
        }
        let side = free.short_side();
        if side <= 0.0 {
            break;
        }

        // Grow the row while doing so improves the worst aspect ratio. Slots are
        // sorted descending, so the row max is always the first entry and the
        // row min is always the last one added.
        let mut row_area = first_area;
        let mut best = worst_ratio(first_area, first_area, first_area, side);
        let mut end = placed + 1;
        while end < count {
            let next = slots.get(end).map_or(0.0, |slot| slot.weight * scale);
            if next <= 0.0 {
                break;
            }
            let candidate_sum = row_area + next;
            let candidate = worst_ratio(first_area, next, candidate_sum, side);
            if candidate > best {
                break;
            }
            best = candidate;
            row_area = candidate_sum;
            end += 1;
        }

        let thickness = row_area / side;
        if !thickness.is_finite() || thickness <= 0.0 {
            break;
        }
        let horizontal = free.w <= free.h;
        let mut cursor = 0.0_f64;
        for index in placed..end {
            let Some(slot) = slots.get_mut(index) else {
                break;
            };
            let area = slot.weight * scale;
            let length = (area / thickness).min(side - cursor).max(0.0);
            slot.rect = if horizontal {
                Rect {
                    x: free.x + cursor,
                    y: free.y,
                    w: length,
                    h: thickness.min(free.h),
                }
            } else {
                Rect {
                    x: free.x,
                    y: free.y + cursor,
                    w: thickness.min(free.w),
                    h: length,
                }
            };
            cursor += length;
        }

        if horizontal {
            free.y += thickness;
            free.h -= thickness;
        } else {
            free.x += thickness;
            free.w -= thickness;
        }
        placed = end;

        if free.w <= 0.0 || free.h <= 0.0 {
            break;
        }
    }

    placed
}

/// Lays `slots` out as consecutive sub-intervals of `[offset, offset + extent)`.
///
/// This is the icicle and — after the polar transform at emit time — the
/// sunburst. `slots` must be sorted by descending weight; the walk stops at the
/// first sub-interval below `min_extent`, which is sound precisely because the
/// order is descending.
///
/// Returns the number of leading slots that received an interval.
pub(crate) fn slice(slots: &mut [Slot], offset: f64, extent: f64, total: f64, min_extent: f64) -> usize {
    if slots.is_empty() || total <= 0.0 || extent <= 0.0 {
        return 0;
    }
    let scale = extent / total;
    let end = offset + extent;
    let mut cursor = offset;
    let mut placed = 0_usize;
    for slot in slots.iter_mut() {
        let width = slot.weight * scale;
        if width < min_extent {
            break;
        }
        let clamped = width.min(end - cursor).max(0.0);
        if clamped < min_extent {
            break;
        }
        slot.rect = Rect {
            x: cursor,
            y: 0.0,
            w: clamped,
            h: 0.0,
        };
        cursor += clamped;
        placed += 1;
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::{Rect, Slot, order_desc, slice, sort_drawable_prefix, squarify};

    fn slots(weights: &[f64]) -> Vec<Slot> {
        weights
            .iter()
            .enumerate()
            .map(|(index, weight)| Slot::new(u32::try_from(index).expect("test index fits in u32"), *weight, 0))
            .collect()
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        let epsilon = 1e-9;
        a.x + a.w > b.x + epsilon && b.x + b.w > a.x + epsilon && a.y + a.h > b.y + epsilon && b.y + b.h > a.y + epsilon
    }

    #[test]
    fn ordering_is_total_and_breaks_ties_on_node_id() {
        let mut list = [Slot::new(7, 5.0, 0), Slot::new(2, 5.0, 0), Slot::new(9, 9.0, 0)];
        list.sort_unstable_by(order_desc);
        assert_eq!(list.iter().map(|slot| slot.node).collect::<Vec<_>>(), vec![9, 2, 7]);
    }

    #[test]
    fn the_drawable_prefix_keeps_the_heaviest_and_stays_sorted() {
        let mut list = slots(&[1.0, 9.0, 3.0, 7.0, 5.0]);
        sort_drawable_prefix(&mut list, 3);
        assert_eq!(list.len(), 3);
        let weights: Vec<f64> = list.iter().map(|slot| slot.weight).collect();
        assert_eq!(weights, vec![9.0, 7.0, 5.0]);
    }

    #[test]
    fn a_cap_at_or_above_the_length_keeps_everything() {
        let mut list = slots(&[1.0, 9.0, 3.0]);
        sort_drawable_prefix(&mut list, 99);
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn squarified_tiles_are_proportional_disjoint_and_inside_the_region() {
        let region = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 400.0,
        };
        let weights = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let total: f64 = weights.iter().sum();
        let mut list = slots(&weights);
        list.sort_unstable_by(order_desc);
        let placed = squarify(&mut list, region, total, 0.0);
        assert_eq!(placed, weights.len());

        let scale = region.area() / total;
        for slot in &list {
            let expected = slot.weight * scale;
            let actual = slot.rect.area();
            assert!(
                (actual - expected).abs() <= expected * 1e-6 + 1e-6,
                "area {actual} != {expected}"
            );
            assert!(slot.rect.x >= -1e-9 && slot.rect.y >= -1e-9);
            assert!(slot.rect.x + slot.rect.w <= region.w + 1e-6);
            assert!(slot.rect.y + slot.rect.h <= region.h + 1e-6);
        }
        for (index, left) in list.iter().enumerate() {
            for right in list.iter().skip(index + 1) {
                assert!(!overlaps(left.rect, right.rect), "tiles overlap");
            }
        }
    }

    #[test]
    fn squarified_aspect_ratios_beat_a_naive_strip() {
        let region = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 400.0,
        };
        let weights = [1.0_f64; 16];
        let total: f64 = weights.iter().sum();
        let mut list = slots(&weights);
        let placed = squarify(&mut list, region, total, 0.0);
        assert_eq!(placed, 16);
        for slot in &list {
            let ratio = (slot.rect.w / slot.rect.h).max(slot.rect.h / slot.rect.w);
            assert!(ratio < 2.0, "aspect ratio {ratio} is not square enough");
        }
    }

    #[test]
    fn squarify_stops_at_the_first_sub_minimum_area() {
        let region = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        // Two big children and a long tail of dust.
        let mut weights = vec![5_000.0, 4_000.0];
        weights.extend(core::iter::repeat_n(1.0, 500));
        let total: f64 = weights.iter().sum();
        let mut list = slots(&weights);
        list.sort_unstable_by(order_desc);
        let placed = squarify(&mut list, region, total, 9.0);
        assert_eq!(placed, 2, "the dust must not be laid out");
    }

    #[test]
    fn squarify_refuses_a_degenerate_region_or_total() {
        let mut list = slots(&[1.0]);
        assert_eq!(squarify(&mut list, Rect::ZERO, 1.0, 0.0), 0);
        let region = Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        assert_eq!(squarify(&mut list, region, 0.0, 0.0), 0);
    }

    #[test]
    fn slices_tile_the_extent_exactly_and_in_order() {
        let weights = [4.0, 3.0, 2.0, 1.0];
        let total: f64 = weights.iter().sum();
        let mut list = slots(&weights);
        list.sort_unstable_by(order_desc);
        let placed = slice(&mut list, 20.0, 100.0, total, 0.0);
        assert_eq!(placed, 4);
        let mut cursor = 20.0;
        for slot in &list {
            assert!((slot.rect.x - cursor).abs() < 1e-9, "{} != {cursor}", slot.rect.x);
            cursor += slot.rect.w;
        }
        assert!((cursor - 120.0).abs() < 1e-9, "{cursor}");
    }

    #[test]
    fn slices_stop_at_the_cutoff() {
        let weights = [90.0, 9.0, 1.0];
        let total: f64 = weights.iter().sum();
        let mut list = slots(&weights);
        list.sort_unstable_by(order_desc);
        // 100 px of extent: the tail is 1 px wide, below a 5 px cutoff.
        let placed = slice(&mut list, 0.0, 100.0, total, 5.0);
        assert_eq!(placed, 2);
    }
}
