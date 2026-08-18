/**
 * The hierarchy table — the `<DataTable>`'s first and heaviest call site.
 *
 * Column set from the docs/05 mock: `Name | Size | Alloc | % | ▁▃█`, with the
 * category chip carrying the palette into the table so the treemap's colours
 * are learnable.
 *
 * Things that look like details but are contract:
 *
 * - **Sorting is a request, not a transform.** The `SortingState` below is
 *   translated to a `Sort` and sent to Rust; nothing here reorders rows.
 * - **The virtual `<Files>` group has no expander, no reveal, and no trash.**
 *   It is `VIRTUAL_GROUP_BIT | dir_index`, not a directory entry — there is no
 *   path to act on. It is rendered in italics and named `‹files›` so it cannot
 *   be mistaken for a real child.
 * - **Incomplete subtrees are marked.** Any of `UNREADABLE | EXCLUDED |
 *   INCOMPLETE | AGGREGATED` means the totals are a floor, so the row gets a
 *   lock/warning affordance and its `%Bar` is hatched.
 * - **Two size columns, never one.** Logical and allocated are shown side by
 *   side and never summed or reconciled.
 */

import type { ColumnDef, SortingState } from "@tanstack/react-table";
import { ChevronDown, ChevronRight, Eye, File, Folder, Link2, Lock, Trash2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef } from "react";

import { DataTable } from "@/components/DataTable";
import { CategoryChip } from "@/components/cells/CategoryChip";
import { PercentBar } from "@/components/cells/PercentBar";
import { ContextMenuItem, ContextMenuSeparator } from "@/components/ui/context-menu";
import { formatMtime, formatSI } from "@/lib/format";
import { toBackendSort } from "@/lib/sort";
import { useTreeRows, type FlatRow } from "@/lib/useTreeRows";
import { cn } from "@/lib/utils";
import { FLAGS, hasFlag, INCOMPLETE_SUBTREE } from "@/lib/wire";
import type { SizeMetric } from "@/state/store";

export interface TreeTableProps {
  generation: number;
  root: number;
  rootLogical: number;
  rootAllocated: number;
  sizeMetric: SizeMetric;
  sorting: SortingState;
  onSortingChange: (sorting: SortingState) => void;
  selection: ReadonlySet<number>;
  focused: number | null;
  onSelect: (node: number, mode: "replace" | "toggle" | "add") => void;
  /** Shift-click range and select-all. Both are one gesture, so one call. */
  onSelectMany?: (nodes: readonly number[], mode: "replace" | "add") => void;
  /**
   * Combined size of the selected rows, in the metric on screen.
   *
   * Reported from here because the table is what holds the rows — the shell
   * has ids and no sizes. Only loaded rows can contribute, which is correct:
   * a selection can only be made from rows the user can see.
   */
  onSelectionBytes?: (bytes: number) => void;
  /** Double-click drills into a directory: pushes the navigation stack. */
  onDrillDown: (node: number) => void;
  onHover?: (node: number | null) => void;
  onReveal?: (node: number) => void;
  onTrash?: (node: number) => void;
  /**
   * Whether the row menu offers Trash. Defaults to `false` — the shell owns the
   * arming switch, and a table that decided this for itself would be a second
   * place deletion could turn on.
   */
  trashEnabled?: boolean;
}

