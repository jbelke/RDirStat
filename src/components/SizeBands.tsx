/**
 * Size bands — "what are the big things", without reading down a sorted column.
 *
 * ## The unit problem, stated rather than hidden
 *
 * Band edges are binary (1024-based) because the threshold a person carries in
 * their head comes from `du -h`, which is 1024-based on both BSD and GNU. The
 * rest of this interface is decimal SI because Finder is. On this machine the
 * same directory reads:
 *
 * ```text
 * du -h /Applications   ->  128G
 * this app              ->  138 GB
 * ```
 *
 * Neither is wrong — `138 x 10^9 / 1024^3 = 128.5`. So every edge is rendered
 * **both ways**, IEC first because that is the number the user typed a command
 * to get, SI in parentheses because that is the number in the size column two
 * inches to the right. Showing one and silently meaning the other is how a
 * size UI loses trust.
 *
 * ## What is counted
 *
 * Files, by allocated bytes — what the filesystem actually spent, which is what
 * `du` measures. Directories are not banded: a directory's size is its subtree
 * total, so banding both it and its contents would count the same bytes twice
 * and the rows would not sum to the subtree. The bands partition the leaves.
 *
 * ## Two tables, not one
 *
 * The band rows and the file rows inside an expanded band do not describe the
 * same kind of thing and do not want the same columns — a band has a file count
 * and no modification time, a file has the reverse. One shared header would
 * therefore be half em dashes, and worse, a sortable header on it would be
 * ambiguous about *which* list it sorts. So the breakdown is its own `<table>`
 * with its own `<caption>` and its own sortable headers, nested in the
 * expansion row.
 *
 * The rejected alternative was `role="treegrid"` with the files as level-2 rows
 * of one grid. A treegrid promises that a child row is another row of the same
 * grid — same columns, same meaning per column. That is exactly what is not
 * true here, so the role would be a claim the DOM cannot honour. A plain
 * `table` with `aria-expanded` on the band rows says only what is true.
 *
 * ## Keyboard model
 *
 * One roving tab stop across *both* tables: Up/Down walk every visible row in
 * DOM order (which is visual order, so an expanded band's files are stepped
 * through in place), Home/End jump, Right/Left expand and collapse a band,
 * Left on a file row goes back to its band, Return selects (⌘/⇧ extend) and
 * Space toggles selection — the same verbs as `LargestItemsList`, per
 * docs/05-UI.md#accessibility-and-performance.
 *
 * Focus, selection, and hover stay three distinct states here for the same
 * reason the store keeps them apart: a ring is where the keyboard is, a fill is
 * what is selected, and merging them makes arrow-key navigation destructive.
 *
 * There is deliberately **no live region**. `aria-sort` on the header and the
 * button's own name already carry the sort state, and this view has an
 * expanded list that re-sorts on every click; an `aria-live` announcement per
 * change is how a previous component made VoiceOver unusable.
 */

import { ChevronDown, ChevronRight, ChevronUp, Copy, Eye, Info, Lock, Trash2 } from "lucide-react";
import { Fragment, useCallback, useMemo, useRef, useState } from "react";

import { CategoryChip } from "@/components/cells/CategoryChip";
import { formatCount, formatIEC, formatPercent, formatSI, formatMtime } from "@/lib/format";
import { categoryColorVar, categoryOf } from "@/lib/categories";
import type { SizeBandEntryView, SizeBandView } from "@/lib/ipc";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { BAND_ENTRY_LIMIT, useSizeBandEntries } from "@/lib/queries";
import { cn } from "@/lib/utils";

/** Module-level so the default does not allocate a new `Set` every render. */
const EMPTY_SELECTION: ReadonlySet<number> = new Set<number>();

