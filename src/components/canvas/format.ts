/* =============================================================================
 * Byte and percent formatting for the canvas surfaces.
 *
 * The canonical implementation is `rdirstat_core::format_si` / `format_iec` /
 * `format_percent`; the TypeScript port of it lives in `src/lib/format.ts` and
 * is shared by the whole app. This module does NOT reimplement it — a second
 * formatter is how the tooltip and the details panel start disagreeing about
 * the same file. It re-exports the shared functions under the local spelling
 * and adds the one thing the canvas needs and nothing else does.
 *
 * The import is relative rather than through the `@/` alias so these modules
 * stay loadable by `node --test`, which has no path mapping.
 * ========================================================================== */

export { formatSI as formatSi, formatIEC as formatIec, formatPercent } from "../../lib/format.ts";

/**
 * Percent from a float share in `[0, 1]`.
 *
 * The pinned `layout` schema carries geometry, not bytes, so "share of view" is
 * derived from tile area (or angular sweep) and is a float. `formatPercent`
 * takes two integers and cannot express it, which is why this exists.
 */
export function formatShare(share: number): string {
  if (!Number.isFinite(share) || share <= 0) return "0.0%";
  const tenths = Math.round(share * 1000);
  const units = Math.floor(tenths / 10);
  return `${units}.${tenths % 10}%`;
}
