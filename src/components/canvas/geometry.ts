/* =============================================================================
 * Geometry: hit-testing, the sunburst polar frame, and magnitude ranking.
 *
 * Hit-testing is a REVERSE LINEAR SCAN over the typed arrays. At a few thousand
 * drawn tiles that is microseconds, it allocates nothing, and it needs no
 * rebuild when the batch changes — a quadtree here would be a cache to
 * invalidate on every navigate in exchange for nothing measurable. Reverse
 * order matters: the backend emits parents before children, the renderer paints
 * in that order, so the last match is the topmost tile.
 * ========================================================================== */

import type { LayoutBatch, LayoutKind } from "./types.ts";

export const NO_HIT = -1;

const TAU = Math.PI * 2;

/**
 * Polar frame for a sunburst batch.
 *
 * The pinned schema reuses `x/y/w/h` as `(start_angle, inner_radius, sweep,
 * thickness)` but does not pin their UNITS, and `rdirstat-treemap` is being
 * written in parallel with this file. Rather than guess, the frame is derived
 * from the batch itself:
 *
 *   - angles are scaled so the widest observed extent becomes exactly one full
 *     turn — a no-op when the backend already emits radians over a full circle,
 *     and correct when it emits turns (0..1) or degrees;
 *   - radii are scaled so the outermost ring touches the inscribed circle of the
 *     viewport, which is what a sunburst should do at any unit.
 *
 * Both are computed once per batch, not per frame.
 */
export interface SunburstFrame {
  readonly cx: number;
  readonly cy: number;
  readonly angleScale: number;
  readonly radiusScale: number;
  readonly maxRadius: number;
}

export function computeSunburstFrame(batch: LayoutBatch, width: number, height: number): SunburstFrame {
  let maxAngle = 0;
  let maxRadius = 0;
  for (let i = 0; i < batch.count; i += 1) {
    const angleEnd = batch.x[i] + batch.w[i];
    if (angleEnd > maxAngle) maxAngle = angleEnd;
    const radiusEnd = batch.y[i] + batch.h[i];
    if (radiusEnd > maxRadius) maxRadius = radiusEnd;
  }
  const inscribed = Math.min(width, height) / 2;
  return {
    cx: width / 2,
    cy: height / 2,
    angleScale: maxAngle > 0 ? TAU / maxAngle : 0,
    radiusScale: maxRadius > 0 ? inscribed / maxRadius : 0,
    maxRadius: inscribed,
  };
}

/** Resolved pixel geometry of one tile. Filled in place; never allocated in a loop. */
export interface TileRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Resolved polar geometry of one sunburst arc. */
export interface TileArc {
  startAngle: number;
  endAngle: number;
  innerRadius: number;
  outerRadius: number;
}

export function readRect(batch: LayoutBatch, index: number, out: TileRect): TileRect {
  out.x = batch.x[index];
  out.y = batch.y[index];
  out.w = batch.w[index];
  out.h = batch.h[index];
  return out;
}

export function readArc(batch: LayoutBatch, index: number, frame: SunburstFrame, out: TileArc): TileArc {
  const start = batch.x[index] * frame.angleScale;
  out.startAngle = start;
  out.endAngle = start + batch.w[index] * frame.angleScale;
  out.innerRadius = batch.y[index] * frame.radiusScale;
  out.outerRadius = out.innerRadius + batch.h[index] * frame.radiusScale;
  return out;
}

/**
 * Topmost tile containing `(px, py)` in CSS pixels, or `NO_HIT`.
 * Zero allocations; reads the typed arrays directly.
 */
export function hitTestRect(batch: LayoutBatch, px: number, py: number): number {
  for (let i = batch.count - 1; i >= 0; i -= 1) {
    const x = batch.x[i];
    if (px < x) continue;
    const y = batch.y[i];
    if (py < y) continue;
    if (px < x + batch.w[i] && py < y + batch.h[i]) return i;
  }
  return NO_HIT;
}

