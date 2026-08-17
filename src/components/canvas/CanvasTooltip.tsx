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
        <span className="tabular-nums">{formatShare(share)} of view</span>
      </div>
      {logical === null && allocated === null ? null : (
        <div className="mt-0.5 flex gap-3 text-[11px] tabular-nums">
          {logical === null ? null : <span>{logical} logical</span>}
          {allocated === null ? null : <span>{allocated} allocated</span>}
        </div>
      )}
    </div>
  );
}
