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

import { formatCount, formatIEC, formatSI } from "@/lib/format";
import type { SizeBandView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface SizeBandsProps {
  rows: readonly SizeBandView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  /** Total allocated bytes of the subtree, for the share column. */
  subtreeAllocated: number | null;
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

export function SizeBands({ rows, isLoading, error, subtreeAllocated, className }: SizeBandsProps) {
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
      <header className="flex flex-col gap-1">
        <h2 className="text-sm font-medium">Files by size</h2>
        <p className="max-w-2xl text-xs text-muted-foreground">
          Bands are powers of 1024, matching <code className="font-mono">du -h</code>. The decimal
          equivalent is shown beneath each one, because the size column and Finder are decimal SI —
          the same bytes read <span className="rds-numeric">128G</span> in{" "}
          <code className="font-mono">du</code> and <span className="rds-numeric">138 GB</span> here.
          Directories are not counted: these bands partition the files.
        </p>
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
            return (
              <tr
                key={row.band}
                className={cn("border-b border-border/30", empty && "text-muted-foreground/50")}
              >
                <th scope="row" className="py-2 text-left font-normal">
                  <div className="font-medium">{edgeLabel(row)}</div>
                  {/* The conversion, always present — never inferred. */}
                  <div className="text-xs text-muted-foreground">{conversionLabel(row)}</div>
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
            );
          })}
        </tbody>
      </table>

      <p className="text-xs text-muted-foreground">
        {formatCount(totalFiles)} files counted. Allocated and logical are never summed — allocated
        is what the filesystem spent, logical is what the files claim, and on APFS they disagree for
        sparse, compressed, and cloned files.
      </p>
    </div>
  );
}
