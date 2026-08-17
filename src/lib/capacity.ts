/**
 * Capacity-bar arithmetic, kept out of the component so it can be tested
 * without a DOM.
 *
 * The rule this file exists to enforce, from docs/05-UI.md:
 *
 *   "930 GB used plus 26 GB available does not reach 995 GB, and the 39 GB gap
 *    is purgeable space macOS has not yet reclaimed. Purgeable is therefore
 *    drawn *inside* capacity, never added beside it — segments that sum past
 *    the total are how a capacity UI loses trust."
 *
 * `used` is therefore defined as the residual (`total - importantAvailable`)
 * rather than measured independently. Defined that way, `used + purgeable +
 * available === total` is an identity, not something to remember to check.
 */

export interface CapacitySegments {
  readonly total: number;
  readonly used: number;
  readonly purgeable: number;
  readonly available: number;
  /** `used / total`, in `[0, 1]`. Drives the pressure colour. */
  readonly pressure: number;
}

function clamp(value: number, low: number, high: number): number {
  if (!Number.isFinite(value)) return low;
  return value < low ? low : value > high ? high : value;
}

/**
 * @param total  `statfs` total bytes.
 * @param available  bytes free right now.
 * @param importantAvailable  macOS "available for important usage", or `null`
 *   when the platform did not supply it — in which case there is no purgeable
 *   segment and none is invented.
 */
export function capacitySegments(
  total: number,
  available: number,
  importantAvailable: number | null,
): CapacitySegments {
  if (!Number.isFinite(total) || total <= 0) {
    return { total: 0, used: 0, purgeable: 0, available: 0, pressure: 0 };
  }

  const free = clamp(available, 0, total);
  // "Available for important usage" is never *less* than plain available. If a
  // platform ever reports that, treat it as absent rather than as a negative
  // purgeable segment.
  const important =
    importantAvailable === null ? free : clamp(Math.max(importantAvailable, free), free, total);

  const purgeable = important - free;
  const used = total - important;

  return { total, used, purgeable, available: free, pressure: used / total };
}

/**
 * Three thresholds, not a gradient. A continuously interpolated hue tells the
 * user nothing actionable at 61%; a step at 75% and 90% does.
 */
export function pressureClass(pressure: number): string {
  if (pressure >= 0.9) return "bg-pressure-critical";
  if (pressure >= 0.75) return "bg-pressure-warn";
  return "bg-pressure-ok";
}
