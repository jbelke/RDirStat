/**
 * Scan diff — what appeared, what vanished, and what moved the needle.
 *
 * ## A diff that does not name its two sides is unreadable
 *
 * The header is not decoration. "42 files added" is meaningless without "since
 * when", and a restored snapshot can be stale by any amount (the same reason
 * `DriveSwitcher` refuses to offer a restore without an age). Both scan roots
 * and both timestamps are therefore **required props**, printed at the top, and
 * a missing timestamp renders as an explicit "time unknown" rather than being
 * quietly omitted.
 *
 * ## Signs, not colours
 *
 * The delta is the number the eye should land on, so it is always written with
 * an explicit leading `+` or `−` in tabular figures, and the magnitude is drawn
 * as a bar that leaves the centre line rightwards for growth and leftwards for
 * shrinkage. Colour is deliberately **not** the carrier: `styles/categories.css`
 * reserves the green/amber/red ramp for capacity pressure and warns against
 * reusing it on another axis, and on this screen "red" would have to mean
 * "grew", which is the opposite of what a red capacity bar means two panels
 * away. Sign plus direction survives that, and survives colour blindness.
 *
 * ## Logical and allocated are never summed
 *
 * Every row carries both deltas. One of them — the one the segmented control
 * selects — orders the list and decides grown from shrunk; the other is shown
 * beside it, greyed, and never added to it. The header says which one is doing
 * the ranking, because a screenshot must not be ambiguous about that.
 *
 * ## Bounded by construction
 *
 * The backend returns at most `limit` rows per class and the true counts
 * separately. Where the two disagree the footer says so — "the 500 largest of
 * 41,220" — rather than letting a truncated list read as a complete inventory.
 */

