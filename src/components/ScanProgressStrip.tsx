/**
 * The scan panel: the one thing on screen that moves while a scan runs.
 *
 * docs/05-UI.md, "Scan UX": "During a scan: the bottom status strip animates
 * from the 10 Hz `scan:progress` event, **Cancel always live**. **No live
 * tree.** Incremental rollup means contended writes up every ancestor chain and
 * numbers that visibly churn; counters during, tree after."
 *
 * A five-minute walk is the longest thing this app does, and until now it was
 * reported in a 36px strip of small grey text. This is the same contract —
 * counters during, tree after — given enough room to actually be read:
 * a coverage bar, a colour ramp that shows travel at a glance, a rate, and the
 * path currently being read.
 *
 * Four things here are deliberate and easy to undo by accident:
 *
 * - The progress payload is held in component state that is replaced wholesale
 *   10 times a second. It is deliberately NOT in the Zustand store: that would
 *   re-render every subscriber of every other slice at 10 Hz.
 * - `current_dir` is explicitly best-effort in the contract — published through
 *   a `try_lock` and skipped under contention — so a blank path is normal and
 *   must not read as a stall.
 * - **Cancel is optimistic in appearance only.** The button disables itself and
 *   says "Cancelling…", but the panel keeps rendering the scan until the
 *   supervisor reports the state transition. `CancelState::Acknowledged` means
 *   the request was received, not that the walk has stopped.
 * - **No `aria-live`.** An earlier version put `aria-live="polite"` on the whole
 *   strip, which asks a screen reader to announce six changing numbers ten times
 *   a second — a firehose that makes the app unusable with VoiceOver rather than
 *   accessible. The bar is a `role="progressbar"` with an `aria-valuetext`
 *   instead, which assistive tech reports when asked rather than continuously.
 */

import { ChevronUp, Loader, X } from "lucide-react";
import { useEffect, useState } from "react";

import { ScanErrorList } from "@/components/ScanErrorList";
import { Button } from "@/components/ui/button";
import { formatCount, formatDuration, formatSI } from "@/lib/format";
import { subscribeScanProgress, type ScanProgressView, type ScanState } from "@/lib/ipc";
import { useScanErrors, useVolumes } from "@/lib/queries";
import {
  coveragePercent,
  scanCoverage,
  scanRampClass,
  volumeForRoot,
  type CoverageBasis,
} from "@/lib/scanProgress";
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
  /**
   * The root this scan was started on, so coverage has a denominator.
   *
   * `scan_status.summary` is null until a scan *completes*, so the root cannot
   * be recovered from status while the scan that needs it is still running —
   * the caller that started the scan is the only thing that knows.
   */
  scanRoot?: string | null;
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

/** What the bar is a fraction *of*. A progress bar that does not say is decoration. */
function basisLabel(basis: CoverageBasis): string {
  switch (basis) {
    case "bytes":
      return "of the volume's used bytes";
    case "directories":
      return "of directories found so far";
    case "unknown":
      return "";
  }
}