/** Topmost sunburst arc containing `(px, py)` in CSS pixels, or `NO_HIT`. */
export function hitTestSunburst(batch: LayoutBatch, frame: SunburstFrame, px: number, py: number): number {
  if (frame.angleScale === 0 || frame.radiusScale === 0) return NO_HIT;
  const dx = px - frame.cx;
  const dy = py - frame.cy;
  const radius = Math.sqrt(dx * dx + dy * dy);
  if (radius > frame.maxRadius) return NO_HIT;
  let theta = Math.atan2(dy, dx);
  if (theta < 0) theta += TAU;

  for (let i = batch.count - 1; i >= 0; i -= 1) {
    const inner = batch.y[i] * frame.radiusScale;
    if (radius < inner) continue;
    const outer = inner + batch.h[i] * frame.radiusScale;
    if (radius >= outer) continue;

    const sweep = batch.w[i] * frame.angleScale;
    if (sweep >= TAU) return i;
    const start = batch.x[i] * frame.angleScale;
    let delta = theta - start;
    // Normalise into [0, TAU) so a wedge crossing the 3-o'clock seam still hits.
    delta -= Math.floor(delta / TAU) * TAU;
    if (delta < sweep) return i;
  }
  return NO_HIT;
}

/** Kind-aware dispatch. The sunburst frame is ignored by the rectangular kinds. */
export function hitTest(batch: LayoutBatch, frame: SunburstFrame | null, px: number, py: number): number {
  if (batch.kind === "sunburst") {
    return frame === null ? NO_HIT : hitTestSunburst(batch, frame, px, py);
  }
  return hitTestRect(batch, px, py);
}

/**
 * The layout's own measure of "how big is this tile".
 *
 * The pinned schema carries no byte count, so share-of-view is derived from the
 * geometry the backend already sized by bytes: area for the rectangular kinds,
 * angular sweep for the sunburst (whose thickness is a constant per ring and
 * therefore carries no information).
 */
export function tileMagnitude(batch: LayoutBatch, index: number): number {
  if (batch.kind === "sunburst") return batch.w[index];
  return batch.w[index] * batch.h[index];
}

/** The magnitude of the largest tile at the shallowest drawn depth, or 0. */
export function rootMagnitude(batch: LayoutBatch): number {
  if (batch.count === 0) return 0;
  let shallowest = Number.POSITIVE_INFINITY;
  for (let i = 0; i < batch.count; i += 1) {
    const d = batch.depth[i];
    if (d < shallowest) shallowest = d;
  }
  let total = 0;
  for (let i = 0; i < batch.count; i += 1) {
    if (batch.depth[i] === shallowest) total += tileMagnitude(batch, i);
  }
  return total;
}

export interface RankedTile {
  readonly index: number;
  readonly node: number;
  readonly depth: number;
  readonly category: number;
  /** Fraction of the drawn root, in `[0, 1]`. */
  readonly share: number;
  readonly magnitude: number;
}

/**
 * The `limit` largest drawn tiles, descending. This backs the accessible list —
 * a canvas is opaque to a screen reader, so this ranking is the equivalent
 * surface, and it is derived from exactly the tiles that were painted.
 *
 * Partial selection sort: O(count * limit) with limit fixed at ~25, which beats
 * sorting an index array of every tile and allocates one small array.
 */
export function rankLargestTiles(batch: LayoutBatch, limit: number): RankedTile[] {
  const total = rootMagnitude(batch);
  const wanted = Math.max(0, Math.min(limit, batch.count));
  const picked: number[] = [];
  const taken = new Uint8Array(batch.count);

  for (let slot = 0; slot < wanted; slot += 1) {
    let best = NO_HIT;
    let bestMagnitude = -1;
    for (let i = 0; i < batch.count; i += 1) {
      if (taken[i] === 1) continue;
      const magnitude = tileMagnitude(batch, i);
      if (magnitude > bestMagnitude) {
        bestMagnitude = magnitude;
        best = i;
      }
    }
    if (best === NO_HIT) break;
    taken[best] = 1;
    picked.push(best);
  }

  return picked.map((index) => {
    const magnitude = tileMagnitude(batch, index);
    return {
      index,
      node: batch.node[index],
      depth: batch.depth[index],
      category: batch.category[index],
      share: total > 0 ? magnitude / total : 0,
      magnitude,
    };
  });
}

/** First row index for `node`, or `NO_HIT`. Linear; used on tree -> canvas sync. */
export function indexOfNode(batch: LayoutBatch, node: number): number {
  for (let i = 0; i < batch.count; i += 1) {
    if (batch.node[i] === node) return i;
  }
  return NO_HIT;
}

/** Human label for the segmented control and the canvas `aria-label`. */
export function layoutKindLabel(kind: LayoutKind): string {
  switch (kind) {
    case "treemap":
      return "Treemap";
    case "icicle":
      return "Icicle";
    case "sunburst":
      return "Sunburst";
  }
}
