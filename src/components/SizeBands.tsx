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
 */

import { ChevronRight, Copy, Eye, Info, Lock, Trash2 } from "lucide-react";
import { Fragment, useState } from "react";

import { formatCount, formatIEC, formatSI, formatMtime } from "@/lib/format";
import type { SizeBandView } from "@/lib/ipc";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { BAND_ENTRY_LIMIT, useSizeBandEntries } from "@/lib/queries";
import { cn } from "@/lib/utils";

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
  className,
}: SizeBandsProps) {
  const [expanded, setExpanded] = useState<number | null>(null);
  const [notesOpen, setNotesOpen] = useState(false);
  const entries = useSizeBandEntries(generation, root, expanded);

  if (error !== null) {
    return (
      <div className={cn("p-6 text-sm text-pressure-critical", className)}>
        Size bands could not be computed: {error.message}
      </div>
    );
  }
  if (isLoading || rows === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Counting…</div>;
  }

  // Largest first: the question this view answers is "what is big", and the
  // answer should not be at the bottom.
  const ordered = [...rows].reverse();
  const widest = ordered.reduce((most, row) => Math.max(most, row.allocated), 0);
  const totalFiles = ordered.reduce((sum, row) => sum + row.files, 0);

  return (
    <div className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}>
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
          </div>
        )}
      </header>

      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border/60 text-xs text-muted-foreground">
            <th scope="col" className="py-1.5 text-left font-normal">
              Band
            </th>
            <th scope="col" className="py-1.5 text-right font-normal">
              Files
            </th>
            <th scope="col" className="py-1.5 text-right font-normal">
              Allocated
            </th>
            <th scope="col" className="py-1.5 pl-3 text-left font-normal">
              Share
            </th>
            <th scope="col" className="py-1.5 text-right font-normal">
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
            return (
              <Fragment key={row.band}>
              <tr
                className={cn(
                  "border-b border-border/30",
                  empty ? "text-muted-foreground/50" : "cursor-pointer hover:bg-accent/40",
                  open && "bg-accent/30",
                )}
                onClick={empty ? undefined : () => setExpanded(open ? null : row.band)}
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
                      rows={entries.data}
                      isLoading={entries.isLoading}
                      error={entries.error}
                      total={row.files}
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

/**
 * The heaviest files in one band.
 *
 * Explicitly a leaderboard. The smallest band on a boot volume holds ten
 * million files, so the header says how many there are and the list shows the
 * top few hundred — the alternative is not a longer list, it is a hang.
 */
function BandBreakdown({
  rows,
  isLoading,
  error,
  total,
  onReveal,
  onTrash,
  trashEnabled,
}: {
  rows: readonly { node: number; path: string; allocated: number; logical: number; mtime: number }[] | undefined;
  isLoading: boolean;
  error: Error | null;
  total: number;
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
    return <div className="px-4 py-3 text-xs text-muted-foreground">No files in this band.</div>;
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
            {/* The same three verbs the tree and the canvas offer, on the same
              * handlers. A file listed here is a file, and a list you cannot act
              * on is a report rather than a tool. */}
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
          Capped at {formatCount(BAND_ENTRY_LIMIT)}; the count above is the true total.
        </div>
      )}
    </div>
  );
}
