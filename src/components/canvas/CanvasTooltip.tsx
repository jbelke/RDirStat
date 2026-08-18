/* =============================================================================
 * Hover tooltip: path + size.
 *
 * Positioned IMPERATIVELY by `HierarchyCanvas` (it writes `style.transform` on
 * mousemove) so following the cursor costs no React render. This component only
 * re-renders when the hovered tile CHANGES, which is the contract that keeps
 * hover off the paint budget entirely — the canvas itself is never repainted
 * for hover.
 *
 * `aria-hidden`: the tooltip duplicates information the accessible list already
 * exposes, and a mouse-follow tooltip is not reachable by keyboard anyway.
 * ========================================================================== */

import type { Ref } from "react";

import { cn } from "@/lib/utils";

import { formatShare } from "./format.ts";

export interface CanvasTooltipProps {
  readonly ref: Ref<HTMLDivElement>;
  /** Percent-escaped display path, or a fallback label. */
  readonly title: string;
  readonly categoryLabel: string;
  readonly categoryColor: string;
  readonly logical: string | null;
  readonly allocated: string | null;
  readonly share: number;
  /**
   * Which of the two numbers the tile's AREA was drawn from. The other one is
   * still shown — they are two measurements of the same object and neither is
   * a correction of the other — but only one of them explains the rectangle
   * under the cursor, and the tooltip says which.
   */
  readonly areaMetric: "logical" | "allocated";
  /**
   * True when a category filter is re-proportioning the layout.
   *
   * The backend does not dim filtered-out bytes, it removes them from the
   * weights, so a directory tile then shows *only its matching bytes* while the
   * figures below are the whole node. Without this line a 50 GB folder with
   * 1 GB of matching data reads as a rendering bug.
   */
  readonly filtered: boolean;
  readonly visible: boolean;
  readonly reducedMotion: boolean;
}

export function CanvasTooltip({
  ref,
  title,
  categoryLabel,
  categoryColor,
  logical,
  allocated,
  share,
  areaMetric,
  filtered,
  visible,
  reducedMotion,
}: CanvasTooltipProps) {
  return (
    <div
      ref={ref}
      aria-hidden
      className={cn(
        "pointer-events-none absolute left-0 top-0 z-30 max-w-96 rounded-md border border-border",
        "bg-popover/95 px-2.5 py-1.5 text-popover-foreground shadow-lg backdrop-blur-sm",
        visible ? "opacity-100" : "opacity-0",
        reducedMotion ? "transition-none" : "transition-opacity duration-100",
      )}
      style={{ willChange: "transform" }}
    >
      <div className="truncate text-xs font-medium" title={title}>
        {title}
      </div>
      <div className="mt-0.5 flex items-center gap-2 text-[11px] text-muted-foreground">
        <span aria-hidden className="size-2 rounded-[2px]" style={{ backgroundColor: categoryColor }} />
        <span>{categoryLabel}</span>
        <span aria-hidden>·</span>
        <span className="tabular-nums">
          {formatShare(share)} of {filtered ? "the selected categories" : "view"}
        </span>
      </div>
      {logical === null && allocated === null ? null : (
        <div className="mt-0.5 flex gap-3 text-[11px] tabular-nums">
          {logical === null ? null : (
            <span className={cn(areaMetric === "logical" && "font-medium text-foreground")}>
              {logical} logical
            </span>
          )}
          {allocated === null ? null : (
            <span className={cn(areaMetric === "allocated" && "font-medium text-foreground")}>
              {allocated} allocated
            </span>
          )}
        </div>
      )}
      <div className="mt-0.5 text-[10px] text-muted-foreground">
        {filtered
          ? `Area is the ${areaMetric} bytes in the selected categories, not the whole item.`
          : `Area is ${areaMetric} bytes.`}
      </div>
    </div>
  );
}