export interface SizeBandsProps {
  rows: readonly SizeBandView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  /** Total allocated bytes of the subtree, for the share column. */
  subtreeAllocated: number | null;
  /** The subtree the bands describe, for the per-band breakdown. */
  generation: number;
  root: number | null;
  /** Reveal one file in Finder. Same handler the tree and the canvas use. */
  onReveal?: (node: number) => void;
  /** Move one file to the Trash. Refused unless deletion is armed. */
  onTrash?: (node: number) => void;
  /** Whether deletion is armed; drives the Trash item's wording, not just its state. */
  trashEnabled?: boolean;
  /**
   * The shared multi-selection, by NodeId. Defaults to empty so the component
   * still renders standalone; band rows are never in it, because a band is not
   * a node and cannot be revealed, trashed, or described.
   */
  selection?: ReadonlySet<number>;
  /**
   * Selection request for a file row. Same three modes and same meanings as
   * `TreeTable`, so ⌘-click and ⇧-click do the same thing on both routes.
   * Omitting it leaves the list navigable but not selectable — and in that case
   * no `aria-selected` is emitted at all, rather than telling a screen reader
   * every row is "not selected" when none of them can be.
   */
  onSelect?: (node: number, mode: "replace" | "toggle" | "add") => void;
  /**
   * The keyboard cursor moved onto a file row (`node`), or onto a band row,
   * which is not a node (`null`). Wire this to the store's `focused` so the
   * details panel follows the keyboard; it is intentionally separate from
   * `onSelect`, because moving the cursor must not change what is selected.
   */
  onFocusNode?: (node: number | null) => void;
  /** Pointer hover on a file row, for the tree <-> canvas highlight sync. */
  onHover?: (node: number | null) => void;
  className?: string;
}

/** `5 GiB – 50 GiB`, or `50 GiB and larger` for the open-ended top band. */
function edgeLabel(row: SizeBandView): string {
  if (row.upperBytes === null) return `${formatIEC(row.lowerBytes)} and larger`;
  if (row.lowerBytes === 0) return `under ${formatIEC(row.upperBytes)}`;
  return `${formatIEC(row.lowerBytes)} – ${formatIEC(row.upperBytes)}`;
}

/** The same edges in decimal SI, so the row reconciles with the size column. */
function conversionLabel(row: SizeBandView): string {
  if (row.upperBytes === null) return `${formatSI(row.lowerBytes)} and larger`;
  if (row.lowerBytes === 0) return `under ${formatSI(row.upperBytes)}`;
  return `${formatSI(row.lowerBytes)} – ${formatSI(row.upperBytes)}`;
}

/* ---------------------------------------------------------------------------
 * Sorting the breakdown
 *
 * This sorts **the fetched page**, never the band. The backend ranks the whole
 * band by allocated bytes and returns the top `BAND_ENTRY_LIMIT`; re-ordering
 * those rows by name or date cannot reach the rest, so the caption says so
 * whenever the list is capped and the order is no longer the one the cap was
 * taken in. Sorting 250 rows and implying ten million were sorted is the exact
 * dishonesty this view exists to avoid.
 * ------------------------------------------------------------------------- */

type EntrySortKey = "path" | "logical" | "allocated" | "category" | "mtime";

interface EntrySort {
  readonly key: EntrySortKey;
  readonly direction: "asc" | "desc";
}

/** The order the backend already returned the page in. */
const DEFAULT_ENTRY_SORT: EntrySort = { key: "allocated", direction: "desc" };

/**
 * First click on a column picks the direction that answers the question the
 * column is usually asked: biggest first, newest first, but names A→Z.
 */
const FIRST_DIRECTION: Record<EntrySortKey, "asc" | "desc"> = {
  path: "asc",
  logical: "desc",
  allocated: "desc",
  category: "asc",
  mtime: "desc",
};

const SORT_LABEL: Record<EntrySortKey, string> = {
  path: "path",
  logical: "logical size",
  allocated: "allocated bytes",
  category: "category",
  mtime: "modification time",
};

// `numeric` so `file10` sorts after `file9` rather than after `file1`, matching
// Finder's list view; `base` so case does not split a directory in two.
const PATH_COLLATOR = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function compareBy(a: SizeBandEntryView, b: SizeBandEntryView, key: EntrySortKey): number {
  switch (key) {
    case "path":
      return PATH_COLLATOR.compare(a.path, b.path);
    case "logical":
      return a.logical - b.logical;
    case "allocated":
      return a.allocated - b.allocated;
    // By the label that is on screen, not by the id: the id order is the
    // taxonomy's, which is not what someone clicking "Category" is reading.
    case "category":
      return PATH_COLLATOR.compare(categoryOf(a.category).label, categoryOf(b.category).label);
    case "mtime":
      return a.mtime - b.mtime;
  }
}