import { ArrowDownRight, ArrowUpRight, Copy, Eye, FilePlus2, FileX2, Lock, Trash2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";

import { SegmentedControl } from "@/components/SegmentedControl";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { categoryColorVar, categoryOf } from "@/lib/categories";
import { formatCount, formatMtime, formatSI } from "@/lib/format";
import type { Kind } from "@/lib/ipc";
import { cn } from "@/lib/utils";

// ---------------------------------------------------------------------------
// View models.
//
// Declared here rather than in `@/lib/ipc` because the command that produces
// them is not wired yet; the adapter owns the snake_case-to-camelCase mapping
// and this file only ever sees the mapped shape. These mirror
// `rdirstat_core::diff` field for field.
// ---------------------------------------------------------------------------

/** Which size ordered and classified the comparison. */
export type DiffMetricKind = "logical" | "allocated";

/** How one entry differs. There is no `metadataChanged`: see `DiffSummaryView`. */
export type DiffChangeKind = "added" | "removed" | "grown" | "shrunk";

/**
 * Which tree a row's `node` indexes.
 *
 * `before` node ids belong to the earlier scan, which is **not** the published
 * generation. They name a path that no longer exists, so Reveal and Trash are
 * refused for them — stated here so a call site cannot forget.
 */
export type DiffSideKind = "before" | "after";

/** Which two scans are being compared, and when each was taken. */
export interface DiffScanRef {
  /** The scan root, escaped for display. */
  readonly root: string;
  /** When that scan finished, Unix **milliseconds**. `null` when unknown. */
  readonly takenUnixMs: number | null;
  /** Retained nodes in that scan's arena. */
  readonly nodes: number;
}

/** Signed totals for one change class. Never truncated, never merged. */
export interface DiffClassTotalsView {
  readonly entries: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
}

/** The uncapped counts. The lists beside them are leaderboards. */
export interface DiffSummaryView {
  readonly added: DiffClassTotalsView;
  readonly removed: DiffClassTotalsView;
  readonly grown: DiffClassTotalsView;
  readonly shrunk: DiffClassTotalsView;
  /** Same bytes, different mtime/kind/flags. Counted, not listed: a metadata
   * change has no magnitude and cannot be ranked among size movers. */
  readonly metadataChanged: number;
  /** Entries whose kind changed — a file replaced by a folder of the same name. */
  readonly kindChanged: number;
  readonly unchanged: number;
  readonly compared: number;
  readonly descended: number;
  readonly beforeLogical: number;
  readonly beforeAllocated: number;
  readonly afterLogical: number;
  readonly afterAllocated: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
  /** The backend walk stopped early. Only reachable on a corrupt arena. */
  readonly truncated: boolean;
}

/** One change, ready to draw. */
export interface DiffEntryView {
  readonly change: DiffChangeKind;
  readonly side: DiffSideKind;
  readonly node: number;
  readonly path: string;
  readonly kind: Kind;
  readonly kindChanged: boolean;
  /** Entries this row covers, including itself. A removed folder is one row. */
  readonly entries: number;
  readonly beforeLogical: number;
  readonly beforeAllocated: number;
  readonly afterLogical: number;
  readonly afterAllocated: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
  /** Unix **seconds**, signed. `null` on the side the entry did not exist. */
  readonly beforeMtime: number | null;
  readonly afterMtime: number | null;
  readonly category: number;
}

/** The whole comparison, exactly as `rdirstat_core::diff::DiffReport`. */
export interface DiffReportView {
  readonly before: DiffScanRef;
  readonly after: DiffScanRef;
  readonly metric: DiffMetricKind;
  /** Rows per class the backend was willing to return. */
  readonly limit: number;
  readonly summary: DiffSummaryView;
  readonly added: readonly DiffEntryView[];
  readonly removed: readonly DiffEntryView[];
  readonly grown: readonly DiffEntryView[];
  readonly shrunk: readonly DiffEntryView[];
}

export interface DiffRouteProps {
  report: DiffReportView | undefined;
  isLoading: boolean;
  error: Error | null;
  /**
   * Why there is nothing to diff, when that is a fact rather than a failure —
   * "only one saved scan for this volume". Distinguishes "not possible yet"
   * from "still loading", which an `undefined` report alone cannot.
   */
  unavailableReason?: string | null;
  /** Reveal one file in Finder. Same handler the tree and the canvas use. */
  onReveal?: (node: number) => void;
  /** Move one file to the Trash. Refused unless deletion is armed. */
  onTrash?: (node: number) => void;
  /** Whether deletion is armed; drives the Trash item's wording, not just its state. */
  trashEnabled?: boolean;
  /** Re-rank by the other size. Omit to hide the control. */
  onMetricChange?: (metric: DiffMetricKind) => void;
  className?: string;
}

// ---------------------------------------------------------------------------
// Formatting.
// ---------------------------------------------------------------------------

/**
 * A signed byte delta.
 *
 * `formatSI` clamps negatives to zero — byte counts are `u64` in Rust and it
 * mirrors that — so the sign is carried separately and the magnitude formatted
 * from the absolute value. A minus sign is U+2212, not a hyphen, so it lines
 * up with the digits in tabular figures.
 */
function formatDelta(bytes: number): string {
  if (bytes === 0) return "±0 B";
  const sign = bytes > 0 ? "+" : "−";
  return `${sign}${formatSI(Math.abs(bytes))}`;
}

/** `Aug 17, 2026, 14:32`, or an explicit gap. Input is Unix milliseconds. */
function formatTaken(takenUnixMs: number | null): string {
  if (takenUnixMs === null) return "time unknown";
  return formatMtime(takenUnixMs / 1000, true);
}

const CHANGE_LABEL: Record<DiffChangeKind, string> = {
  added: "Added",
  removed: "Removed",
  grown: "Grew",
  shrunk: "Shrank",
};

const FILTERS = [
  { value: "all" as const, label: "All", title: "Every change, biggest first" },
  { value: "added" as const, label: "Added", title: "Present only in the newer scan" },
  { value: "removed" as const, label: "Removed", title: "Present only in the older scan" },
  { value: "grown" as const, label: "Grew", title: "In both scans, larger now" },
  { value: "shrunk" as const, label: "Shrank", title: "In both scans, smaller now" },
];

type FilterValue = (typeof FILTERS)[number]["value"];

const METRICS = [
  { value: "logical" as const, label: "Logical", title: "File size as reported by the filesystem" },
  { value: "allocated" as const, label: "Allocated", title: "Blocks actually occupied (st_blocks × 512)" },
];

/** The delta the report was ranked by, for one row. */
function rankedDelta(entry: DiffEntryView, metric: DiffMetricKind): number {
  const selected = metric === "logical" ? entry.logicalDelta : entry.allocatedDelta;
  // The backend falls back to the other metric when the selected one did not
  // move (a file rewritten sparse keeps its `st_size`), so the ordering here
  // has to fall back the same way or the list looks unsorted.
  if (selected !== 0) return selected;
  return metric === "logical" ? entry.allocatedDelta : entry.logicalDelta;
}

// ---------------------------------------------------------------------------

export function DiffRoute({
  report,
  isLoading,
  error,
  unavailableReason = null,
  onReveal,
  onTrash,
  trashEnabled = false,
  onMetricChange,
  className,
}: DiffRouteProps) {
  const [filter, setFilter] = useState<FilterValue>("all");

  const metric = report?.metric ?? "logical";
  const rows = useMemo(() => {
    if (report === undefined) return [];
    const chosen =
      filter === "all"
        ? [...report.added, ...report.removed, ...report.grown, ...report.shrunk]
        : [...report[filter]];
    // Each class arrives ranked, but "All" interleaves four lists, so it is
    // re-ranked by magnitude here. Sorting a merged view of at most
    // `4 × limit` rows is trivial; sorting the underlying changes is the
    // backend's job and it already did it.
    return chosen.sort((a, b) => Math.abs(rankedDelta(b, metric)) - Math.abs(rankedDelta(a, metric)));
  }, [report, filter, metric]);

  const widest = useMemo(
    () => rows.reduce((most, row) => Math.max(most, Math.abs(rankedDelta(row, metric))), 0),
    [rows, metric],
  );

  if (error !== null) {
    return (
      <div className={cn("p-6 text-sm text-pressure-critical", className)}>
        The two scans could not be compared: {error.message}
      </div>
    );
  }
  if (unavailableReason !== null) {
    return (
      <div className={cn("flex flex-col gap-2 p-6 text-sm text-muted-foreground", className)}>
        <p>Nothing to compare yet.</p>
        <p className="text-xs">{unavailableReason}</p>
      </div>
    );
  }
  if (isLoading || report === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Comparing…</div>;
  }

  const { summary } = report;
  const shownCounts: Record<FilterValue, number> = {
    all:
      summary.added.entries + summary.removed.entries + summary.grown.entries + summary.shrunk.entries,
    added: summary.added.entries,
    removed: summary.removed.entries,
    grown: summary.grown.entries,
    shrunk: summary.shrunk.entries,
  };
  const total = shownCounts[filter];

  return (
    <div className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}>
      <DiffHeader report={report} onMetricChange={onMetricChange} />

      <div className="grid shrink-0 grid-cols-2 gap-2 sm:grid-cols-4">
        <SummaryTile
          label="Added"
          icon={<FilePlus2 aria-hidden className="size-3.5" />}
          entries={summary.added.entries}
          delta={metric === "logical" ? summary.added.logicalDelta : summary.added.allocatedDelta}
        />
        <SummaryTile
          label="Removed"
          icon={<FileX2 aria-hidden className="size-3.5" />}
          entries={summary.removed.entries}
          delta={metric === "logical" ? summary.removed.logicalDelta : summary.removed.allocatedDelta}
        />
        <SummaryTile
          label="Grew"
          icon={<ArrowUpRight aria-hidden className="size-3.5" />}
          entries={summary.grown.entries}
          delta={metric === "logical" ? summary.grown.logicalDelta : summary.grown.allocatedDelta}
        />
        <SummaryTile
          label="Shrank"
          icon={<ArrowDownRight aria-hidden className="size-3.5" />}
          entries={summary.shrunk.entries}
          delta={metric === "logical" ? summary.shrunk.logicalDelta : summary.shrunk.allocatedDelta}
        />
      </div>

      <p className="shrink-0 text-xs text-muted-foreground">
        {formatCount(summary.compared)} names compared. {formatCount(summary.unchanged)} unchanged,{" "}
        {formatCount(summary.metadataChanged)} touched without changing size
        {summary.kindChanged > 0 && `, ${formatCount(summary.kindChanged)} changed type`}.
        {summary.truncated && " The comparison stopped early — these counts are a floor."}
      </p>

      <div className="flex shrink-0 items-center gap-2">
        <SegmentedControl
          label="Change class"
          options={FILTERS}
          value={filter}
          onChange={(next) => setFilter(next)}
        />
        <span className="rds-numeric text-xs text-muted-foreground">
          {rows.length < total
            ? `the ${formatCount(rows.length)} largest of ${formatCount(total)}`
            : `all ${formatCount(rows.length)}`}
        </span>
      </div>

      {rows.length === 0 ? (
        <div className="p-6 text-sm text-muted-foreground">
          {total === 0
            ? "The two scans agree here — nothing was added, removed, or resized."
            : "No rows in this class."}
        </div>
      ) : (
        <ul className="flex flex-col">
          {rows.map((entry) => (
            <DiffRow
              key={`${entry.side}:${entry.change}:${entry.node}`}
              entry={entry}
              metric={metric}
              widest={widest}
              onReveal={onReveal}
              onTrash={onTrash}
              trashEnabled={trashEnabled}
            />
          ))}
        </ul>
      )}

      {rows.length < total && (
        <p className="shrink-0 text-[11px] text-muted-foreground">
          Capped at {formatCount(report.limit)} rows per class; the counts above are the true totals.
        </p>
      )}
    </div>
  );
}

