/**
 * `%Bar` — the share-of-parent cell.
 *
 * docs/05-UI.md argues for this from `pdu`: "you read the hierarchy and the
 * distribution in one pass, with no separate chart. That is exactly the `%Bar`
 * cell … and `pdu` is the argument for making it a default column rather than
 * an option."
 *
 * Two constraints make the implementation specific:
 *
 * - **A styled `div`, not an image and not a canvas.** It is drawn once per
 *   visible row, up to ~40 rows per frame while scrolling. Two nested divs with
 *   a percentage width cost nothing; an inline `<canvas>` per row costs a
 *   context per row.
 * - **Coloured by category, not by magnitude.** The bar and the treemap tile
 *   for the same node are then the same colour, which is what makes the
 *   palette learnable from the table (docs/05, "Category chip"). The colour is
 *   `var(--cat-*)`, resolved by CSS, so light/dark needs no JS.
 *
 * Accessibility: the bar is `aria-hidden` and the numeric share sits beside it
 * as text. A `role="meter"` per row would make VoiceOver announce a meter on
 * every arrow-key move through a four-million-row tree; the number is the
 * information, the bar is the encoding.
 */

import { categoryColorVar } from "@/lib/categories";
import { formatPercent, shareOf } from "@/lib/format";
import { cn } from "@/lib/utils";

export interface PercentBarProps {
  /** This row's byte count, in whichever metric the toolbar selected. */
  value: number;
  /** The parent's total in the same metric. Zero renders an empty bar, not NaN. */
  total: number;
  /** `CategoryId`. Drives the fill colour via `--cat-*`. */
  category: number;
  /** Show the `33.3%` label beside the bar. */
  showLabel?: boolean;
  /**
   * The subtree is known-incomplete (`flags::INCOMPLETE_SUBTREE`), so the share
   * is a floor. Rendered hatched rather than solid so it does not read as a
   * measured value.
   */
  partial?: boolean;
  className?: string;
}

export function PercentBar({
  value,
  total,
  category,
  showLabel = true,
  partial = false,
  className,
}: PercentBarProps) {
  const share = shareOf(value, total);
  const label = formatPercent(value, total);

  return (
    <div className={cn("flex items-center gap-2", className)}>
      {showLabel && (
        <span className="rds-numeric w-12 shrink-0 text-right text-xs text-muted-foreground">
          {label}
        </span>
      )}
      <div
        aria-hidden
        className="h-1.5 min-w-8 flex-1 overflow-hidden rounded-full bg-foreground/8"
      >
        <div
          className={cn("h-full rounded-full", partial && "opacity-70")}
          style={{
            width: `${share * 100}%`,
            backgroundColor: categoryColorVar(category),
            // A partial subtree gets a diagonal hatch so "this is a floor" is
            // visible without colour alone carrying the meaning.
            backgroundImage: partial
              ? "repeating-linear-gradient(135deg, rgb(0 0 0 / 0.28) 0 2px, transparent 2px 5px)"
              : undefined,
          }}
        />
      </div>
    </div>
  );
}
