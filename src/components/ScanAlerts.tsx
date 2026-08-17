/**
 * Completion alerts.
 *
 * docs/05-UI.md: "On completion, alerts surface unreadable directories,
 * exclusions, mutations, aggregation, and a material tree-versus-volume
 * discrepancy with the causes in `03`. **Permission guidance appears only when
 * the recorded error classes support it.**"
 *
 * That last sentence is the reason this component reads `errorCounts` rather
 * than `counts.unreadable_dirs` alone. `ErrorClass::PermissionDenied` is the
 * only class that may trigger Full Disk Access guidance; a directory that
 * failed with `input_output` or `remote_unavailable` is a different problem and
 * telling the user to open System Settings would be a wrong answer confidently
 * delivered.
 *
 * These are alerts, not dialogs, because a per-path failure is recorded and the
 * scan continues — that is the scanner's documented policy, and a partial scan
 * is a success with a payload, never an error.
 */

import { CircleAlert, Info, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { ScanErrorList } from "@/components/ScanErrorList";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { formatCount } from "@/lib/format";
import type { ScanSummaryView } from "@/lib/ipc";
import { useScanErrors } from "@/lib/queries";
import { cn } from "@/lib/utils";

export interface ScanAlertsProps {
  summary: ScanSummaryView | null;
  className?: string;
}

export function ScanAlerts({ summary, className }: ScanAlertsProps) {
  // Unconditional: this component returns null on several paths below, and the
  // hook order must not depend on which one.
  const [detailOpen, setDetailOpen] = useState(false);
  const errors = useScanErrors(detailOpen);

  if (summary === null) return null;

  const permissionDenied = summary.errorCounts
    .filter((entry) => entry.errorClass === "permission_denied")
    .reduce((total, entry) => total + entry.count, 0);

  const otherErrors = summary.errorCounts
    .filter((entry) => entry.errorClass !== "permission_denied")
    .reduce((total, entry) => total + entry.count, 0);

  const alerts: React.ReactNode[] = [];

  if (summary.counts.unreadableDirs > 0) {
    alerts.push(
      <Alert key="unreadable" variant="warning">
        <TriangleAlert aria-hidden />
        <AlertTitle>
          {formatCount(summary.counts.unreadableDirs)} director
          {summary.counts.unreadableDirs === 1 ? "y" : "ies"} could not be read
        </AlertTitle>
        <AlertDescription>
          Their contents are not included, so every total above them is a floor rather than a
          measurement.
          {permissionDenied > 0 ? (
            <>
              {" "}
              {formatCount(permissionDenied)} of the recorded failures were permission denials.
              Granting Full Disk Access, or rescanning the specific folder through the folder picker
              (macOS&rsquo;s explicit-consent path), would let those be counted.
            </>
          ) : (
            <>
              {" "}
              None of the recorded failures were permission denials, so Full Disk Access would not
              change this result.
            </>
          )}
        </AlertDescription>
      </Alert>,
    );
  }

  if (otherErrors > 0) {
    alerts.push(
      <Alert key="errors">
        <CircleAlert aria-hidden />
        <AlertTitle>{formatCount(otherErrors)} other path errors were recorded</AlertTitle>
        <AlertDescription>
          {summary.errorCounts
            .filter((entry) => entry.errorClass !== "permission_denied")
            .map((entry) => `${entry.errorClass.replace(/_/g, " ")} ×${formatCount(entry.count)}`)
            .join(", ")}
          . Each was recorded and the scan continued.
        </AlertDescription>
      </Alert>,
    );
  }

  if (summary.mutations > 0) {
    alerts.push(
      <Alert key="mutations">
        <Info aria-hidden />
        <AlertTitle>The filesystem changed during the scan</AlertTitle>
        <AlertDescription>
          {formatCount(summary.mutations)} entries were created, removed, or resized while they were
          being walked. The tree is a consistent snapshot of what was observed, not of any single
          instant.
        </AlertDescription>
      </Alert>,
    );
  }

  if (summary.excludedRoots.length > 0) {
    alerts.push(
      <Alert key="excluded">
        <Info aria-hidden />
        <AlertTitle>{summary.excludedRoots.length} paths were excluded</AlertTitle>
        <AlertDescription className="break-all font-mono text-[11px]">
          {summary.excludedRoots.slice(0, 8).join(" · ")}
          {summary.excludedRoots.length > 8 && ` · +${summary.excludedRoots.length - 8} more`}
        </AlertDescription>
      </Alert>,
    );
  }

  if (summary.aggregated) {
    alerts.push(
      <Alert key="aggregated">
        <Info aria-hidden />
        <AlertTitle>Small entries were aggregated</AlertTitle>
        <AlertDescription>
          {formatCount(summary.counts.aggregatedNodes)} entries below the aggregation threshold were
          folded into their parents to bound memory. Their bytes are counted; their names are not
          available.
        </AlertDescription>
      </Alert>,
    );
  }

  // The failure list itself, one disclosure away from every alert above that
  // counted something. The alerts say how many and what kind; this says which
  // paths, and is not fetched until it is opened.
  if (permissionDenied + otherErrors > 0) {
    alerts.push(
      <div key="detail" className="flex flex-col gap-2">
        <button
          type="button"
          aria-expanded={detailOpen}
          onClick={() => setDetailOpen((open) => !open)}
          className="self-start text-xs text-muted-foreground underline underline-offset-2 hover:text-foreground"
        >
          {detailOpen ? "Hide the failed paths" : "Show the failed paths"}
        </button>
        {detailOpen && (
          <ScanErrorList
            report={errors.data}
            isLoading={errors.isLoading}
            error={errors.error}
            className="rounded-md border border-border/60 p-3"
          />
        )}
      </div>,
    );
  }

  if (alerts.length === 0) return null;

  return <div className={cn("flex flex-col gap-2", className)}>{alerts}</div>;
}