/**
 * Which two scans, taken when, and which size is doing the ranking.
 *
 * Always rendered, never collapsible. This is the sentence that makes every
 * number below it mean something.
 */
function DiffHeader({
  report,
  onMetricChange,
}: {
  report: DiffReportView;
  onMetricChange?: (metric: DiffMetricKind) => void;
}) {
  const { before, after, metric, summary } = report;
  const net = metric === "logical" ? summary.logicalDelta : summary.allocatedDelta;
  const other = metric === "logical" ? summary.allocatedDelta : summary.logicalDelta;

  return (
    <header className="flex shrink-0 flex-col gap-2">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h2 className="text-sm font-medium">Changes between two scans</h2>
        {onMetricChange !== undefined && (
          <SegmentedControl
            label="Rank changes by"
            options={METRICS}
            value={metric}
            onChange={onMetricChange}
            className="ml-auto"
          />
        )}
      </div>

      <div className="flex flex-wrap items-stretch gap-2 text-xs">
        <ScanCard caption="Before" scan={before} />
        <ScanCard caption="After" scan={after} />
        <div className="flex min-w-[10rem] flex-1 flex-col justify-center rounded-md border border-border px-3 py-2">
          <span className="text-muted-foreground">Net change</span>
          <span className="rds-numeric text-base tabular-nums">{formatDelta(net)}</span>
          {/* The other metric, present and never added to the one above it. */}
          <span className="rds-numeric text-[11px] text-muted-foreground tabular-nums">
            {metric === "logical" ? "allocated" : "logical"} {formatDelta(other)}
          </span>
        </div>
      </div>
      <p className="text-[11px] text-muted-foreground">
        Ranked and classified by {metric} bytes. Sizes are decimal SI, matching Finder; logical and
        allocated are shown side by side and never summed.
      </p>
    </header>
  );
}

