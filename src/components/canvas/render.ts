/* =============================================================================
 * The paint.
 *
 * One `<canvas>`, `fillRect` straight off the typed arrays. No SVG, no DOM
 * tiles, no Recharts — docs/01-ARCHITECTURE.md#frontend-boundary: "thousands of
 * tiles as DOM elements will not hold the paint budget."
 *
 * Cost discipline in here:
 *   - the tile loops read the typed arrays directly and allocate nothing;
 *   - `fillStyle` is only assigned when the category changes from the previous
 *     tile, because assigning it re-parses the colour string every time;
 *   - borders are a second pass so the fill pass stays branch-light, and are
 *     skipped entirely for tiles too small to show one;
 *   - the sunburst culls arcs whose outer arc-length is sub-pixel.
 *
 * `drawLayout` is called on batch change, resize, selection change and theme
 * change. It is NOT called on mousemove: hover feedback is a DOM overlay.
 * ========================================================================== */

import { computeSunburstFrame, type SunburstFrame } from "./geometry.ts";
import type { Palette } from "./palette.ts";
import type { LayoutBatch } from "./types.ts";

/** Below this a border hairline is more ink than tile. */
const BORDER_MIN_PX = 5;
/** Arc segments shorter than this along their outer edge are not worth a path. */
const ARC_MIN_LENGTH_PX = 0.75;

export interface DrawOptions {
  readonly batch: LayoutBatch;
  readonly palette: Palette;
  /** CSS pixels. */
  readonly width: number;
  readonly height: number;
  readonly devicePixelRatio: number;
  /** Node ids currently selected. Empty is the common case. */
  readonly selected: ReadonlySet<number>;
  /** Precomputed frame for the sunburst kind; ignored otherwise. */
  readonly frame: SunburstFrame | null;
  /** Draw hierarchy hairlines. Off is measurably faster; on is the default. */
  readonly borders?: boolean;
}

export interface DrawStats {
  /** Tiles that actually produced a fill. */
  readonly drawn: number;
  /** Tiles skipped by the sub-pixel cull. */
  readonly culled: number;
  /** Wall time of the paint, in milliseconds. */
  readonly paintMs: number;
}

/**
 * Size the backing store for the device pixel ratio and return a context whose
 * user units are CSS pixels. Returns `null` when 2D is unavailable, which the
 * caller must surface rather than silently skip.
 */
export function prepareContext(
  canvas: HTMLCanvasElement,
  width: number,
  height: number,
  devicePixelRatio: number,
): CanvasRenderingContext2D | null {
  const ratio = devicePixelRatio > 0 ? devicePixelRatio : 1;
  const backingWidth = Math.max(1, Math.round(width * ratio));
  const backingHeight = Math.max(1, Math.round(height * ratio));
  if (canvas.width !== backingWidth) canvas.width = backingWidth;
  if (canvas.height !== backingHeight) canvas.height = backingHeight;
  const context = canvas.getContext("2d", { alpha: false });
  if (context === null) return null;
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  return context;
}

function drawRectangles(context: CanvasRenderingContext2D, options: DrawOptions): { drawn: number; culled: number } {
  const { batch, palette } = options;
  let drawn = 0;
  let culled = 0;
  let lastCategory = -1;

  for (let i = 0; i < batch.count; i += 1) {
    const w = batch.w[i];
    const h = batch.h[i];
    if (w <= 0 || h <= 0) {
      culled += 1;
      continue;
    }
    const category = batch.category[i];
    if (category !== lastCategory) {
      context.fillStyle = palette.fills[category] ?? palette.fills[0] ?? "#6b7280";
      lastCategory = category;
    }
    context.fillRect(batch.x[i], batch.y[i], w, h);
    drawn += 1;
  }

  if (options.borders !== false && drawn > 0) {
    context.strokeStyle = palette.border;
    context.lineWidth = 1;
    context.beginPath();
    for (let i = 0; i < batch.count; i += 1) {
      const w = batch.w[i];
      const h = batch.h[i];
      if (w < BORDER_MIN_PX || h < BORDER_MIN_PX) continue;
      // Half-pixel offset keeps a 1px hairline on the pixel grid.
      context.rect(batch.x[i] + 0.5, batch.y[i] + 0.5, w - 1, h - 1);
    }
    context.stroke();
  }

  return { drawn, culled };
}

