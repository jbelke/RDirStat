/**
 * Ages — "how old is the data sitting here", without sorting a date column and
 * reading down it.
 *
 * ## Two unit systems, again, and again shown rather than assumed
 *
 * A bucket edge is a **duration** ("30 days"), because that is the threshold a
 * person carries in their head. The mtime column in the breakdown underneath is
 * a **date** ("14 Nov 2023"), because that is what a file has. Those are not
 * the same kind of thing, and asking the user to subtract one from the other in
 * their head is how a report loses trust — so every row renders both: the
 * duration as the heading, and the calendar window it resolves to on the line
 * below. The conversion is shown, never inferred.
 *
 * That window moves. It is computed against a single pinned instant supplied by
 * the caller (`nowUnixSeconds`), not against `Date.now()` at render time, for
 * the same reason the Rust side takes `now` as a parameter: the row counts, the
 * expanded file list, and the dates in the header must all describe one moment.
 * Re-reading the clock per render would let a row say "since 14 Nov" over a
 * list computed a second earlier.
 *
 * ## What is counted
 *
 * Files, by their own mtime. Directories are not bucketed: a directory's size
 * is its subtree total, so bucketing both it and its contents would count the
 * same bytes twice and the rows would not sum to the subtree. A directory's
 * mtime is also a poor proxy for its contents' age — it moves when an entry is
 * added or removed, not when one is edited. The buckets partition the files.
 *
 * A file dated in the **future** — clock skew, a build system stamping
 * artefacts forward, `touch -t 2099` — is in the newest bucket. It is not
 * stale, and the alternative would file tomorrow's file under "older than two
 * years" and invite the user to delete it.
 */

import { ChevronRight, Copy, Eye, Info, Lock, Trash2 } from "lucide-react";
import { Fragment, useState } from "react";

import { formatCount, formatMtime, formatSI } from "@/lib/format";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { cn } from "@/lib/utils";

/**
 * One row of the age histogram, mirroring `rdirstat_core::by_age::AgeBucketRow`.
 *
 * Edges are ages in **seconds**, not timestamps, because that is what the
 * backend can state without a timezone. Resolving them to dates is this file's
 * job and needs the pinned `now`.
 */
export interface AgeBucketView {
  readonly bucket: number;
  /** Inclusive lower age bound in seconds; `0` on the newest bucket. */
  readonly lowerSeconds: number;
  /** Exclusive upper age bound in seconds; `null` on the oldest bucket. */
  readonly upperSeconds: number | null;
  readonly files: number;
  readonly logical: number;
  readonly allocated: number;
}

/** One file inside a bucket. The breakdown is a leaderboard, not an enumeration. */
export interface AgeBucketEntryView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly logical: number;
  readonly mtime: number;
  readonly category: number;
}

export interface AgesRouteProps {
  rows: readonly AgeBucketView[] | undefined;
  isLoading: boolean;
  error: Error | null;
  /** Total allocated bytes of the subtree, for the share column. */
  subtreeAllocated: number | null;
  /**
   * The instant `rows` and `entries` were both computed against, in whole Unix
   * seconds. Must be the same value handed to the backend, or the dates in the
   * header describe a different moment than the counts beside them.
   */
  nowUnixSeconds: number;
  /**
   * Which bucket is expanded, or `null`.
   *
   * Controlled rather than internal, unlike the equivalent in `SizeBands`. The
   * breakdown is an `O(subtree)` query, so whoever owns the query has to know
   * which bucket is open; splitting that across two sources of truth is how a
   * list ends up rendered under the wrong heading.
   */
  expandedBucket: number | null;
  onExpandedBucketChange: (bucket: number | null) => void;
  /** The largest files in `expandedBucket`, biggest first. */
  entries: readonly AgeBucketEntryView[] | undefined;
  entriesLoading: boolean;
  entriesError: Error | null;
  /** The backend's ceiling on `entries`, for the truncation footer. */
  entryLimit?: number;
  /** Reveal one file in Finder. Same handler the tree and the canvas use. */
  onReveal?: (node: number) => void;
  /** Move one file to the Trash. Refused unless deletion is armed. */
  onTrash?: (node: number) => void;
  /** Whether deletion is armed; drives the Trash item's wording, not just its state. */
  trashEnabled?: boolean;
  className?: string;
}