export function TreeTable({
  generation,
  root,
  rootLogical,
  rootAllocated,
  sizeMetric,
  sorting,
  onSortingChange,
  selection,
  focused,
  onSelect,
  onSelectMany,
  onSelectionBytes,
  onDrillDown,
  onHover,
  onReveal,
  onTrash,
  trashEnabled = false,
}: TreeTableProps) {
  const sort = useMemo(() => toBackendSort(sorting), [sorting]);
  const tree = useTreeRows({ generation, root, sort, rootLogical, rootAllocated });

  const columns = useMemo<ColumnDef<FlatRow, unknown>[]>(
    () => [
      {
        id: "name",
        header: "Name",
        size: 360,
        enableSorting: true,
        cell: ({ row }) => <NameCell entry={row.original} onToggle={tree.toggle} />,
      },
      {
        id: "logical",
        header: "Size",
        size: 100,
        enableSorting: true,
        cell: ({ row }) => (
          <span className="rds-numeric w-full text-right text-xs">
            {formatSI(row.original.row.logical)}
          </span>
        ),
      },
      {
        id: "allocated",
        header: "Alloc",
        size: 100,
        enableSorting: true,
        cell: ({ row }) => (
          <span className="rds-numeric w-full text-right text-xs">
            {formatSI(row.original.row.allocated)}
          </span>
        ),
      },
      {
        id: "share",
        header: "% of parent",
        size: 170,
        enableSorting: false,
        cell: ({ row }) => {
          const entry = row.original;
          const value = sizeMetric === "logical" ? entry.row.logical : entry.row.allocated;
          const total = sizeMetric === "logical" ? entry.parentLogical : entry.parentAllocated;
          return (
            <PercentBar
              value={value}
              total={total}
              category={entry.row.category}
              partial={hasFlag(entry.row.flags, INCOMPLETE_SUBTREE)}
              className="w-full"
            />
          );
        },
      },
      {
        id: "category",
        header: "Category",
        size: 150,
        enableSorting: true,
        cell: ({ row }) => <CategoryChip compact category={row.original.row.category} />,
      },
      {
        id: "mtime",
        header: "Modified",
        size: 110,
        enableSorting: true,
        cell: ({ row }) => (
          <span className="rds-numeric text-xs text-muted-foreground">
            {formatMtime(row.original.row.mtime)}
          </span>
        ),
      },
    ],
    [sizeMetric, tree.toggle],
  );

  /*
   * The anchor a shift-click extends FROM.
   *
   * Held in a ref rather than state because it must not cause a render: it
   * changes on every plain click, and re-rendering a 500-row virtualized table
   * to remember where a range might start later is pure cost.
   */
  const anchorRef = useRef<number | null>(null);

  const handleClick = useCallback(
    (entry: FlatRow, event: React.MouseEvent) => {
      const node = entry.row.node;

      // Shift extends a RANGE over the rows as they are currently flattened —
      // which is what the user sees, and therefore what they mean. Extending
      // over node ids or over the unexpanded tree would select rows that are
      // not on screen, which is the classic way a bulk action eats something
      // the user never saw.
      if (event.shiftKey && onSelectMany !== undefined && anchorRef.current !== null) {
        const rows = tree.rows;
        const from = rows.findIndex((candidate) => candidate.row.node === anchorRef.current);
        const to = rows.findIndex((candidate) => candidate.row.node === node);
        if (from >= 0 && to >= 0) {
          const [lo, hi] = from <= to ? [from, to] : [to, from];
          const span = rows.slice(lo, hi + 1).map((candidate) => candidate.row.node);
          // The anchor stays put, so dragging the shift-click further extends
          // from the same origin rather than walking it along.
          onSelectMany(span, event.metaKey ? "add" : "replace");
          return;
        }
      }

      anchorRef.current = node;
      onSelect(node, event.metaKey ? "toggle" : "replace");
    },
    [onSelect, onSelectMany, tree.rows],
  );

  useEffect(() => {
    if (onSelectionBytes === undefined) return;
    let bytes = 0;
    for (const entry of tree.rows) {
      if (!selection.has(entry.row.node)) continue;
      bytes += sizeMetric === "logical" ? entry.row.logical : entry.row.allocated;
    }
    onSelectionBytes(bytes);
  }, [selection, tree.rows, sizeMetric, onSelectionBytes]);

  /*
   * Select-all over the rows currently flattened, not the whole subtree.
   *
   * "All" has to mean what is on screen. A directory can hold a million
   * entries and the table holds a page of them, so selecting the true subtree
   * would hand a bulk action a set the user has never seen and cannot review —
   * exactly the shape of an irreversible mistake.
   */
  useEffect(() => {
    if (onSelectMany === undefined) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.metaKey || event.key !== "a") return;
      const target = event.target as HTMLElement | null;
      // Never steal ⌘A from a text field.
      if (target !== null && /^(INPUT|TEXTAREA)$/.test(target.tagName)) return;
      event.preventDefault();
      onSelectMany(
        tree.rows.filter((entry) => !entry.row.isVirtualGroup).map((entry) => entry.row.node),
        "replace",
      );
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onSelectMany, tree.rows]);

  // Which directory the next cursor page belongs to.
  //
  // Not always the navigation root: if the bottom of the flattened list is
  // inside an expanded subdirectory, that subdirectory is the one with rows
  // left to fetch. Paging the root instead would append 200 rows the user
  // cannot see while the list they are actually looking at stays truncated.
  const lastRow = tree.rows.at(-1);
  const pagingNode =
    lastRow !== undefined && tree.hasMore(lastRow.parent)
      ? lastRow.parent
      : tree.hasMore(root)
        ? root
        : null;

  const handleDoubleClick = useCallback(
    (entry: FlatRow) => {
      // A virtual group is not a place; drilling into it would be a request the
      // backend must reject.
      if (entry.row.isVirtualGroup) return;
      if (entry.row.kind === "directory") onDrillDown(entry.row.node);
    },
    [onDrillDown],
  );

  return (
    <DataTable<FlatRow>
      label="Directory tree"
      data={tree.rows}
      columns={columns}
      getRowId={(entry) => String(entry.row.node)}
      sorting={sorting}
      onSortingChange={onSortingChange}
      rowCount={tree.rootTotal}
      isLoading={tree.isLoading}
      error={tree.error}
      emptyMessage="This directory has no retained entries."
      hasNextPage={pagingNode !== null}
      fetchNextPage={() => {
        if (pagingNode !== null) tree.loadMore(pagingNode);
      }}
      isRowSelected={(entry) => selection.has(entry.row.node)}
      isRowFocused={(entry) => focused === entry.row.node}
      onRowClick={handleClick}
      onRowDoubleClick={handleDoubleClick}
      onRowPointerEnter={(entry) => onHover?.(entry.row.node)}
      onRowPointerLeave={() => onHover?.(null)}
      renderContextMenu={(entry) => (
        <>
          <ContextMenuItem
            disabled={entry.row.isVirtualGroup || onReveal === undefined}
            onSelect={() => onReveal?.(entry.row.node)}
          >
            <Eye aria-hidden />
            Reveal in Finder
          </ContextMenuItem>
          <ContextMenuItem
            disabled={entry.row.kind !== "directory" || entry.row.isVirtualGroup}
            onSelect={() => onDrillDown(entry.row.node)}
          >
            <Folder aria-hidden />
            Zoom to this subtree
          </ContextMenuItem>
          <ContextMenuSeparator />
          <ContextMenuItem
            variant="destructive"
            disabled={entry.row.isVirtualGroup || onTrash === undefined || !trashEnabled}
            onSelect={() => onTrash?.(entry.row.node)}
          >
            {trashEnabled ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
            {trashEnabled ? "Move to Trash…" : "Move to Trash… (deletion off)"}
          </ContextMenuItem>
        </>
      )}
    />
  );
}

