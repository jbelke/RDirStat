/**
 * Types — what *kind* of content occupies this subtree.
 *
 * The sibling of `SizeBands`. Both answer "what is taking up the room" over the
 * same subtree in one backend traversal; Sizes partitions the files by how big
 * each one is, this partitions them by what each one is. Read together they must
 * agree, so the two are built the same way on purpose.
 *
 * ## What is counted
 *
 * Files, and only files.
 *
 * - **Not directories.** A directory's size is its subtree total, so counting a
 *   directory and its contents would count the same bytes twice and the rows
 *   would not sum to the subtree. These rows partition the files.
 * - **Not symlinks**, which is the part worth stating out loud, because the
 *   taxonomy does define a `Symlink` category that this view can therefore never
 *   show. `bands` counts regular files and nothing else, the two routes are one
 *   click apart, and a grand total that silently differs between them by the
 *   size of the symlink population is a discrepancy the user cannot explain.
 *   `crates/rdirstat-core/src/by_category.rs` records the same trade; if it is
 *   ever revisited, it is revisited in both places on the same day.
 *
 * ## Why there is no row for every category
 *
 * `SizeBands` renders all six bands including the empty ones, because the band
 * edges are a closed set defined in one const array and "there is nothing over
 * 50 GiB here" is an answer. Categories are not that: the table lives in
 * `rdirstat-classify`, `rdirstat-core` cannot enumerate it, and the wire type is
 * a `u8`. The backend therefore returns one row per category that actually has
 * files, and this view renders exactly what it is given rather than padding the
 * table out to 256 mostly-nonexistent rows.
 *
 * ## Why the expanded category is a prop and not local state
 *
 * Expanding a category costs an `O(subtree)` walk in Rust, so exactly one
 * category's file list may be in flight at a time and the query key *is* the
 * expansion state. Owning the state here and the query in the shell would make
 * two sources of truth for one thing, so both live in the caller and this
 * component is controlled. Everything else it needs arrives as plain data:
 * nothing here calls `invoke`.
 *
 * Sizes are decimal SI throughout, matching Finder (docs/05-UI.md). Allocated
 * and logical are shown side by side and never summed — on APFS they legitimately
 * disagree for sparse, compressed, and cloned files.
 */

import { ArrowDown, ArrowUp, ChevronRight, Copy, Eye, Info, Lock, Trash2 } from "lucide-react";
import { Fragment, useMemo, useState } from "react";

import { CategorySwatch } from "@/components/cells/CategoryChip";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { categoryColorVar, categoryOf } from "@/lib/categories";
import { formatCount, formatMtime, formatSI } from "@/lib/format";
import { cn } from "@/lib/utils";

/**
 * One row of the Types report — the view mirror of `by_category::CategoryRow`.
 *
 * Declared here rather than imported from `@/lib/ipc` because the command does
 * not exist yet. When it does, this interface is what the `ipc.ts` view type
 * must be structurally compatible with; the caller can pass its rows straight
 * through with no adapter.
 */
export interface CategoryRowView {
  /** `CategoryId` as sent by Rust. Resolved to a label and colour locally. */
  readonly category: number;
  /** Files of this category, including ones that contribute zero bytes. */
  readonly files: number;
  readonly logical: number;
  readonly allocated: number;
}

/** One file inside a category. The breakdown is a leaderboard, not an enumeration. */
export interface CategoryEntryView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly logical: number;
  readonly mtime: number;
}

/**
 * Which column the table is ordered by. `category` sorts by the *label* the user
 * can see, not by the numeric id, because the id order is a classifier
 * implementation detail and sorting by an invisible key reads as no sort at all.
 */
type TypesSortKey = "category" | "files" | "allocated" | "logical";

interface TypesSort {
  readonly key: TypesSortKey;
  readonly descending: boolean;
}

/** Largest first: the question this view answers is "what is big". */
const DEFAULT_SORT: TypesSort = { key: "allocated", descending: true };

/** Matches the backend's own ceiling on one category's leaderboard. */
const DEFAULT_ENTRY_LIMIT = 250;