function ScanCard({ caption, scan }: { caption: string; scan: DiffScanRef }) {
  return (
    <div className="flex min-w-[12rem] flex-1 flex-col rounded-md border border-border px-3 py-2">
      <span className="text-muted-foreground">{caption}</span>
      <span className="truncate font-mono text-[11px]" title={scan.root}>
        {scan.root.length > 0 ? scan.root : "(unknown root)"}
      </span>
      <span className="rds-numeric text-[11px] text-muted-foreground tabular-nums">
        {formatTaken(scan.takenUnixMs)} · {formatCount(scan.nodes)} entries
      </span>
    </div>
  );
}

function SummaryTile({
  label,
  icon,
  entries,
  delta,
}: {
  label: string;
  icon: ReactNode;
  entries: number;
  delta: number;
}) {
  return (
    <div className="flex flex-col rounded-md border border-border px-3 py-2">
      <span className="flex items-center gap-1 text-xs text-muted-foreground">
        {icon}
        {label}
      </span>
      <span className="rds-numeric text-base tabular-nums">{formatCount(entries)}</span>
      <span className="rds-numeric text-[11px] text-muted-foreground tabular-nums">
        {formatDelta(delta)}
      </span>
    </div>
  );
}

/**
 * One change.
 *
 * The bar is diverging: it leaves the centre line rightwards for growth and
 * leftwards for shrinkage, scaled against the largest change *in view* rather
 * than against the volume. Scaling against the volume would flatten every row
 * to nothing, since a diff is usually a rounding error on a full disk.
 */
