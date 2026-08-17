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
import { SegmentedControl } from "@/components/SegmentedControl";
import { categoryOf } from "@/lib/categories";
import { cn } from "@/lib/utils";

const COLOR_BY_OPTIONS = [
  {
    value: "family" as ColorBy,
    label: "Family",
    title: "Five headings — legible at tile size on a whole-volume scan",
  },
  {
    value: "category" as ColorBy,
    label: "Category",
    title: "All 25 categories — useful once a drill-down has narrowed the view",
  },
];

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
      {/* The same segmented control the layout and metric toggles use, rather
        * than a third bespoke style. It is also the reason the "Colour" label
        * is gone: the swatches to the right already say the row is about
        * colour, so the word was restating what the user can see. The control
        * carries the meaning in its accessible name instead, where it costs no
        * horizontal space and still reaches a screen reader. */}
      {onColorByChange !== undefined && (
        <SegmentedControl
          label="Colour tiles by"
          options={COLOR_BY_OPTIONS}
          value={colorBy}
          onChange={onColorByChange}
        />
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