function sortEntries(
  rows: readonly SizeBandEntryView[] | undefined,
  sort: EntrySort,
): SizeBandEntryView[] | undefined {
  if (rows === undefined) return undefined;
  const factor = sort.direction === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const primary = compareBy(a, b, sort.key) * factor;
    // Ties break on the NodeId ascending in *both* directions — the same rule
    // `size_band_entries` uses — so flipping the direction does not silently
    // reshuffle equal-sized files.
    return primary !== 0 ? primary : a.node - b.node;
  });
}

/**
 * What the list is, in one or two sentences, stated on the table itself.
 *
 * The caveat appears only when it is true: if the band fits inside the cap
 * there is nothing beyond the fetched rows to be wrong about, and if the sort
 * is still the one the cap was taken in the list really is the band's top N.
 */
function captionText(listed: number, total: number, sort: EntrySort): string {
  const truncated = total > listed;
  const order = `${SORT_LABEL[sort.key]}, ${sort.direction === "asc" ? "ascending" : "descending"}`;
  if (!truncated) {
    return `All ${formatCount(listed)} files in this band, ordered by ${order}.`;
  }
  const head = `The ${formatCount(listed)} largest of ${formatCount(total)} — capped at ${formatCount(BAND_ENTRY_LIMIT)}, ordered by ${order}.`;
  if (sort.key === DEFAULT_ENTRY_SORT.key && sort.direction === DEFAULT_ENTRY_SORT.direction) {
    return head;
  }
  return `${head} Only these ${formatCount(listed)} rows are sorted: the band was ranked by allocated bytes before it was capped, so the ${SORT_LABEL[sort.key]} extreme for all ${formatCount(total)} files may not be listed here.`;
}

/**
 * `formatPercent` rounds to one decimal, so a file that is a real but tiny part
 * of a ten-million-file band prints `0.0%` — which reads as "none". `<0.1%` is
 * the same number without the false zero.
 */
function shareLabel(part: number, whole: number): string {
  if (whole <= 0) return "—";
  const label = formatPercent(part, whole);
  return label === "0.0%" && part > 0 ? "<0.1%" : label;
}

/**
 * The band table's header cells: sticky against the scroll container so the
 * columns stay named while a long expanded list is being read.
 */
const BAND_HEAD = "sticky top-0 z-10 border-b border-border/60 bg-background py-1.5 font-normal";

/* Row keys. One namespace for both tables, because one roving tab stop walks
 * both of them and the key is what identifies a position in that walk. */
const bandKey = (band: number): string => `band:${band}`;
const fileKey = (node: number): string => `file:${node}`;