function DiffRow({
  entry,
  metric,
  widest,
  onReveal,
  onTrash,
  trashEnabled,
}: {
  entry: DiffEntryView;
  metric: DiffMetricKind;
  widest: number;
  onReveal?: (node: number) => void;
  onTrash?: (node: number) => void;
  trashEnabled: boolean;
}) {
  const delta = rankedDelta(entry, metric);
  const secondary = metric === "logical" ? entry.allocatedDelta : entry.logicalDelta;
  const share = widest > 0 ? Math.min(1, Math.abs(delta) / widest) : 0;
  const grew = delta > 0;

  // A `before` row names a path that no longer exists and a node id from a tree
  // that is not the published generation. Acting on it is not "disabled because
  // deletion is off" — it is impossible, and the menu says which.
  const addressable = entry.side === "after";
  const mtime = entry.afterMtime ?? entry.beforeMtime;

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <li className="flex cursor-default items-center gap-3 rounded-sm border-b border-border/20 py-1.5 last:border-0 hover:bg-accent/40">
          <span
            aria-hidden
            className="size-2 shrink-0 rounded-full"
            style={{ background: categoryColorVar(entry.category) }}
          />
          <span className="w-16 shrink-0 text-[11px] text-muted-foreground">
            {CHANGE_LABEL[entry.change]}
          </span>

          <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={entry.path}>
            {entry.path}
          </span>

          {entry.entries > 1 && (
            <span className="rds-numeric shrink-0 text-[11px] text-muted-foreground tabular-nums">
              {formatCount(entry.entries)} entries
            </span>
          )}
          {entry.kindChanged && (
            <span className="shrink-0 rounded border border-border px-1 text-[10px] text-muted-foreground">
              now {entry.kind}
            </span>
          )}

          {/* Diverging bar: the centre line is "no change". */}
          <span aria-hidden className="relative hidden h-1.5 w-32 shrink-0 sm:block">
            <span className="absolute inset-y-0 left-1/2 w-px bg-border" />
            <span
              className={cn("absolute inset-y-0 rounded-full bg-brand")}
              style={
                grew
                  ? { left: "50%", width: `${(share * 100) / 2}%` }
                  : { right: "50%", width: `${(share * 100) / 2}%` }
              }
            />
          </span>

          <span className="rds-numeric w-28 shrink-0 text-right text-xs tabular-nums">
            {formatDelta(delta)}
          </span>
          {/* The metric that is not ranking, present and never summed with it. */}
          <span className="rds-numeric hidden w-24 shrink-0 text-right text-[11px] text-muted-foreground tabular-nums md:block">
            {formatDelta(secondary)}
          </span>
          <span className="rds-numeric hidden w-24 shrink-0 text-right text-[11px] text-muted-foreground tabular-nums lg:block">
            {mtime === null ? "—" : formatMtime(mtime)}
          </span>
        </li>
      </ContextMenuTrigger>

      {/* The same three verbs the tree, the canvas, and the size bands offer, on
        * the same handlers — with the one honest exception that a row from the
        * older scan cannot be acted on. */}
      <ContextMenuContent>
        <ContextMenuItem
          disabled={!addressable || onReveal === undefined}
          onSelect={() => onReveal?.(entry.node)}
        >
          <Eye aria-hidden />
          {addressable ? "Reveal in Finder" : "Reveal in Finder (gone since this scan)"}
        </ContextMenuItem>
        <ContextMenuItem onSelect={() => void navigator.clipboard.writeText(entry.path)}>
          <Copy aria-hidden />
          Copy Path
        </ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem
          variant="destructive"
          disabled={!addressable || onTrash === undefined || !trashEnabled}
          onSelect={() => onTrash?.(entry.node)}
        >
          {trashEnabled && addressable ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
          {!addressable
            ? "Move to Trash… (already gone)"
            : trashEnabled
              ? "Move to Trash…"
              : "Move to Trash… (deletion off)"}
        </ContextMenuItem>
        <ContextMenuSeparator />
        {/* Both absolutes, so "grew by 3.6 GB" can be read as "from 1.2 to 4.8". */}
        <div className="px-2 py-1 text-[11px] text-muted-foreground">
          <div className="rds-numeric tabular-nums">
            logical {formatSI(entry.beforeLogical)} → {formatSI(entry.afterLogical)}
          </div>
          <div className="rds-numeric tabular-nums">
            allocated {formatSI(entry.beforeAllocated)} → {formatSI(entry.afterAllocated)}
          </div>
          <div>{categoryOf(entry.category).label}</div>
        </div>
      </ContextMenuContent>
    </ContextMenu>
  );
}