export function ScanProgressStrip({
  state,
  progress,
  scanRoot = null,
  onCancel,
  cancelling = false,
  className,
}: ScanProgressStripProps) {
  // Hooks run unconditionally; the early return happens after them, because a
  // panel that unmounts between scans must not change the hook order.
  const [errorsOpen, setErrorsOpen] = useState(false);
  const errors = useScanErrors(errorsOpen);
  // Already fetched and cached by the volume picker, so this is a cache read in
  // the common case rather than a request per scan.
  const volumes = useVolumes();

  if (!ACTIVE_STATES.has(state)) return null;

  const isCancelling = cancelling || state === "cancelling";
  const isFinalizing = state === "finalizing";
  const errorCount = progress?.errors ?? 0;

  const volume = volumeForRoot(volumes.data ?? [], scanRoot);
  const coverage = scanCoverage(progress, volume, scanRoot);
  const percent = coveragePercent(coverage.fraction);

  // Rolling up totals is a real phase with no meaningful fraction, so the bar
  // stops claiming one rather than freezing at whatever it last read.
  const showBar = !isFinalizing && coverage.fraction !== null;
  const target = scanRoot === null ? null : (volume?.name ?? scanRoot);

  const heading = isFinalizing
    ? "Rolling up totals"
    : isCancelling
      ? "Cancelling the scan"
      : target === null
        ? "Scanning"
        : `Scanning ${target}`;

  return (
    <footer
      role="group"
      aria-label="Scan progress"
      className={cn(
        "relative flex shrink-0 flex-col gap-2 border-t border-border/60 bg-background/95 px-4 py-3",
        className,
      )}
    >
      {/* The breakdown opens upward, over the content: the panel is pinned to
        * the bottom edge, and something that pushed the layout would resize the
        * tree every time someone asked a question about the errors. */}
      {errorsOpen && (
        <div className="absolute bottom-full right-4 z-50 mb-1 w-96 rounded-lg border border-border bg-popover p-3 shadow-xl">
          <div className="mb-2 flex items-baseline justify-between">
            <h3 className="text-xs font-medium">Recorded failures</h3>
            <button
              type="button"
              onClick={() => setErrorsOpen(false)}
              className="text-[10px] text-muted-foreground hover:text-foreground"
            >
              Close
            </button>
          </div>
          <ScanErrorList report={errors.data} isLoading={errors.isLoading} error={errors.error} />
        </div>
      )}

      {/* Row 1 — what is happening, how far along, and the way out. */}
      <div className="flex items-center gap-3">
        <Loader aria-hidden className="size-4 shrink-0 animate-spin text-brand" />
        <span className="shrink-0 text-sm font-medium">{heading}</span>

        {percent !== null && !isFinalizing && (
          <span className="rds-numeric shrink-0 text-2xl font-semibold leading-none tabular-nums">
            {percent}
            <span className="ml-0.5 text-base font-normal text-muted-foreground">%</span>
          </span>
        )}

        <span className="min-w-0 flex-1" />

        {progress !== null && (
          <span className="rds-numeric shrink-0 text-sm text-muted-foreground">
            {formatDuration(progress.elapsedMs)}
          </span>
        )}

        <Button
          variant="outline"
          size="sm"
          // Always live: the contract's whole point is that a 69M-entry walk can
          // be stopped. Only an already-acknowledged cancel disables it.
          disabled={onCancel === undefined || isCancelling}
          onClick={onCancel}
          className="shrink-0"
        >
          <X aria-hidden />
          {isCancelling ? "Cancelling…" : "Cancel"}
        </Button>
      </div>

      {/* Row 2 — the bar. Colour is travel, not threshold: see scanRampClass. */}
      {showBar && (
        <div
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={percent ?? 0}
          aria-valuetext={`${percent ?? 0} percent ${basisLabel(coverage.basis)}`}
          className="h-2 w-full overflow-hidden rounded-full bg-muted"
        >
          <div
            className={cn(
              "h-full rounded-full transition-[width,background-color] duration-300 ease-out",
              scanRampClass(coverage.fraction),
            )}
            style={{ width: `${(coverage.fraction ?? 0) * 100}%` }}
          />
        </div>
      )}

      {/* Row 3 — what the bar is a fraction of, stated rather than implied, and
        * always as a floor. */}
      {showBar && (
        <div className="flex items-baseline gap-2 text-xs text-muted-foreground">
          {coverage.basis === "bytes" && coverage.targetBytes !== null ? (
            <span>
              <span className="rds-numeric text-foreground">{formatSI(coverage.observedBytes)}</span> seen of{" "}
              <span className="rds-numeric text-foreground">{formatSI(coverage.targetBytes)}</span> used
            </span>
          ) : (
            <span>{basisLabel(coverage.basis)}</span>
          )}
          <span aria-hidden>·</span>
          {/* Said once, plainly. Exclusions, refused directories and hard-link
            * policy all mean the number only ever understates. */}
          <span>a floor, never a total</span>
        </div>
      )}

      {/* Row 4 — where the walk actually is. Blank is normal. */}
      <div
        className="min-h-4 truncate font-mono text-[11px] text-muted-foreground/80"
        title={progress?.currentDir ?? undefined}
      >
        {progress?.currentDir ?? ""}
      </div>

      {/* Row 5 — the counters. */}
      {progress !== null && (
        <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-xs">
          <Counter label="entries" value={formatCount(progress.observedEntries)} />
          <Counter label="directories" value={formatCount(progress.directories)} />
          <Counter label="pending" value={formatCount(progress.pendingDirs)} />
          {coverage.entriesPerSecond !== null && (
            <Counter label="entries/sec" value={formatCount(Math.round(coverage.entriesPerSecond))} />
          )}
          {errorCount > 0 && (
            // The one counter that reports something going wrong is the one
            // counter you can open. docs/05-UI.md: "An error count is not an
            // error report."
            <button
              type="button"
              aria-expanded={errorsOpen}
              onClick={() => setErrorsOpen((open) => !open)}
              title="Show which paths failed and why"
              className={cn(
                "flex shrink-0 items-center gap-1 whitespace-nowrap rounded-sm px-1 py-0.5 text-pressure-warn",
                "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
              )}
            >
              <span className="rds-numeric text-foreground">{formatCount(errorCount)}</span>
              <span>unreadable</span>
              <ChevronUp aria-hidden className={cn("size-3 transition-transform", errorsOpen && "rotate-180")} />
            </button>
          )}
        </div>
      )}
    </footer>
  );
}

function Counter({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <span className={cn("shrink-0 whitespace-nowrap text-muted-foreground", className)}>
      <span className="rds-numeric text-foreground">{value}</span> {label}
    </span>
  );
}
