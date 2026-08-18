/**
 * The one `<DataTable>`.
 *
 * docs/05-UI.md, "The tables": "One `<DataTable>` built on TanStack Table +
 * TanStack Virtual, reused everywhere. Rust does the sorting and windowing; the
 * table's `manualSorting`/`manualPagination` modes exist for exactly this."
 *
 * Both manual flags are hard-coded rather than exposed as props, because the
 * alternative is not a configuration choice — it is a contract violation.
 * `getSortedRowModel` would need every row in JS to order them, and the IPC
 * contract caps a request at 500 rows precisely so that can never happen. A
 * future call site that wants client sorting is a call site that has already
 * decided to hold the whole tree in memory.
 *
 * Layout notes, because they are load-bearing rather than cosmetic:
 *
 * - The table is `display: grid`, not the default table layout. Absolutely
 *   positioned `<tr>`s inside a normal `<table>` are not laid out by the table
 *   algorithm at all in WebKit; the grid/flex form is what makes virtual rows
 *   and a sticky header coexist. Semantics survive because the elements are
 *   still `table`/`thead`/`tr`/`td`, so the accessibility tree is unchanged.
 * - Row height is fixed and passed to the virtualizer as a constant.
 *   `measureElement` would give per-row heights at the cost of a layout read
 *   per row per frame, which is the wrong trade at this row count.
 * - `aria-rowcount` reports the **server** total, not the number of rendered
 *   rows, and each row carries its true `aria-rowindex`. Telling a screen
 *   reader there are 30 rows when the directory has 400,000 is worse than no
 *   ARIA at all.
 *
 * One context-menu root serves the whole table, with the right-clicked row
 * tracked in state. One Radix root per row would mount thousands of portals.
 */

import {
  flexRender,
  getCoreRowModel,
  useReactTable,
  type ColumnDef,
  type Row,
  type SortingState,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronDown, ChevronUp, Loader } from "lucide-react";
import { useCallback, useRef, useState, type ReactNode } from "react";

import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";

export const DEFAULT_ROW_HEIGHT = 28;

export interface DataTableProps<TData> {
  data: readonly TData[];
  // eslint-disable-next-line @typescript-eslint/no-explicit-any -- ColumnDef's value generic is per-column.
  columns: ColumnDef<TData, any>[];

  /** Stable identity across pages. Use the NodeId, never the array index. */
  getRowId: (row: TData, index: number) => string;

  /**
   * Sort state, owned by the caller and sent to Rust. `manualSorting` means the
   * table never reorders anything itself; this is display state plus the
   * request parameter.
   */
  sorting: SortingState;
  onSortingChange: (sorting: SortingState) => void;

  /** Total rows on the server, for `aria-rowcount` and the footer count. */
  rowCount?: number;

  rowHeight?: number;
  /** Rows rendered beyond the viewport on each side. */
  overscan?: number;

  isLoading?: boolean;
  error?: Error | null;
  emptyMessage?: ReactNode;

  /** Cursor paging. Called when the viewport approaches the last fetched row. */
  hasNextPage?: boolean;
  isFetchingNextPage?: boolean;
  fetchNextPage?: () => void;

  isRowSelected?: (row: TData) => boolean;
  isRowFocused?: (row: TData) => boolean;
  onRowClick?: (row: TData, event: React.MouseEvent) => void;
  onRowDoubleClick?: (row: TData, event: React.MouseEvent) => void;
  onRowPointerEnter?: (row: TData) => void;
  onRowPointerLeave?: () => void;

  /** Menu body for the right-clicked row. Rendered once, not per row. */
  renderContextMenu?: (row: TData) => ReactNode;

  /** Accessible name. Required — a virtualized grid with no name is unusable. */
  label: string;
  className?: string;
}