function drawArcs(context: CanvasRenderingContext2D, options: DrawOptions, frame: SunburstFrame): { drawn: number; culled: number } {
  const { batch, palette } = options;
  let drawn = 0;
  let culled = 0;
  let lastCategory = -1;

  for (let i = 0; i < batch.count; i += 1) {
    const sweep = batch.w[i] * frame.angleScale;
    const inner = batch.y[i] * frame.radiusScale;
    const outer = inner + batch.h[i] * frame.radiusScale;
    if (sweep <= 0 || outer <= inner) {
      culled += 1;
      continue;
    }
    if (sweep * outer < ARC_MIN_LENGTH_PX) {
      culled += 1;
      continue;
    }
    const start = batch.x[i] * frame.angleScale;
    const end = start + sweep;

    const category = batch.category[i];
    if (category !== lastCategory) {
      context.fillStyle = palette.fills[category] ?? palette.fills[0] ?? "#6b7280";
      lastCategory = category;
    }

    context.beginPath();
    if (inner <= 0) {
      // A full disc for the centre ring: `arc` alone plus a line to the centre.
      context.moveTo(frame.cx, frame.cy);
      context.arc(frame.cx, frame.cy, outer, start, end, false);
      context.closePath();
    } else {
      context.arc(frame.cx, frame.cy, outer, start, end, false);
      context.arc(frame.cx, frame.cy, inner, end, start, true);
      context.closePath();
    }
    context.fill();
    drawn += 1;
  }

  return { drawn, culled };
}

function outlineSelection(context: CanvasRenderingContext2D, options: DrawOptions, frame: SunburstFrame | null): void {
  const { batch, selected, palette } = options;
  if (selected.size === 0) return;
  context.strokeStyle = palette.selection;
  context.lineWidth = 2;
  context.beginPath();

  if (batch.kind === "sunburst" && frame !== null) {
    for (let i = 0; i < batch.count; i += 1) {
      if (!selected.has(batch.node[i])) continue;
      const sweep = batch.w[i] * frame.angleScale;
      const inner = batch.y[i] * frame.radiusScale;
      const outer = inner + batch.h[i] * frame.radiusScale;
      if (sweep <= 0 || outer <= inner) continue;
      const start = batch.x[i] * frame.angleScale;
      context.moveTo(frame.cx + Math.cos(start) * inner, frame.cy + Math.sin(start) * inner);
      context.arc(frame.cx, frame.cy, outer, start, start + sweep, false);
      context.arc(frame.cx, frame.cy, inner, start + sweep, start, true);
      context.closePath();
    }
  } else {
    for (let i = 0; i < batch.count; i += 1) {
      if (!selected.has(batch.node[i])) continue;
      const w = batch.w[i];
      const h = batch.h[i];
      if (w <= 0 || h <= 0) continue;
      context.rect(batch.x[i] + 1, batch.y[i] + 1, Math.max(0, w - 2), Math.max(0, h - 2));
    }
  }
  context.stroke();
}

/** Paint one batch. Returns what was drawn so the shell can record the budget. */
export function drawLayout(canvas: HTMLCanvasElement, options: DrawOptions): DrawStats | null {
  const started = typeof performance === "undefined" ? 0 : performance.now();
  const context = prepareContext(canvas, options.width, options.height, options.devicePixelRatio);
  if (context === null) return null;

  context.fillStyle = options.palette.background;
  context.fillRect(0, 0, options.width, options.height);

  const frame =
    options.batch.kind === "sunburst"
      ? (options.frame ?? computeSunburstFrame(options.batch, options.width, options.height))
      : null;

  const counts = frame !== null ? drawArcs(context, options, frame) : drawRectangles(context, options);
  outlineSelection(context, options, frame);

  const finished = typeof performance === "undefined" ? 0 : performance.now();
  return { drawn: counts.drawn, culled: counts.culled, paintMs: finished - started };
}
