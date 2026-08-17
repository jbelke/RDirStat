/**
 * "What are those N errors?"
 *
 * docs/05-UI.md: "**An error count is not an error report.** The strip's
 * `N errors` is the one counter that describes something going *wrong*, and a
 * bare number invites the only two readings that are both wrong: 'the scan is
 * broken' and 'this is normal, ignore it'."
 *
 * So this renders the two halves the backend keeps, and is careful about which
 * is which:
 *
 * - **The class breakdown is exact.** Every recorded failure is counted, and
 *   the totals here always add up to the number in the strip.
 * - **The path list is a sample.** A running scan keeps the first 64 in full
 *   and a completed one the first 10,000; `truncated` says so out loud rather
 *   than letting a list of 64 paths imply that there were 64 failures.
 *
 * It renders the same in both states — live and final — because the question is
 * the same one, and a user watching a counter climb should not have to wait for
 * a scan to end to find out that every failure so far is `~/Library` refusing
 * to open.
 */

import { ShieldAlert } from "lucide-react";

import { formatCount } from "@/lib/format";
import type { ErrorClass, Operation, ScanErrorsView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

/**
 * Plain-language names for the stable error classes.
 *
 * The wire name (`permission_denied`) is a contract token, not English, and
 * `errno` is worse. Every string here says what happened to the *path*, which
 * is the only thing the user can act on.
 */
const CLASS_LABEL: Record<ErrorClass, string> = {
  permission_denied: "macOS refused access",
  not_found: "vanished mid-scan",
  not_a_directory: "not a directory when reopened",
  symlink_loop: "symlink loop",
  too_many_open_files: "too many open files",
  name_too_long: "name too long",
  invalid_name: "name is not valid UTF-8",
  input_output: "I/O error from the device",
  remote_unavailable: "network volume stopped answering",
  other: "other OS error",
};

/** What the scanner was doing. `null` for failures that have no operation. */
const OPERATION_LABEL: Record<Operation, string> = {
  open_dir: "opening a folder",
  read_dir: "listing a folder",
  metadata: "reading metadata",
  read_link: "reading a symlink",
  stat_fs: "reading volume capacity",
  read_file: "reading a file",
  reveal: "revealing in Finder",
  trash: "moving to the Trash",
  persist: "writing a snapshot",
};

/**
 * The classes for which Full Disk Access is the actual remedy.
 *
 * docs/05: "Permission guidance appears only when the recorded error classes
 * support it." Telling someone to open System Settings because a network share
 * timed out is a wrong answer delivered confidently.
 */
function isPermissionClass(errorClass: ErrorClass): boolean {
  return errorClass === "permission_denied";
}

export interface ScanErrorListProps {
  report: ScanErrorsView | undefined;
  isLoading?: boolean;
  error?: Error | null;
  /** How many sample paths to draw. The rest stay behind the truncation note. */
  maxSamples?: number;
  className?: string;
}

export function ScanErrorList({
  report,
  isLoading = false,
  error = null,
  maxSamples = 40,
  className,
}: ScanErrorListProps) {
  if (error !== null) {
    return <p className={cn("text-xs text-destructive", className)}>Could not read the error log: {error.message}</p>;
  }
  if (isLoading && report === undefined) {
    return <p className={cn("text-xs text-muted-foreground", className)}>Reading the error log…</p>;
  }
  if (report === undefined || report.total === 0) {
    return <p className={cn("text-xs text-muted-foreground", className)}>No failures were recorded.</p>;
  }

  const permissionDenied = report.counts
    .filter((entry) => isPermissionClass(entry.errorClass))
    .reduce((total, entry) => total + entry.count, 0);

  return (
    <div className={cn("flex flex-col gap-3", className)}>
      <p className="text-xs text-muted-foreground">
        {formatCount(report.total)} path{report.total === 1 ? "" : "s"} could not be read
        {report.live ? " so far" : ""}. Each one was recorded and the scan continued, so every total
        above them is a floor rather than a measurement.
      </p>

      <ul className="flex flex-col gap-1">
        {report.counts.map((entry) => (
          <li
            key={`${entry.errorClass}:${entry.operation ?? "-"}`}
            className="flex items-baseline justify-between gap-3 text-xs"
          >
            <span className={cn(isPermissionClass(entry.errorClass) && "text-pressure-warn")}>
              {CLASS_LABEL[entry.errorClass]}
              {entry.operation !== null && (
                <span className="text-muted-foreground"> while {OPERATION_LABEL[entry.operation]}</span>
              )}
            </span>
            <span className="rds-numeric shrink-0 tabular-nums">{formatCount(entry.count)}</span>
          </li>
        ))}
      </ul>

      {permissionDenied > 0 && (
        <p className="flex gap-2 rounded-md border border-border/60 bg-muted/30 p-2 text-[11px] text-muted-foreground">
          <ShieldAlert aria-hidden className="mt-px size-3.5 shrink-0 text-pressure-warn" />
          <span>
            {formatCount(permissionDenied)} of these are permission denials. Granting Full Disk Access,
            or rescanning that folder through the folder picker — macOS&rsquo;s explicit-consent path —
            would let them be counted.
          </span>
        </p>
      )}

      {report.samples.length > 0 && (
        <div className="flex min-h-0 flex-col gap-1">
          <div className="flex items-baseline justify-between">
            <h4 className="text-[10px] uppercase tracking-wide text-muted-foreground">Paths</h4>
            {report.truncated && (
              <span className="text-[10px] text-muted-foreground">
                first {formatCount(Math.min(report.samples.length, maxSamples))} of{" "}
                {formatCount(report.total)}
              </span>
            )}
          </div>
          <ul className="max-h-56 overflow-y-auto rounded-md border border-border/60">
            {report.samples.slice(0, maxSamples).map((sample, index) => (
              <li
                key={`${sample.path ?? sample.kind}:${index}`}
                className="border-b border-border/40 px-2 py-1 last:border-b-0"
              >
                <div className="break-all font-mono text-[11px]" data-selectable>
                  {sample.path ?? sample.detail}
                </div>
                <div className="text-[10px] text-muted-foreground">
                  {CLASS_LABEL[sample.errorClass]}
                  {sample.operation !== null && ` · ${OPERATION_LABEL[sample.operation]}`}
                  {sample.osCode !== null && sample.osCode !== 0 && ` · errno ${sample.osCode}`}
                </div>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
