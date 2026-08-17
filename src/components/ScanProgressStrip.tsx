/**
 * The bottom status strip.
 *
 * docs/05-UI.md, "Scan UX": "During a scan: the bottom status strip animates
 * from the 10 Hz `scan:progress` event, **Cancel always live**. **No live
 * tree.** Incremental rollup means contended writes up every ancestor chain and
 * numbers that visibly churn; counters during, tree after."
 *
 * So this component is the *only* thing that moves while a scan runs. Three
 * consequences:
 *
 * - The progress payload is held in a `useRef`-backed state that is replaced
 *   wholesale 10 times a second. It is deliberately NOT in the Zustand store:
 *   that would re-render every subscriber of every other slice at 10 Hz.
 * - `current_dir` is explicitly best-effort in the contract — it is published
 *   through a `try_lock` and skipped under contention — so a blank path is
 *   normal and must not read as a stall.
 * - **Cancel is optimistic in appearance only.** The button disables itself and
 *   says "Cancelling…", but the strip keeps rendering the scan until the
 *   supervisor reports the state transition. `CancelState::Acknowledged` means
 *   the request was received, not that the walk has stopped.
 */

import { Loader, X } from "lucide-react";
import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { formatCount, formatDuration, formatSI } from "@/lib/format";
import { subscribeScanProgress, type ScanProgressView, type ScanState } from "@/lib/ipc";
import { cn } from "@/lib/utils";

/** Subscribe once and expose the latest absolute counters. */
export function useScanProgress(): ScanProgressView | null {
  const [progress, setProgress] = useState<ScanProgressView | null>(null);
  useEffect(() => subscribeScanProgress(setProgress), []);
  return progress;
}

export interface ScanProgressStripProps {
  state: ScanState;
  progress: ScanProgressView | null;
  onCancel?: () => void;
  /** True from the moment Cancel is pressed until the state actually changes. */
  cancelling?: boolean;
  className?: string;
}

const ACTIVE_STATES: ReadonlySet<ScanState> = new Set<ScanState>([
  "scanning",
  "cancelling",
  "finalizing",
]);

export function ScanProgressStrip({
  state,
  progress,
  onCancel,
  cancelling = false,
  className,
}: ScanProgressStripProps) {
  if (!ACTIVE_STATES.has(state)) return null;

  const isCancelling = cancelling || state === "cancelling";

  return (
    <footer
      role="status"
      aria-live="polite"
      className={cn(
        "flex h-9 shrink-0 items-center gap-3 border-t border-border/60 bg-background/90 px-3 text-xs",
        className,
      )}
    >
      <Loader aria-hidden className="size-3.5 shrink-0 animate-spin text-brand" />

      <span className="shrink-0 font-medium">
        {state === "finalizing" ? "Rolling up totals" : isCancelling ? "Cancelling" : "Scanning"}
      </span>

      <span className="min-w-0 flex-1 truncate font-mono text-muted-foreground" title={progress?.currentDir ?? undefined}>
        {progress?.currentDir ?? ""}
      </span>

      {progress !== null && (
        <>
          <Counter label="entries" value={formatCount(progress.observedEntries)} />
          <Counter label="dirs" value={formatCount(progress.directories)} />
          <Counter label="allocated" value={formatSI(progress.allocatedBytes)} />
          {progress.errors > 0 && (
            <Counter
              label="errors"
              value={formatCount(progress.errors)}
              className="text-pressure-warn"
            />
          )}
          <Counter label="pending" value={formatCount(progress.pendingDirs)} />
          <span className="rds-numeric shrink-0 text-muted-foreground">
            {formatDuration(progress.elapsedMs)}
          </span>
        </>
      )}

      <Button
        variant="outline"
        size="sm"
        // Always live: the contract's whole point is that a 69M-entry walk can
        // be stopped. Only an already-acknowledged cancel disables it.
        disabled={onCancel === undefined || isCancelling}
        onClick={onCancel}
      >
        <X aria-hidden />
        {isCancelling ? "Cancelling…" : "Cancel"}
      </Button>
    </footer>
  );
}

function Counter({
  label,
  value,
  className,
}: {
  label: string;
  value: string;
  className?: string;
}) {
  return (
    <span className={cn("shrink-0 whitespace-nowrap text-muted-foreground", className)}>
      <span className="rds-numeric text-foreground">{value}</span> {label}
    </span>
  );
}