export function SizeBands({
  rows,
  isLoading,
  error,
  subtreeAllocated,
  generation,
  root,
  onReveal,
  onTrash,
  trashEnabled = false,
  selection = EMPTY_SELECTION,
  onSelect,
  onFocusNode,
  onHover,
  className,
}: SizeBandsProps) {
  const [expanded, setExpanded] = useState<number | null>(null);
  const [notesOpen, setNotesOpen] = useState(false);
  const [entrySort, setEntrySort] = useState<EntrySort>(DEFAULT_ENTRY_SORT);
  // The roving tab stop, as a row key rather than an index: an index would move
  // under the user when the breakdown re-sorts, a key does not.
  const [cursor, setCursor] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const entries = useSizeBandEntries(generation, root, expanded);

  // Every hook runs before the loading/error returns below, so the hook order
  // cannot depend on whether the data arrived.

  // Largest first: the question this view answers is "what is big", and the
  // answer should not be at the bottom.
  //
  // Band rows are deliberately **not** sortable. The band axis is an axis: it
  // is ordered by size because that is what it measures, and re-ranking it by
  // "Allocated" would trade a readable distribution for one fact the share bar
  // already shows pre-attentively.
  const ordered = useMemo(() => (rows === undefined ? [] : [...rows].reverse()), [rows]);
  const sorted = useMemo(() => sortEntries(entries.data, entrySort), [entries.data, entrySort]);

  // Every focusable row, in visual order. Also the fallback rule for the tab
  // stop: if the cursor points at a row that no longer exists — a collapsed
  // band's file, a generation change — the first row takes the tab stop, so the
  // table can never end up with zero tab stops and become unreachable.
  const rowKeys = useMemo(() => {
    const keys: string[] = [];
    for (const row of ordered) {
      keys.push(bandKey(row.band));
      if (expanded === row.band && sorted !== undefined) {
        for (const entry of sorted) keys.push(fileKey(entry.node));
      }
    }
    return keys;
  }, [ordered, expanded, sorted]);
  const activeKey = cursor !== null && rowKeys.includes(cursor) ? cursor : (rowKeys[0] ?? null);

  const focusKey = useCallback((key: string) => {
    containerRef.current?.querySelector<HTMLTableRowElement>(`tr[data-rds-row="${key}"]`)?.focus();
  }, []);

  const moveFocus = useCallback(
    (from: string, target: number | "first" | "last") => {
      if (rowKeys.length === 0) return;
      const index = rowKeys.indexOf(from);
      const next =
        target === "first"
          ? 0
          : target === "last"
            ? rowKeys.length - 1
            : Math.min(Math.max(index + target, 0), rowKeys.length - 1);
      const key = rowKeys[next];
      if (key !== undefined) focusKey(key);
    },
    [rowKeys, focusKey],
  );

  const handleRowFocus = useCallback(
    (key: string, node: number | null) => {
      setCursor((current) => (current === key ? current : key));
      onFocusNode?.(node);
    },
    [onFocusNode],
  );

  const toggleBand = useCallback((band: number) => {
    setExpanded((current) => (current === band ? null : band));
    // Collapsing unmounts that band's file rows. Parking the cursor on the band
    // itself keeps the tab stop on something that is still on screen, and is
    // also where a keyboard user expects to be left after Left-arrow.
    setCursor(bandKey(band));
  }, []);

  const handleBandKeyDown = useCallback(
    (event: React.KeyboardEvent, band: number, expandable: boolean) => {
      const key = bandKey(band);
      const open = expanded === band;
      switch (event.key) {
        case "ArrowDown":
          moveFocus(key, 1);
          break;
        case "ArrowUp":
          moveFocus(key, -1);
          break;
        case "Home":
          moveFocus(key, "first");
          break;
        case "End":
          moveFocus(key, "last");
          break;
        case "ArrowRight":
          // Open a closed band; step into an open one. Never a toggle, so
          // repeating the key cannot close what it just opened.
          if (expandable && !open) toggleBand(band);
          else if (open) moveFocus(key, 1);
          break;
        case "ArrowLeft":
          if (open) toggleBand(band);
          break;
        case "Enter":
        case " ":
          if (expandable) toggleBand(band);
          break;
        default:
          return;
      }
      // Only after a handled key: Space and the arrows still scroll and type
      // normally everywhere this component does not claim them.
      event.preventDefault();
    },
    [expanded, moveFocus, toggleBand],
  );

  const handleFileKeyDown = useCallback(
    (event: React.KeyboardEvent, node: number, band: number) => {
      const key = fileKey(node);
      switch (event.key) {
        case "ArrowDown":
          moveFocus(key, 1);
          break;
        case "ArrowUp":
          moveFocus(key, -1);
          break;
        case "Home":
          moveFocus(key, "first");
          break;
        case "End":
          moveFocus(key, "last");
          break;
        case "ArrowLeft":
          // Back to the band this file is in, without collapsing it: the second
          // Left, now on the band row, is the one that closes it.
          focusKey(bandKey(band));
          break;
        case "Enter":
          onSelect?.(node, event.metaKey ? "toggle" : event.shiftKey ? "add" : "replace");
          break;
        case " ":
          onSelect?.(node, "toggle");
          break;
        default:
          return;
      }
      event.preventDefault();
    },
    [focusKey, moveFocus, onSelect],
  );

  if (error !== null) {
    return (
      // One-shot alert, not a live region: it fires when a query fails and then
      // stops, which is what `role="alert"` is for.
      <div role="alert" className={cn("p-6 text-sm text-pressure-critical", className)}>
        Size bands could not be computed: {error.message}
      </div>
    );
  }
  if (isLoading || rows === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Counting…</div>;
  }

  const widest = ordered.reduce((most, row) => Math.max(most, row.allocated), 0);
  const totalFiles = ordered.reduce((sum, row) => sum + row.files, 0);

  return (
    <div
      ref={containerRef}
      onPointerLeave={() => onHover?.(null)}
      className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}
    >
      <header className="relative flex shrink-0 items-center gap-2">
        <h2 className="text-sm font-medium">Files by size</h2>
        <span className="rds-numeric text-xs text-muted-foreground">
          {formatCount(totalFiles)} files
        </span>
        {/* The explanation is real and occasionally necessary, but it is the
          * same paragraph every time and it was the tallest thing on the route.
          * One icon, opened on demand. */}
        <button
          type="button"
          aria-expanded={notesOpen}
          onClick={() => setNotesOpen((open) => !open)}
          title="How these bands are defined"
          className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Info aria-hidden className="size-3.5" />
          <span className="sr-only">How these bands are defined</span>
        </button>
        {notesOpen && (
          <div className="absolute left-0 top-full z-50 mt-1 flex w-[30rem] max-w-[80vw] flex-col gap-2 rounded-lg border border-border bg-popover p-3 text-xs text-muted-foreground shadow-xl">
            <p>
              Bands are powers of 1024, matching <code className="font-mono">du -h</code>. The
              decimal equivalent is shown beneath each one, because the size column and Finder are
              decimal SI — the same bytes read differently in the two systems.
            </p>
            <p>
              Directories are not counted: these bands partition the files, so the rows sum to the
              subtree exactly once.
            </p>
            <p>
              Allocated and logical are never summed. Allocated is what the filesystem spent,
              logical is what the files claim, and on APFS they disagree for sparse, compressed, and
              cloned files.
            </p>
            <p>
              Expanding a band lists its largest files only — up to {formatCount(BAND_ENTRY_LIMIT)}.
              Sorting that list re-orders those rows, not the band.
            </p>
          </div>
        )}
      </header>

      <table className="w-full text-sm">
        <caption className="sr-only">
          File size bands for this subtree, largest band first. Expand a band to list its largest
          files.
        </caption>
        <thead>
          {/* The nested table's own header is deliberately not sticky: two
            * sticky headers in one scrollport would stack on top of each
            * other. */}
          <tr className="text-xs text-muted-foreground">
            {/* The rule lives on the cells, not on the `<tr>`: a collapsed
              * border belongs to the row box, which does not travel with a
              * sticky cell, so a row-level border scrolls out from under its
              * own header. */}
            <th scope="col" className={cn(BAND_HEAD, "text-left")}>
              Band
            </th>
            <th scope="col" className={cn(BAND_HEAD, "text-right")}>
              Files
            </th>
            <th scope="col" className={cn(BAND_HEAD, "text-right")}>
              Allocated
            </th>
            <th scope="col" className={cn(BAND_HEAD, "pl-3 text-left")}>
              Share
            </th>
            <th scope="col" className={cn(BAND_HEAD, "text-right")}>
              Logical
            </th>
          </tr>
        </thead>
        <tbody>
          {ordered.map((row) => {
            const share =
              subtreeAllocated !== null && subtreeAllocated > 0 ? row.allocated / subtreeAllocated : 0;
            const empty = row.files === 0;
            const open = expanded === row.band;
            const key = bandKey(row.band);
            return (
              <Fragment key={row.band}>
              <tr
                data-rds-row={key}
                // Roving tab stop: exactly one row in the whole view is in the
                // tab order, and the arrows move it. 250 file rows each taking
                // a Tab would make the keyboard unusable, which is the failure
                // this replaces.
                tabIndex={activeKey === key ? 0 : -1}
                aria-expanded={empty ? undefined : open}
                aria-controls={open ? `rds-band-${row.band}` : undefined}
                onFocus={() => handleRowFocus(key, null)}
                onKeyDown={(event) => handleBandKeyDown(event, row.band, !empty)}
                className={cn(
                  "border-b border-border/30 outline-none",
                  // The ring is focus. Selection is a fill, and only file rows
                  // can have one — the two are never the same pixel.
                  "focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring",
                  empty ? "text-muted-foreground/50" : "cursor-pointer hover:bg-accent/40",
                  open && "bg-accent/30",
                )}
                onClick={empty ? undefined : () => toggleBand(row.band)}
              >
                <th scope="row" className="py-2 text-left font-normal">
                  <div className="flex items-center gap-1">
                    {!empty && (
                      <ChevronRight
                        aria-hidden
                        className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")}
                      />
                    )}
                    <span className={cn("font-medium", empty && "pl-4.5")}>{edgeLabel(row)}</span>
                  </div>
                  {/* The conversion, always present — never inferred. */}
                  <div className="pl-4.5 text-xs text-muted-foreground">{conversionLabel(row)}</div>
                </th>
                <td className="rds-numeric py-2 text-right tabular-nums">
                  {empty ? "—" : formatCount(row.files)}
                </td>
                <td className="rds-numeric py-2 text-right tabular-nums">
                  {empty ? "—" : formatSI(row.allocated)}
                </td>
                <td className="py-2 pl-3">
                  <div className="flex items-center gap-2">
                    <div className="h-1.5 w-24 overflow-hidden rounded-full bg-muted">
                      <div
                        className="h-full rounded-full bg-brand"
                        // Scaled against the heaviest band, not the subtree, so
                        // a single dominant band does not flatten every other
                        // bar to nothing.
                        style={{ width: widest > 0 ? `${(row.allocated / widest) * 100}%` : "0%" }}
                      />
                    </div>
                    <span className="rds-numeric text-xs text-muted-foreground tabular-nums">
                      {subtreeAllocated === null || empty ? "" : `${(share * 100).toFixed(1)}%`}
                    </span>
                  </div>
                </td>
                <td className="rds-numeric py-2 text-right tabular-nums text-muted-foreground">
                  {empty ? "—" : formatSI(row.logical)}
                </td>
              </tr>
              {open && (
                <tr className="border-b border-border/30">
                  <td colSpan={5} className="p-0">
                    <BandBreakdown
                      id={`rds-band-${row.band}`}
                      band={row.band}
                      rows={sorted}
                      isLoading={entries.isLoading}
                      error={entries.error}
                      total={row.files}
                      bandAllocated={row.allocated}
                      sort={entrySort}
                      onSortChange={setEntrySort}
                      activeKey={activeKey}
                      selection={selection}
                      selectable={onSelect !== undefined}
                      onSelect={onSelect}
                      onRowFocus={handleRowFocus}
                      onRowKeyDown={handleFileKeyDown}
                      onHover={onHover}
                      onReveal={onReveal}
                      onTrash={onTrash}
                      trashEnabled={trashEnabled}
                    />
                  </td>
                </tr>
              )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

interface BandBreakdownProps {
  id: string;
  band: number;
  rows: readonly SizeBandEntryView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  total: number;
  bandAllocated: number;
  sort: EntrySort;
  onSortChange: (sort: EntrySort) => void;
  activeKey: string | null;
  selection: ReadonlySet<number>;
  selectable: boolean;
  onSelect?: (node: number, mode: "replace" | "toggle" | "add") => void;
  onRowFocus: (key: string, node: number | null) => void;
  onRowKeyDown: (event: React.KeyboardEvent, node: number, band: number) => void;
  onHover?: (node: number | null) => void;
  onReveal?: (node: number) => void;
  onTrash?: (node: number) => void;
  trashEnabled: boolean;
}

/**
 * The heaviest files in one band.
 *
 * Explicitly a leaderboard. The smallest band on a boot volume holds ten
 * million files, so the caption says how many there are and the list shows the
 * top few hundred — the alternative is not a longer list, it is a hang.
 *
 * Columns mirror `TreeTable`'s order (name, logical, allocated, share,
 * category, modified) so the two routes are read the same way, and both size
 * columns are present for the same reason the tree shows both: on APFS a
 * sparse, compressed, or cloned file's logical and allocated bytes disagree,
 * and reconciling them silently would invent a number.
 *
 * `aria-rowcount` is **not** set to the band's true file count, which is where
 * this departs from `<DataTable>`. There the server total is honest because
 * paging can actually reach those rows; here it never can — the list is capped
 * by design — so claiming ten million rows would promise navigation that does
 * not exist. The caption carries the true total instead.
 */
function BandBreakdown({
  id,
  band,
  rows,
  isLoading,
  error,
  total,
  bandAllocated,
  sort,
  onSortChange,
  activeKey,
  selection,
  selectable,
  onSelect,
  onRowFocus,
  onRowKeyDown,
  onHover,
  onReveal,
  onTrash,
  trashEnabled,
}: BandBreakdownProps) {
  // One context-menu root for the whole list, with the right-clicked row in
  // state — the rule `<DataTable>` states: "One Radix root per row would mount
  // thousands of portals." This list is 250 rows, so it was 250 roots.
  //
  // It stays reachable from the keyboard because the browser fires
  // `contextmenu` at the *focused* element for ⇧F10 and the Menu key, so the
  // row under the cursor sets `contextRow` exactly as a right-click does.
  const [contextRow, setContextRow] = useState<SizeBandEntryView | null>(null);

  if (error !== null) {
    return (
      <div role="alert" className="px-4 py-3 text-xs text-pressure-critical">
        {error.message}
      </div>
    );
  }
  if (isLoading || rows === undefined) {
    return <div className="px-4 py-3 text-xs text-muted-foreground">Finding the largest…</div>;
  }
  if (rows.length === 0) {
    return <div className="px-4 py-3 text-xs text-muted-foreground">No files in this band.</div>;
  }

  // The bar is scaled to the heaviest file *listed*, while the number beside it
  // is the share of the whole band. They have different denominators on
  // purpose: scaling the bar to the band total would draw 250 invisible bars,
  // because one file out of ten million is a rounding error of the band. So the
  // bar encodes "how these compare to each other" and the number states the
  // share — the number is the fact, the bar is the encoding.
  const heaviest = rows.reduce((most, entry) => Math.max(most, entry.allocated), 0);

  const handleSort = (key: EntrySortKey) => {
    onSortChange(
      sort.key === key
        ? { key, direction: sort.direction === "asc" ? "desc" : "asc" }
        : { key, direction: FIRST_DIRECTION[key] },
    );
  };

  return (
    <div id={id} className="flex flex-col gap-1 bg-background/60 px-4 py-3">
      <ContextMenu onOpenChange={(open) => !open && setContextRow(null)}>
        <table className="w-full table-fixed text-xs">
          <caption className="pb-1 text-left text-xs text-muted-foreground">
            {captionText(rows.length, total, sort)}
          </caption>
          <thead>
            <tr className="border-b border-border/40 text-[11px] text-muted-foreground">
              <SortHeader label="Path" column="path" sort={sort} onSort={handleSort} />
              <SortHeader label="Size" column="logical" sort={sort} onSort={handleSort} align="right" className="w-20" />
              <SortHeader label="Alloc" column="allocated" sort={sort} onSort={handleSort} align="right" className="w-20" />
              {/* Not sortable: it is `allocated` divided by a constant, so it
                * would be the Alloc column under another name. `aria-sort` is
                * omitted rather than set to "none", which is what tells a
                * screen reader the column cannot be sorted at all. */}
              <th scope="col" className="w-32 py-1 pl-3 text-left font-normal">
                % of band
              </th>
              <SortHeader label="Category" column="category" sort={sort} onSort={handleSort} className="w-32 pl-3" />
              <SortHeader label="Modified" column="mtime" sort={sort} onSort={handleSort} align="right" className="w-24" />
            </tr>
          </thead>
          {/* The trigger is the `<tbody>`, not the whole table: right-clicking
            * the caption or a column header would otherwise open a menu for
            * whichever row happened to be right-clicked last, or an empty one. */}
          <ContextMenuTrigger asChild>
            <tbody>
              {rows.map((entry) => {
                const key = fileKey(entry.node);
                const selected = selection.has(entry.node);
                return (
                  <tr
                    key={entry.node}
                    data-rds-row={key}
                    tabIndex={activeKey === key ? 0 : -1}
                    data-state={selected ? "selected" : undefined}
                    aria-selected={selectable ? selected : undefined}
                    onFocus={() => onRowFocus(key, entry.node)}
                    onKeyDown={(event) => onRowKeyDown(event, entry.node, band)}
                    onClick={(event) =>
                      onSelect?.(
                        entry.node,
                        event.metaKey ? "toggle" : event.shiftKey ? "add" : "replace",
                      )
                    }
                    onPointerEnter={() => onHover?.(entry.node)}
                    onContextMenu={() => setContextRow(entry)}
                    className={cn(
                      "cursor-default border-b border-border/20 outline-none last:border-0",
                      "focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring",
                      "hover:bg-accent/40 data-[state=selected]:bg-accent",
                    )}
                  >
                    <td className="py-1 pr-3">
                      {/* `table-fixed` bounds the cell, so the truncation has
                        * something to truncate against; the full path is the
                        * title, since a middle-elided path is not copyable. */}
                      <div className="truncate font-mono text-[11px]" title={entry.path}>
                        {entry.path}
                      </div>
                    </td>
                    <td className="rds-numeric py-1 text-right tabular-nums text-muted-foreground">
                      {formatSI(entry.logical)}
                    </td>
                    <td className="rds-numeric py-1 text-right tabular-nums">
                      {formatSI(entry.allocated)}
                    </td>
                    <td className="py-1 pl-3">
                      <div className="flex items-center gap-2">
                        <div className="h-1.5 min-w-8 flex-1 overflow-hidden rounded-full bg-foreground/8">
                          <div
                            aria-hidden
                            className="h-full rounded-full"
                            style={{
                              width: heaviest > 0 ? `${(entry.allocated / heaviest) * 100}%` : "0%",
                              // Same `--cat-*` variable the treemap tile and the
                              // tree's %Bar use, so the palette stays learnable
                              // across all three views. Resolved by CSS rather
                              // than JS, so light/dark needs no re-render.
                              backgroundColor: categoryColorVar(entry.category),
                            }}
                          />
                        </div>
                        <span className="rds-numeric w-12 shrink-0 text-right tabular-nums text-muted-foreground">
                          {shareLabel(entry.allocated, bandAllocated)}
                        </span>
                      </div>
                    </td>
                    <td className="overflow-hidden py-1 pl-3">
                      <CategoryChip compact category={entry.category} className="max-w-full" />
                    </td>
                    <td
                      className="rds-numeric py-1 text-right tabular-nums text-muted-foreground"
                      // The column is a date because a time on every row is
                      // noise; the exact stamp is one hover away.
                      title={formatMtime(entry.mtime, true)}
                    >
                      {formatMtime(entry.mtime)}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </ContextMenuTrigger>
        </table>
        {/* The same three verbs the tree and the canvas offer, on the same
          * handlers. A file listed here is a file, and a list you cannot act
          * on is a report rather than a tool. */}
        <ContextMenuContent>
          {contextRow !== null && (
            <>
              <ContextMenuItem
                disabled={onReveal === undefined}
                onSelect={() => onReveal?.(contextRow.node)}
              >
                <Eye aria-hidden />
                Reveal in Finder
              </ContextMenuItem>
              <ContextMenuItem onSelect={() => void navigator.clipboard.writeText(contextRow.path)}>
                <Copy aria-hidden />
                Copy Path
              </ContextMenuItem>
              <ContextMenuSeparator />
              <ContextMenuItem
                variant="destructive"
                disabled={onTrash === undefined || !trashEnabled}
                onSelect={() => onTrash?.(contextRow.node)}
              >
                {trashEnabled ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
                {trashEnabled ? "Move to Trash…" : "Move to Trash… (deletion off)"}
              </ContextMenuItem>
            </>
          )}
        </ContextMenuContent>
      </ContextMenu>
    </div>
  );
}

/**
 * A sortable column header.
 *
 * The clickable thing is a real `<button>` inside the `<th>`, not a click
 * handler on the `<th>` itself: a bare `onClick` on a header cell is invisible
 * to the keyboard, and this table's whole point is to be operable without a
 * mouse. `aria-sort` stays on the `<th>`, which is where the attribute belongs.
 *
 * There is no third "unsorted" state. `<DataTable>` sets
 * `enableSortingRemoval: false` for the same reason: a list has an order
 * whether or not the user picked it, so "no sort" is not a state the table can
 * actually be in.
 */
function SortHeader({
  label,
  column,
  sort,
  onSort,
  align = "left",
  className,
}: {
  label: string;
  column: EntrySortKey;
  sort: EntrySort;
  onSort: (key: EntrySortKey) => void;
  align?: "left" | "right";
  className?: string;
}) {
  const active = sort.key === column;
  const direction = active ? (sort.direction === "asc" ? "ascending" : "descending") : "none";
  return (
    <th scope="col" aria-sort={direction} className={cn("py-1 font-normal", className)}>
      <button
        type="button"
        onClick={() => onSort(column)}
        className={cn(
          "flex w-full items-center gap-0.5 rounded hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
          align === "right" ? "justify-end" : "justify-start",
        )}
      >
        <span className="truncate">{label}</span>
        {active &&
          (sort.direction === "asc" ? (
            <ChevronUp aria-hidden className="size-3 shrink-0" />
          ) : (
            <ChevronDown aria-hidden className="size-3 shrink-0" />
          ))}
      </button>
    </th>
  );
}
