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
import { X } from "lucide-react";

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
  /**
   * The active filter as `CategoryId`s, or `null` for "show everything".
   *
   * Always category ids, never families, even while the legend is showing
   * families — see [`LegendEntry.categoryIds`]. Omitting `onFilterChange`
   * renders the legend as a plain key with no interaction, which is what the
   * settings/help surface wants.
   */
  selected?: readonly number[] | null;
  onFilterChange?: (selected: readonly number[] | null) => void;
  className?: string;
}

export function CategoryLegend({
  colorBy,
  present,
  highlighted = null,
  onColorByChange,
  selected = null,
  onFilterChange,
  className,
}: CategoryLegendProps) {
  const entries = legendEntries(colorBy, present);
  if (entries.length === 0) return null;

  const active = selected !== null && selected.length > 0;
  const selectedSet = new Set(selected ?? []);
  const interactive = onFilterChange !== undefined;

  /*
   * Clicking a row shows only that thing; clicking it again clears.
   *
   * Toggle-in/toggle-out over a set, rather than single-select, because the
   * one-click case is identical either way — clicking a row when nothing is
   * filtered gives you exactly that row — while the set also answers "caches
   * AND build junk", which is the actual question someone reclaiming space is
   * asking. An empty set means no filter rather than an empty view: filtering
   * everything out is never what a click meant.
   */
  const toggle = (entry: LegendEntry): void => {
    if (onFilterChange === undefined) return;
    const isOn = entry.categoryIds.every((id) => selectedSet.has(id));
    const next = new Set(selectedSet);
    for (const id of entry.categoryIds) {
      if (isOn) next.delete(id);
      else next.add(id);
    }
    onFilterChange(next.size === 0 ? null : [...next]);
  };

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

      <ul className="flex min-w-0 flex-wrap items-center gap-x-1 gap-y-1">
        {entries.map((entry) => (
          <LegendRow
            key={entry.id}
            entry={entry}
            highlighted={entry.id === highlightedId}
            // Under a filter, the rows that are NOT in it are dimmed rather
            // than hidden: the legend is also the control that puts them back,
            // so removing them would remove the way out.
            selected={entry.categoryIds.every((id) => selectedSet.has(id))}
            dimmed={active && !entry.categoryIds.some((id) => selectedSet.has(id))}
            onToggle={interactive ? () => toggle(entry) : undefined}
          />
        ))}
      </ul>

      {active && onFilterChange !== undefined && (
        <button
          type="button"
          onClick={() => onFilterChange(null)}
          className="ml-auto flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-xs text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <X aria-hidden className="size-3" />
          Show all
        </button>
      )}
    </div>
  );
}

function LegendRow({
  entry,
  highlighted,
  selected,
  dimmed,
  onToggle,
}: {
  entry: LegendEntry;
  highlighted: boolean;
  selected: boolean;
  dimmed: boolean;
  onToggle?: () => void;
}) {
  const body = (
    <>
      <span
        aria-hidden
        className="size-2.5 shrink-0 rounded-[2px] ring-1 ring-inset ring-black/20"
        style={{ backgroundColor: entry.colorVar }}
      />
      <span className="truncate">{entry.label}</span>
    </>
  );

  const shared = cn(
    "flex shrink-0 items-center gap-1.5 rounded px-1.5 py-0.5 text-xs transition-colors",
    highlighted && "bg-accent text-foreground",
    !highlighted && (selected ? "text-foreground" : "text-muted-foreground"),
    dimmed && "opacity-40",
  );

  // Without a handler this is a key, not a control, and must not claim to be
  // focusable or pressable.
  if (onToggle === undefined) {
    return <li className={shared}>{body}</li>;
  }

  return (
    <li className="shrink-0">
      <button
        type="button"
        // `aria-pressed` rather than a checkbox role: these are toggle buttons
        // that filter a view, not form inputs that submit a value.
        aria-pressed={selected}
        onClick={onToggle}
        title={selected ? `Stop filtering to ${entry.label}` : `Show only ${entry.label}`}
        className={cn(
          shared,
          "cursor-default hover:bg-accent hover:text-foreground",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
          selected && "bg-accent ring-1 ring-inset ring-border",
        )}
      >
        {body}
      </button>
    </li>
  );
}