const SECONDS_PER_DAY = 86_400;
const SECONDS_PER_HOUR = 3_600;

/**
 * The pinned `now` to hand both this component and the backend.
 *
 * Floored to the hour on purpose. The obvious `Math.floor(Date.now() / 1000)`
 * produces a different number every second, and `now` is part of the query key
 * for an `O(subtree)` walk — a value that changes every second is a refetch
 * every second over a twelve-million-node tree. The buckets are measured in
 * days, so an hour of granularity is invisible in the answer and is the
 * difference between one walk and 3,600 of them.
 */
export function pinnedNowUnixSeconds(): number {
  return Math.floor(Date.now() / 1000 / SECONDS_PER_HOUR) * SECONDS_PER_HOUR;
}

/** `604800` -> `"7 days"`, `63072000` -> `"2 years"`. */
function durationLabel(seconds: number): string {
  const days = Math.round(seconds / SECONDS_PER_DAY);
  // Only exact multiples of 365 become years, so "90 days" never silently
  // becomes "0.2 years" and the label always names the edge the backend used.
  if (days >= 365 && days % 365 === 0) {
    const years = days / 365;
    return years === 1 ? "1 year" : `${years} years`;
  }
  return days === 1 ? "1 day" : `${days} days`;
}

/** `Last 7 days`, `30 days – 90 days`, `Older than 2 years`. */
function bucketLabel(row: AgeBucketView): string {
  if (row.upperSeconds === null) return `Older than ${durationLabel(row.lowerSeconds)}`;
  if (row.lowerSeconds === 0) return `Last ${durationLabel(row.upperSeconds)}`;
  return `${durationLabel(row.lowerSeconds)} – ${durationLabel(row.upperSeconds)}`;
}

/**
 * The same edge as a calendar window, so the row reconciles with the date
 * column in the breakdown.
 *
 * Ages run the opposite way to dates: the *lower* age bound is the *later*
 * date. Written out rather than left as an inversion for the reader to perform.
 */
function windowLabel(row: AgeBucketView, nowUnixSeconds: number): string {
  const oldestEnd = row.upperSeconds === null ? null : nowUnixSeconds - row.upperSeconds;
  const newestEnd = row.lowerSeconds === 0 ? null : nowUnixSeconds - row.lowerSeconds;
  if (oldestEnd === null) return `before ${formatMtime(newestEnd ?? nowUnixSeconds)}`;
  if (newestEnd === null) return `since ${formatMtime(oldestEnd)}`;
  return `${formatMtime(oldestEnd)} – ${formatMtime(newestEnd)}`;
}