export function DataTable<TData>({
  data,
  columns,
  getRowId,
  sorting,
  onSortingChange,
  rowCount,
  rowHeight = DEFAULT_ROW_HEIGHT,
  overscan = 12,
  isLoading = false,
  error = null,
  emptyMessage = "Nothing here.",
  hasNextPage = false,
  isFetchingNextPage = false,
  fetchNextPage,
  isRowSelected,
  isRowFocused,
  onRowClick,
  onRowDoubleClick,
  onRowPointerEnter,
  onRowPointerLeave,
  renderContextMenu,
  label,
  className,
}: DataTableProps<TData>) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [contextRow, setContextRow] = useState<TData | null>(null);

  const table = useReactTable({
    data: data as TData[],
    columns,
    getRowId,
    state: { sorting },
    onSortingChange: (updater) => {
      onSortingChange(typeof updater === "function" ? updater(sorting) : updater);
    },
    getCoreRowModel: getCoreRowModel(),
    // Rust owns ordering and windowing. See the header comment.
    manualSorting: true,
    manualPagination: true,
    enableSortingRemoval: false,
  });

  const rows = table.getRowModel().rows;

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan,
  });

  const virtualRows = virtualizer.getVirtualItems();
  const totalSize = virtualizer.getTotalSize();

  // Cursor paging: fire when the last rendered row is within one screen of the
  // end. Doing it on scroll rather than with an IntersectionObserver sentinel
  // avoids a sentinel element inside an absolutely positioned virtual stack.
  const handleScroll = useCallback(() => {
    if (!hasNextPage || isFetchingNextPage || fetchNextPage === undefined) return;
    const element = scrollRef.current;
    if (element === null) return;
    const remaining = element.scrollHeight - element.scrollTop - element.clientHeight;
    if (remaining < element.clientHeight) fetchNextPage();
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);

  const body = (
    <div
      ref={scrollRef}
      onScroll={handleScroll}
      onPointerLeave={onRowPointerLeave}
      className="rds-virtual-viewport h-full w-full"
    >
      <Table
        className="grid"
        aria-label={label}
        aria-rowcount={rowCount ?? rows.length}
        aria-busy={isLoading || undefined}
      >
        <TableHeader className="sticky top-0 z-10 grid">
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id} className="flex w-full" aria-rowindex={1}>
              {headerGroup.headers.map((header) => {
                const canSort = header.column.getCanSort();
                const direction = header.column.getIsSorted();
                return (
                  <TableHead
                    key={header.id}
                    style={{ width: header.getSize() }}
                    className="flex items-center gap-1"
                    // `aria-sort` belongs on the header cell, not on the
                    // control inside it, so it stays here while the click
                    // target moves into a real button below.
                    aria-sort={
                      direction === "asc"
                        ? "ascending"
                        : direction === "desc"
                          ? "descending"
                          : canSort
                            ? "none"
                            : undefined
                    }
                  >
                    {header.isPlaceholder ? null : canSort ? (
                      // A `<th onClick>` announced a sortable column to a
                      // screen reader and then could not be sorted from the
                      // keyboard: no button, no tabindex, no key handler. A
                      // real button is the affordance the ARIA already claimed.
                      <button
                        type="button"
                        onClick={header.column.getToggleSortingHandler()}
                        className="-mx-1 flex cursor-pointer items-center gap-1 rounded px-1 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
                      >
                        {flexRender(header.column.columnDef.header, header.getContext())}
                        {direction === "asc" && <ChevronUp aria-hidden className="size-3" />}
                        {direction === "desc" && <ChevronDown aria-hidden className="size-3" />}
                      </button>
                    ) : (
                      flexRender(header.column.columnDef.header, header.getContext())
                    )}
                  </TableHead>
                );
              })}
            </TableRow>
          ))}
        </TableHeader>

        <TableBody className="grid" style={{ height: totalSize, position: "relative" }}>
          {virtualRows.map((virtualRow) => {
            const row = rows[virtualRow.index] as Row<TData>;
            const selected = isRowSelected?.(row.original) ?? false;
            return (
              <TableRow
                key={row.id}
                data-state={selected ? "selected" : undefined}
                data-focused={(isRowFocused?.(row.original) ?? false) || undefined}
                aria-selected={selected}
                // +2: ARIA rows are 1-based and the header occupies row 1.
                aria-rowindex={virtualRow.index + 2}
                className={cn(
                  "absolute flex w-full items-center",
                  "data-[focused=true]:ring-1 data-[focused=true]:ring-inset data-[focused=true]:ring-ring",
                )}
                style={{ height: virtualRow.size, transform: `translateY(${virtualRow.start}px)` }}
                onClick={(event) => onRowClick?.(row.original, event)}
                onDoubleClick={(event) => onRowDoubleClick?.(row.original, event)}
                onPointerEnter={() => onRowPointerEnter?.(row.original)}
                onContextMenu={() => setContextRow(row.original)}
              >
                {row.getVisibleCells().map((cell) => (
                  <TableCell
                    key={cell.id}
                    style={{ width: cell.column.getSize() }}
                    className="flex h-full items-center overflow-hidden"
                  >
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </TableCell>
                ))}
              </TableRow>
            );
          })}
        </TableBody>
      </Table>

      {/* Distinct states, per docs/05: "expansion, loading, empty, error, and
          next-page states are distinct rows." */}
      {error !== null && (
        <p role="alert" className="px-3 py-2 text-sm text-destructive">
          {error.message}
        </p>
      )}
      {isLoading && rows.length === 0 && (
        <p className="flex items-center gap-2 px-3 py-2 text-sm text-muted-foreground">
          <Loader aria-hidden className="size-3.5 animate-spin" />
          Loading…
        </p>
      )}
      {!isLoading && error === null && rows.length === 0 && (
        <p className="px-3 py-2 text-sm text-muted-foreground">{emptyMessage}</p>
      )}
      {isFetchingNextPage && (
        <p className="flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground">
          <Loader aria-hidden className="size-3 animate-spin" />
          Loading more…
        </p>
      )}
      {!isFetchingNextPage && hasNextPage && (
        <p className="px-3 py-2 text-xs text-muted-foreground">Scroll for more…</p>
      )}
    </div>
  );

  if (renderContextMenu === undefined) {
    return <div className={cn("min-h-0 flex-1", className)}>{body}</div>;
  }

  return (
    <div className={cn("min-h-0 flex-1", className)}>
      <ContextMenu onOpenChange={(open) => !open && setContextRow(null)}>
        <ContextMenuTrigger asChild>{body}</ContextMenuTrigger>
        <ContextMenuContent>
          {contextRow !== null && renderContextMenu(contextRow)}
        </ContextMenuContent>
      </ContextMenu>
    </div>
  );
}