export interface TypesRouteProps {
  /** Per-category totals for the subtree, in any order — this view sorts. */
  rows: readonly CategoryRowView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  /** Total allocated bytes of the subtree, for the share column. */
  subtreeAllocated: number | null;
  /**
   * The expanded category, or `null`. Controlled, because the caller owns the
   * query this key drives — see the module docs.
   */
  expanded: number | null;
  onExpandedChange: (category: number | null) => void;
  /** The largest files of `expanded`. Ignored while `expanded` is `null`. */
  entries: readonly CategoryEntryView[] | undefined;
  entriesLoading: boolean;
  entriesError: Error | null;
  /** The ceiling the caller asked the backend for, for the "capped at" footer. */
  entryLimit?: number;
  /** Reveal one file in Finder. Same handler the tree and the canvas use. */
  onReveal?: (node: number) => void;
  /** Move one file to the Trash. Refused unless deletion is armed. */
  onTrash?: (node: number) => void;
  /** Whether deletion is armed; drives the Trash item's wording, not just its state. */
  trashEnabled?: boolean;
  className?: string;
}

export function TypesRoute({
  rows,
  isLoading,
  error,
  subtreeAllocated,
  expanded,
  onExpandedChange,
  entries,
  entriesLoading,
  entriesError,
  entryLimit = DEFAULT_ENTRY_LIMIT,
  onReveal,
  onTrash,
  trashEnabled = false,
  className,
}: TypesRouteProps) {
  const [sort, setSort] = useState<TypesSort>(DEFAULT_SORT);
  const [notesOpen, setNotesOpen] = useState(false);

  // Sorting is a pure function of two small pieces of state and the table is at
  // most a couple of dozen rows, so this is memoised for referential stability
  // rather than for speed.
  const ordered = useMemo(() => sortRows(rows ?? [], sort), [rows, sort]);

  if (error !== null) {
    return (
      <div className={cn("p-6 text-sm text-pressure-critical", className)}>
        Types could not be computed: {error.message}
      </div>
    );
  }
  if (isLoading || rows === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Counting…</div>;
  }

  const totalFiles = ordered.reduce((sum, row) => sum + row.files, 0);
  // Scaled against the heaviest category, not the subtree, so one dominant
  // category does not flatten every other bar to nothing.
  const widest = ordered.reduce((most, row) => Math.max(most, row.allocated), 0);
  // The rows are the whole answer, so their sum is a usable denominator when the
  // caller has no subtree total yet. It is not the same number: the subtree
  // total also carries symlink bytes, which this report does not count.
  const denominator = subtreeAllocated !== null && subtreeAllocated > 0
    ? subtreeAllocated
    : ordered.reduce((sum, row) => sum + row.allocated, 0);

  const toggleSort = (key: TypesSortKey) => {
    setSort((current) =>
      current.key === key
        ? { key, descending: !current.descending }
        // A new column starts in the direction that answers the question it was
        // clicked to ask: biggest first for a quantity, A–Z for a name.
        : { key, descending: key !== "category" },
    );
  };

  return (
    <div className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}>
      <header className="relative flex shrink-0 items-center gap-2">
        <h2 className="text-sm font-medium">Files by type</h2>
        <span className="rds-numeric text-xs text-muted-foreground">
          {formatCount(totalFiles)} files in {ordered.length}{" "}
          {ordered.length === 1 ? "category" : "categories"}
        </span>
        {/* The explanation is real and occasionally necessary, but it is the
          * same paragraph every time and it would be the tallest thing on the
          * route. One icon, opened on demand. */}
        <button
          type="button"
          aria-expanded={notesOpen}
          onClick={() => setNotesOpen((open) => !open)}
          title="How these categories are counted"
          className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Info aria-hidden className="size-3.5" />
          <span className="sr-only">How these categories are counted</span>
        </button>
        {notesOpen && (
          <div className="absolute left-0 top-full z-50 mt-1 flex w-[30rem] max-w-[80vw] flex-col gap-2 rounded-lg border border-border bg-popover p-3 text-xs text-muted-foreground shadow-xl">
            <p>
              Categories come from the file&apos;s name and mode, not from its contents — nothing is
              opened to classify it. Anything the table does not recognise is{" "}
              <span className="text-foreground">Uncategorized</span>.
            </p>
            <p>
              Only regular files are counted. Directories are excluded so the rows sum to the
              subtree exactly once; symlinks are excluded so this total matches the one on the Sizes
              route.
            </p>
            <p>
              A category with no files here is absent rather than shown as a zero row. Padding the
              table with every category the app knows about would be inventing rows for things this
              folder does not contain.
            </p>
            <p>
              Allocated and logical are never summed. Allocated is what the filesystem spent,
              logical is what the files claim, and on APFS they disagree for sparse, compressed, and
              cloned files.
            </p>
          </div>
        )}
      </header>

      {ordered.length === 0 ? (
        <div className="text-sm text-muted-foreground">No files in this subtree.</div>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-border/60 text-xs text-muted-foreground">
              <SortableHeader label="Type" sortKey="category" sort={sort} onSort={toggleSort} align="left" />
              <SortableHeader label="Files" sortKey="files" sort={sort} onSort={toggleSort} align="right" />
              <SortableHeader
                label="Allocated"
                sortKey="allocated"
                sort={sort}
                onSort={toggleSort}
                align="right"
              />
              <th scope="col" className="py-1.5 pl-3 text-left font-normal">
                Share
              </th>
              <SortableHeader label="Logical" sortKey="logical" sort={sort} onSort={toggleSort} align="right" />
            </tr>
          </thead>
          <tbody>
            {ordered.map((row) => {
              const entry = categoryOf(row.category);
              const share = denominator > 0 ? row.allocated / denominator : 0;
              const open = expanded === row.category;
              return (
                <Fragment key={row.category}>
                  <tr
                    className={cn(
                      "cursor-pointer border-b border-border/30 hover:bg-accent/40",
                      open && "bg-accent/30",
                    )}
                    onClick={() => onExpandedChange(open ? null : row.category)}
                  >
                    <th scope="row" className="py-2 text-left font-normal">
                      <div className="flex items-center gap-1">
                        <ChevronRight
                          aria-hidden
                          className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")}
                        />
                        {/* The swatch is never the sole carrier of the category:
                          * docs/05 forbids colour-only meaning, so the label is
                          * always beside it. */}
                        <CategorySwatch category={row.category} />
                        <span className="font-medium">{entry.label}</span>
                      </div>
                      <div className="pl-8 text-xs text-muted-foreground">{entry.family}</div>
                    </th>
                    <td className="rds-numeric py-2 text-right tabular-nums">{formatCount(row.files)}</td>
                    <td className="rds-numeric py-2 text-right tabular-nums">{formatSI(row.allocated)}</td>
                    <td className="py-2 pl-3">
                      <div className="flex items-center gap-2">
                        <div className="h-1.5 w-24 overflow-hidden rounded-full bg-muted">
                          <div
                            className="h-full rounded-full"
                            style={{
                              width: widest > 0 ? `${(row.allocated / widest) * 100}%` : "0%",
                              // The same `var(--cat-*)` the treemap tile uses, so
                              // the palette stays learnable from the table and
                              // light/dark needs no JS.
                              backgroundColor: categoryColorVar(row.category),
                            }}
                          />
                        </div>
                        <span className="rds-numeric text-xs text-muted-foreground tabular-nums">
                          {denominator > 0 ? `${(share * 100).toFixed(1)}%` : ""}
                        </span>
                      </div>
                    </td>
                    <td className="rds-numeric py-2 text-right tabular-nums text-muted-foreground">
                      {formatSI(row.logical)}
                    </td>
                  </tr>
                  {open && (
                    <tr className="border-b border-border/30">
                      <td colSpan={5} className="p-0">
                        <CategoryBreakdown
                          rows={entries}
                          isLoading={entriesLoading}
                          error={entriesError}
                          total={row.files}
                          limit={entryLimit}
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
      )}
    </div>
  );
}

/**
 * A column header that sorts.
 *
 * `aria-sort` on the `th` rather than only an arrow glyph: the arrow is the
 * encoding, the attribute is the information, and a table whose order is
 * announced only in pixels is not usable without sight.
 */
function SortableHeader({
  label,
  sortKey,
  sort,
  onSort,
  align,
}: {
  label: string;
  sortKey: TypesSortKey;
  sort: TypesSort;
  onSort: (key: TypesSortKey) => void;
  align: "left" | "right";
}) {
  const active = sort.key === sortKey;
  const Arrow = sort.descending ? ArrowDown : ArrowUp;
  return (
    <th
      scope="col"
      aria-sort={active ? (sort.descending ? "descending" : "ascending") : "none"}
      className={cn("py-1.5 font-normal", align === "right" ? "text-right" : "text-left")}
    >
      <button
        type="button"
        onClick={() => onSort(sortKey)}
        className={cn(
          "inline-flex items-center gap-1 rounded px-1 py-0.5 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
          active && "text-foreground",
        )}
      >
        {align === "right" && <Arrow aria-hidden className={cn("size-3", !active && "invisible")} />}
        {label}
        {align === "left" && <Arrow aria-hidden className={cn("size-3", !active && "invisible")} />}
      </button>
    </th>
  );
}

/**
 * The heaviest files in one category.
 *
 * Explicitly a leaderboard. `Cache` and `Object / Generated` each hold millions
 * of files on a developer's boot volume, so the header says how many there are
 * and the list shows the top few hundred — the alternative is not a longer list,
 * it is a hang.
 */
function CategoryBreakdown({
  rows,
  isLoading,
  error,
  total,
  limit,
  onReveal,
  onTrash,
  trashEnabled,
}: {
  rows: readonly CategoryEntryView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  total: number;
  limit: number;
  onReveal?: (node: number) => void;
  onTrash?: (node: number) => void;
  trashEnabled: boolean;
}) {
  if (error !== null) {
    return <div className="px-4 py-3 text-xs text-pressure-critical">{error.message}</div>;
  }
  if (isLoading || rows === undefined) {
    return <div className="px-4 py-3 text-xs text-muted-foreground">Finding the largest…</div>;
  }
  if (rows.length === 0) {
    return <div className="px-4 py-3 text-xs text-muted-foreground">No files in this category.</div>;
  }

  const truncated = total > rows.length;

  return (
    <div className="flex flex-col gap-1 bg-background/60 px-4 py-3">
      <div className="text-xs text-muted-foreground">
        {truncated
          ? `The ${formatCount(rows.length)} largest of ${formatCount(total)} — ordered by allocated bytes.`
          : `All ${formatCount(rows.length)}, ordered by allocated bytes.`}
      </div>
      <ul className="flex flex-col">
        {rows.map((entry) => (
          <ContextMenu key={entry.node}>
            <ContextMenuTrigger asChild>
              <li className="flex cursor-default items-baseline gap-3 rounded-sm border-b border-border/20 py-1 last:border-0 hover:bg-accent/40">
                <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={entry.path}>
                  {entry.path}
                </span>
                <span className="rds-numeric shrink-0 text-xs tabular-nums">
                  {formatSI(entry.allocated)}
                </span>
                <span className="rds-numeric w-24 shrink-0 text-right text-[11px] text-muted-foreground tabular-nums">
                  {formatMtime(entry.mtime)}
                </span>
              </li>
            </ContextMenuTrigger>
            {/* The same three verbs the tree, the canvas, and the size bands
              * offer, on the same handlers. A file listed here is a file, and a
              * list you cannot act on is a report rather than a tool. */}
            <ContextMenuContent>
              <ContextMenuItem disabled={onReveal === undefined} onSelect={() => onReveal?.(entry.node)}>
                <Eye aria-hidden />
                Reveal in Finder
              </ContextMenuItem>
              <ContextMenuItem onSelect={() => void navigator.clipboard.writeText(entry.path)}>
                <Copy aria-hidden />
                Copy Path
              </ContextMenuItem>
              <ContextMenuSeparator />
              <ContextMenuItem
                variant="destructive"
                disabled={onTrash === undefined || !trashEnabled}
                onSelect={() => onTrash?.(entry.node)}
              >
                {trashEnabled ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
                {trashEnabled ? "Move to Trash…" : "Move to Trash… (deletion off)"}
              </ContextMenuItem>
            </ContextMenuContent>
          </ContextMenu>
        ))}
      </ul>
      {truncated && (
        <div className="text-[11px] text-muted-foreground">
          Capped at {formatCount(limit)}; the count above is the true total.
        </div>
      )}
    </div>
  );
}

/**
 * Order the table.
 *
 * Every comparison falls back to the category id, so the order is total and the
 * table cannot reshuffle between renders when two categories tie — which they do
 * constantly on the file-count column.
 */
function sortRows(rows: readonly CategoryRowView[], sort: TypesSort): CategoryRowView[] {
  const direction = sort.descending ? -1 : 1;
  return [...rows].sort((a, b) => {
    let delta = 0;
    if (sort.key === "category") {
      // By the visible label, and case-insensitively, because "Video" sorting
      // before "audio" is the kind of ordering that reads as a bug.
      delta = categoryOf(a.category).label.localeCompare(categoryOf(b.category).label, undefined, {
        sensitivity: "base",
      });
    } else {
      delta = a[sort.key] - b[sort.key];
    }
    return delta === 0 ? a.category - b.category : delta * direction;
  });
}