function NameCell({ entry, onToggle }: { entry: FlatRow; onToggle: (node: number) => void }) {
  const { row, depth, isExpanded } = entry;
  // The group IS expandable now. It used to be excluded here on the grounds
  // that it had no children — which was true only because the backend refused
  // to enumerate them, so the row holding most of a directory's bytes offered
  // no way in. It reports its file count now, and `children` on it returns
  // exactly those files.
  const expandable = (row.kind === "directory" || row.isVirtualGroup) && row.children > 0;
  const incomplete = hasFlag(row.flags, INCOMPLETE_SUBTREE);

  return (
    <div
      className="flex min-w-0 items-center gap-1"
      style={{ paddingLeft: `${Math.min(depth, 24) * 12}px` }}
    >
      {expandable ? (
        <button
          type="button"
          aria-label={isExpanded ? `Collapse ${row.name}` : `Expand ${row.name}`}
          aria-expanded={isExpanded}
          onClick={(event) => {
            event.stopPropagation();
            onToggle(row.node);
          }}
          className="flex size-4 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          {isExpanded ? (
            <ChevronDown aria-hidden className="size-3" />
          ) : (
            <ChevronRight aria-hidden className="size-3" />
          )}
        </button>
      ) : (
        <span aria-hidden className="size-4 shrink-0" />
      )}

      <RowIcon entry={entry} />

      <span
        className={cn(
          "truncate text-xs",
          row.isVirtualGroup && "italic text-muted-foreground",
          hasFlag(row.flags, FLAGS.HARD_LINK_REPEAT) && "text-muted-foreground",
        )}
        title={row.name}
      >
        {row.isVirtualGroup ? "‹files›" : row.name}
      </span>

      {incomplete && (
        <Lock
          aria-label="Totals for this subtree are a floor: part of it was unreadable, excluded, or aggregated."
          className="size-3 shrink-0 text-pressure-warn"
        />
      )}
      {hasFlag(row.flags, FLAGS.HARD_LINK_REPEAT) && (
        <span
          title="Another link to a file already counted. It contributes zero bytes to the totals."
          className="shrink-0 text-[10px] text-muted-foreground"
        >
          ↩
        </span>
      )}
    </div>
  );
}

function RowIcon({ entry }: { entry: FlatRow }) {
  const { row } = entry;
  if (row.isVirtualGroup) return <File aria-hidden className="size-3 shrink-0 text-muted-foreground/70" />;
  if (row.kind === "directory")
    return <Folder aria-hidden className="size-3 shrink-0 text-muted-foreground" />;
  if (row.kind === "symlink")
    return <Link2 aria-hidden className="size-3 shrink-0 text-muted-foreground" />;
  return <File aria-hidden className="size-3 shrink-0 text-muted-foreground/70" />;
}