export function AgesRoute({
  rows,
  isLoading,
  error,
  subtreeAllocated,
  nowUnixSeconds,
  expandedBucket,
  onExpandedBucketChange,
  entries,
  entriesLoading,
  entriesError,
  entryLimit = 250,
  onReveal,
  onTrash,
  trashEnabled = false,
  className,
}: AgesRouteProps) {
  const [notesOpen, setNotesOpen] = useState(false);

  if (error !== null) {
    return (
      <div className={cn("p-6 text-sm text-pressure-critical", className)}>
        Ages could not be computed: {error.message}
      </div>
    );
  }
  if (isLoading || rows === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Counting…</div>;
  }

  // Oldest first. The backend hands these back newest-first, but the question
  // this view answers is "what has nobody touched", and that answer should not
  // be at the bottom of the table.
  const ordered = [...rows].reverse();
  const widest = ordered.reduce((most, row) => Math.max(most, row.allocated), 0);
  const totalFiles = ordered.reduce((sum, row) => sum + row.files, 0);

  return (
    <div className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}>
      <header className="relative flex shrink-0 items-center gap-2">
        <h2 className="text-sm font-medium">Files by age</h2>
        <span className="rds-numeric text-xs text-muted-foreground">
          {formatCount(totalFiles)} files
        </span>
        {/* The explanation is real and occasionally necessary, but it is the
          * same paragraph every time and would otherwise be the tallest thing
          * on the route. One icon, opened on demand. */}
        <button
          type="button"
          aria-expanded={notesOpen}
          onClick={() => setNotesOpen((open) => !open)}
          title="How these buckets are defined"
          className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Info aria-hidden className="size-3.5" />
          <span className="sr-only">How these buckets are defined</span>
        </button>
        {notesOpen && (
          <div className="absolute left-0 top-full z-50 mt-1 flex w-[30rem] max-w-[80vw] flex-col gap-2 rounded-lg border border-border bg-popover p-3 text-xs text-muted-foreground shadow-xl">
            <p>
              Age is modification time. Buckets are whole days of exactly 86,400 seconds, not
              calendar months — a month is not a fixed duration, and pinning it to one would make
              the same file change bucket depending on which month it is read in.
            </p>
            <p>
              The calendar window under each heading is measured from{" "}
              {formatMtime(nowUnixSeconds, true)}, the instant these counts were taken.
            </p>
            <p>
              Files dated in the future — clock skew, or a build that stamps artefacts forward —
              are in the newest bucket. They are not stale.
            </p>
            <p>
              Directories are not counted: these buckets partition the files, so the rows sum to
              the subtree exactly once. Allocated and logical are never summed with each other.
            </p>
          </div>
        )}
      </header>

      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border/60 text-xs text-muted-foreground">
            <th scope="col" className="py-1.5 text-left font-normal">
              Age
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
              subtreeAllocated !== null && subtreeAllocated > 0
                ? row.allocated / subtreeAllocated
                : 0;
            const empty = row.files === 0;
            const open = expandedBucket === row.bucket;
            return (
              <Fragment key={row.bucket}>
                <tr
                  className={cn(
                    "border-b border-border/30",
                    empty ? "text-muted-foreground/50" : "cursor-pointer hover:bg-accent/40",
                    open && "bg-accent/30",
                  )}
                  onClick={
                    empty ? undefined : () => onExpandedBucketChange(open ? null : row.bucket)
                  }
                >
                  <th scope="row" className="py-2 text-left font-normal">
                    <div className="flex items-center gap-1">
                      {!empty && (
                        <ChevronRight
                          aria-hidden
                          className={cn(
                            "size-3.5 shrink-0 transition-transform",
                            open && "rotate-90",
                          )}
                        />
                      )}
                      <span className={cn("font-medium", empty && "pl-4.5")}>
                        {bucketLabel(row)}
                      </span>
                    </div>
                    {/* The calendar window, always present — never inferred. */}
                    <div className="pl-4.5 text-xs text-muted-foreground">
                      {windowLabel(row, nowUnixSeconds)}
                    </div>
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
                          // Scaled against the heaviest bucket, not the subtree,
                          // so one dominant bucket does not flatten every other
                          // bar to nothing.
                          style={{
                            width: widest > 0 ? `${(row.allocated / widest) * 100}%` : "0%",
                          }}
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
                      <BucketBreakdown
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
    </div>
  );
}

/**
 * The heaviest files in one age bucket.
 *
 * Ordered by size, not by date, and that is deliberate: everything in the
 * bucket is already about as old as everything else, so the question left over
 * is "of the things nobody has touched, which are worth reclaiming".
 *
 * Explicitly a leaderboard. The oldest bucket on a boot volume holds millions
 * of files, so the header says how many there are and the list shows the top
 * few hundred — the alternative is not a longer list, it is a hang.
 */
function BucketBreakdown({
  rows,
  isLoading,
  error,
  total,
  limit,
  onReveal,
  onTrash,
  trashEnabled,
}: {
  rows: readonly AgeBucketEntryView[] | undefined;
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
    return <div className="px-4 py-3 text-xs text-muted-foreground">No files in this bucket.</div>;
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
                {/* With time, unlike the size report: on this route the date is
                  * the reason the row is here, so it earns the extra column. */}
                <span className="rds-numeric w-36 shrink-0 text-right text-[11px] text-muted-foreground tabular-nums">
                  {formatMtime(entry.mtime, true)}
                </span>
              </li>
            </ContextMenuTrigger>
            {/* The same three verbs the tree and the canvas offer, on the same
              * handlers. A file listed here is a file, and a list you cannot act
              * on is a report rather than a tool. */}
            <ContextMenuContent>
              <ContextMenuItem
                disabled={onReveal === undefined}
                onSelect={() => onReveal?.(entry.node)}
              >
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
