/**
 * The key to the treemap's colours.
 *
 * Without one, the colour encoding is decoration: a user can see that two
 * tiles differ but not what the difference means, and the whole point of
 * classifying by content type is lost. docs/05-UI.md asks for colour to be
 * *information*, which obliges the app to say what it encodes.
 *
 * Two decisions worth keeping:
 *
 * 1. **It lists what is on screen, not the whole taxonomy.** Twenty-five rows
 *    is a wall of names that pushes the tree table below the fold; the handful
 *    of categories actually present in the current subtree is a legend someone
 *    reads. `present` comes from the layout batch, so drilling in shortens it.
 * 2. **Swatches are `var(--cat-*)`, never a resolved colour.** The light/dark
 *    switch then costs no JS and cannot drift from what the canvas painted —
 *    both read the same custom property.
 *
 * The swatch is a `<span aria-hidden>` beside a real text label rather than a
 * coloured square with a `title`: colour alone is not an accessible encoding,
 * so the label is the primary channel and the colour is the redundant one.
 */

import { type ColorBy, type LegendEntry, legendEntries } from "./palette.ts";
import { categoryOf } from "@/lib/categories";
import { cn } from "@/lib/utils";

export interface CategoryLegendProps {
  colorBy: ColorBy;
  /** `CategoryId`s present in the current layout; `null` lists everything. */
  present: ReadonlySet<number> | null;
  /** Highlight one row — the category under the cursor. */
  highlighted?: number | null;
  onColorByChange?: (colorBy: ColorBy) => void;
  className?: string;
}

export function CategoryLegend({
  colorBy,
  present,
  highlighted = null,
  onColorByChange,
  className,
}: CategoryLegendProps) {
  const entries = legendEntries(colorBy, present);
  if (entries.length === 0) return null;

  // Under `"family"` the highlighted *category* has to be mapped onto its
  // family before it can match a row id.
  const highlightedId =
    highlighted === null
      ? null
      : colorBy === "family"
        ? categoryOf(highlighted).family
        : String(highlighted);

  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-border/60 px-3 py-2",
        className,
      )}
    >
      <span className="text-xs font-medium text-muted-foreground">Colour</span>

      {onColorByChange !== undefined && (
        <div role="radiogroup" aria-label="Colour tiles by" className="flex items-center gap-0.5">
          {(["family", "category"] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              role="radio"
              aria-checked={colorBy === mode}
              onClick={() => onColorByChange(mode)}
              className={cn(
                "rounded px-1.5 py-0.5 text-xs capitalize transition-colors",
                colorBy === mode
                  ? "bg-accent font-medium text-accent-foreground"
                  : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
              )}
            >
              {mode}
            </button>
          ))}
        </div>
      )}

      <ul className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
        {entries.map((entry) => (
          <LegendRow key={entry.id} entry={entry} highlighted={entry.id === highlightedId} />
        ))}
      </ul>
    </div>
  );
}

function LegendRow({ entry, highlighted }: { entry: LegendEntry; highlighted: boolean }) {
  return (
    <li
      className={cn(
        "flex shrink-0 items-center gap-1.5 rounded px-1 text-xs transition-colors",
        highlighted ? "bg-accent text-foreground" : "text-muted-foreground",
      )}
    >
      <span
        aria-hidden
        className="size-2.5 shrink-0 rounded-[2px] ring-1 ring-inset ring-black/20"
        style={{ backgroundColor: entry.colorVar }}
      />
      <span className="truncate">{entry.label}</span>
    </li>
  );
}
