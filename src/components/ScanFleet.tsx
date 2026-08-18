/**
 * Several scans at once: the ones running, and the trees they produced.
 *
 * Deliberately **not** a rewrite of {@link ScanProgressStrip}. That strip is a
 * detailed read-out of one scan — coverage against the volume, the current
 * directory, the error breakdown — and it is good at that. This sits above it
 * and answers the different question concurrency creates: *what else is going
 * on, and which tree am I looking at?*
 *
 * The whole component renders nothing when there is nothing concurrency-shaped
 * to say — one scan and one tree looks exactly as it did before. That is the
 * point: the feature should be invisible until you use it.
 *
 * ## Why a waiting scan is shown at all
 *
 * A scan that is queued behind another has no progress to report, so the
 * tempting thing is to hide it until it starts. That is wrong: the user clicked
 * Scan and something has to acknowledge the click, or they conclude it was
 * lost and click again. It is shown, with the reason, and it can be cancelled
 * before it ever runs.
 *
 * ## Why the reason is spelled out
 *
 * "Waiting" alone reads as a bug. "Another scan is reading this disk" reads as
 * a decision — and it is one, because two scans on one device finish *later*
 * than the same two in sequence.
 */

import { Check, Loader2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import { describeWait, type ReadyScanView, type RunningScanView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface ScanFleetProps {
  running: readonly RunningScanView[];
  ready: readonly ReadyScanView[];
  /** The tree currently on screen. */
  viewing: number;
  onView: (generation: number) => void;
  onCancel: (scanId: number) => void;
  className?: string;
}

export function ScanFleet({ running, ready, viewing, onView, onCancel, className }: ScanFleetProps) {
  // One scan and one tree is the old, single-scan world; show nothing so it
  // still looks like it. Two of either is the moment this earns its space.
  const worthShowing = running.length > 1 || ready.length > 1 || running.some((scan) => scan.waiting !== null);
  if (!worthShowing) return null;

  return (
    <div className={cn("flex flex-col gap-1 border-t border-border/60 px-4 py-2 text-xs", className)}>
      {running.length > 0 && (
        <ul className="flex flex-col gap-1">
          {running.map((scan) => (
            <RunningRow key={scan.scanId} scan={scan} onCancel={() => onCancel(scan.scanId)} />
          ))}
        </ul>
      )}

      {ready.length > 1 && (
        <div className="flex flex-wrap items-center gap-1 pt-1">
          <span className="mr-1 text-[10px] uppercase tracking-wide text-muted-foreground">Scanned</span>
          {ready.map((scan) => (
            <ReadyChip
              key={scan.generation}
              scan={scan}
              viewing={scan.generation === viewing}
              onView={() => onView(scan.generation)}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function RunningRow({ scan, onCancel }: { scan: RunningScanView; onCancel: () => void }) {
  const waiting = scan.waiting !== null;
  const progress = scan.lastProgress;

  return (
    <li className="flex items-center gap-2">
      {waiting ? (
        <span aria-hidden className="size-3 shrink-0 rounded-full border border-border/60" />
      ) : (
        <Loader2 aria-hidden className="size-3 shrink-0 animate-spin text-muted-foreground" />
      )}

      <span className="min-w-0 flex-1 truncate font-mono" title={scan.root}>
        {scan.root}
      </span>

      <span className="shrink-0 text-[11px] text-muted-foreground">
        {waiting && scan.waiting !== null
          ? describeWait(scan.waiting)
          : scan.state === "failed"
            ? "failed"
            : progress === null
              ? "starting…"
              : /*
                 * Entries and bytes, not a percentage. The detailed strip below
                 * can compute a percentage because it knows the volume's used
                 * bytes; a compact row for an arbitrary folder does not, and a
                 * made-up denominator is worse than an honest count.
                 */
                `${progress.observedEntries.toLocaleString()} entries · ${formatSI(progress.logicalBytes)}`}
      </span>

      <Button
        variant="outline"
        size="sm"
        className="h-6 shrink-0 px-2"
        onClick={onCancel}
        aria-label={`Cancel the scan of ${scan.root}`}
      >
        <X aria-hidden />
      </Button>
    </li>
  );
}

function ReadyChip({
  scan,
  viewing,
  onView,
}: {
  scan: ReadyScanView;
  viewing: boolean;
  onView: () => void;
}) {
  // The root's last component, because a row of full paths is unreadable at
  // this size and they frequently share a prefix.
  const label = scan.root.replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? scan.root;

  return (
    <button
      type="button"
      onClick={onView}
      aria-current={viewing ? "true" : undefined}
      title={`${scan.root} — ${formatSI(scan.summary.totals.logical)}`}
      className={cn(
        "flex items-center gap-1 rounded border px-2 py-0.5 text-[11px]",
        viewing
          ? "border-foreground/40 bg-foreground/10 font-medium"
          : "border-border/60 text-muted-foreground hover:text-foreground",
      )}
    >
      {viewing && <Check aria-hidden className="size-3" />}
      <span className="max-w-40 truncate">{label}</span>
      <span className="rds-numeric opacity-70">{formatSI(scan.summary.totals.logical)}</span>
    </button>
  );
}
