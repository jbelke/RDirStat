/**
 * What this app has stored on your disk, and how to get it out.
 *
 * The store is the `*.rdstat` snapshots. From the user's point of view they ARE
 * the database: it is where scans live, it is what survives a relaunch, it is
 * what "back up the data-snapshot" refers to, and it is the app's entire disk
 * footprint. A tool that measures everyone else's disk usage owes an especially
 * straight answer about its own.
 *
 * ## Nothing in here describes the build
 *
 * An earlier version explained that the DuckDB catalog was a later phase, citing
 * the design doc. That is roadmap, and roadmap is for the people writing the
 * app, not the people using it: a user reading their own storage does not need
 * to know which phase something is in, and a panel that lists unbuilt features
 * makes the built ones look provisional. If a capability is absent, it is simply
 * not shown.
 *
 * ## Presentational on purpose
 *
 * Everything arrives as props. No fetching, no store reads. That keeps the
 * panel testable without a backend and — more usefully — means the shell
 * decides when this data is refreshed, which matters because reading it walks
 * a directory and peeks every file.
 */

import { AlertTriangle, Check, Database, Download, FolderOpen, HardDrive, Lock, RotateCcw } from "lucide-react";
import { useEffect, useState } from "react";

import { PathField } from "@/components/PathField";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import type { StorageReportView, StoredSnapshotView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface StoragePanelProps {
  report: StorageReportView | null;
  loading?: boolean;
  /** Opens the store directory in Finder. */
  onRevealDirectory?: () => void;
  /**
   * Points the store somewhere else, or at the default when null.
   *
   * Rejected paths come back as a thrown error carrying the backend's reason,
   * which is the string this panel renders — "that folder is not writable" is
   * an answer the user can act on, and inventing a friendlier one here would
   * mean guessing which of a dozen causes applied.
   */
  onChangeDirectory?: (directory: string | null) => Promise<unknown>;
  /** Copies one snapshot somewhere the user chooses. */
  onExport?: (snapshot: StoredSnapshotView) => void;
  className?: string;
}

/**
 * Where the store is, and — when it is allowed — where to move it.
 *
 * Three things have to be visible at once, and leaving any of them out
 * produces a control that looks broken:
 *
 * - **the location in effect**, which is what the snapshots below are read from;
 * - **which layer chose it**, because `RDIRSTAT_DATA_DIR` silently outranking a
 *   saved folder is indistinguishable from the setting not working;
 * - **whether it can be written to**, since an existing-but-unwritable folder
 *   and an empty store look identical until a scan finishes and cannot be saved.
 *
 * Changing the location does not move the files that are already stored. They
 * are a cache — pruned to two per volume, rebuilt by the next scan — and
 * silently relocating gigabytes as a side effect of editing a text field is
 * not a thing a preference should do. The note under the field says so, rather
 * than leaving the user to discover it from a suddenly empty list.
 */
function Location({
  report,
  onReveal,
  onChange,
}: {
  report: StorageReportView;
  onReveal?: () => void;
  onChange?: (directory: string | null) => Promise<unknown>;
}) {
  const saved = report.configuredDirectory ?? "";
  const [draft, setDraft] = useState(saved);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Re-sync when the report changes underneath — after a successful save, or
  // when another surface changed it. Keyed on the saved value, so typing is
  // never interrupted by an unrelated refetch.
  useEffect(() => {
    setDraft(saved);
    setError(null);
  }, [saved]);

  const editable = onChange !== undefined && !report.directoryLocked;
  const dirty = draft.trim() !== saved;

  async function apply(directory: string | null) {
    if (onChange === undefined) return;
    setBusy(true);
    setError(null);
    try {
      await onChange(directory);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="flex items-baseline gap-2">
        <span className="text-xs text-muted-foreground">Location</span>
        <span className="min-w-0 flex-1 truncate font-mono text-xs" title={report.directory}>
          {report.directory}
        </span>
        {report.directoryState === "readOnly" && (
          <span className="shrink-0 text-xs font-medium text-destructive">not writable</span>
        )}
        {onReveal !== undefined && report.directoryState !== "missing" && (
          <Button variant="ghost" size="sm" onClick={onReveal}>
            <FolderOpen aria-hidden />
            Reveal
          </Button>
        )}
      </div>

      {report.directoryLocked && (
        <p className="mt-2 flex items-start gap-1.5 text-xs text-muted-foreground">
          <Lock aria-hidden className="mt-0.5 size-3 shrink-0" />
          <span>
            Set by <code className="font-mono">RDIRSTAT_DATA_DIR</code> in the environment, which
            wins for this run.
            {saved !== "" && (
              <>
                {" "}
                Your saved folder is <code className="font-mono">{saved}</code> and takes effect
                again when the variable is unset.
              </>
            )}
          </span>
        </p>
      )}

      {editable && (
        <PathField
          className="mt-3"
          inputId="snapshot-dir"
          layout="stacked"
          label="Keep snapshots in"
          placeholder={report.defaultDirectory}
          value={draft}
          disabled={busy}
          onChange={setDraft}
          error={error}
          hint={
            <>
              Snapshots already saved stay where they are; this only changes where the next one is
              written. Leave it empty for <code className="font-mono">{report.defaultDirectory}</code>.
            </>
          }
        >
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={busy || !dirty}
            onClick={() => void apply(draft.trim() === "" ? null : draft.trim())}
          >
            <Check aria-hidden />
            Use this
          </Button>
          {saved !== "" && (
            <Button
              variant="ghost"
              size="sm"
              className="shrink-0"
              disabled={busy}
              onClick={() => void apply(null)}
            >
              <RotateCcw aria-hidden />
              Default
            </Button>
          )}
        </PathField>
      )}
    </>
  );
}

function whenTaken(unixMs: number): string {
  return new Date(unixMs).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

export function StoragePanel({
  report,
  loading = false,
  onRevealDirectory,
  onChangeDirectory,
  onExport,
  className,
}: StoragePanelProps) {
  if (loading && report === null) {
    return <p className={cn("p-4 text-sm text-muted-foreground", className)}>Reading the store…</p>;
  }
  if (report === null) return null;

  // The largest snapshot sets the bar scale. Scaling each bar to the total
  // instead would make every bar tiny once there are more than a few, which is
  // exactly when the comparison starts being worth drawing.
  const largest = report.snapshots.reduce((max, snapshot) => Math.max(max, snapshot.bytes), 0);

  return (
    <div className={cn("flex min-h-0 flex-1 flex-col overflow-auto p-4", className)}>
      <header className="mb-3">
        <h2 className="flex items-center gap-2 text-sm font-medium">
          <Database aria-hidden className="size-4" />
          Stored data
        </h2>
        <p className="mt-1 text-xs text-muted-foreground">
          Every completed scan is saved so it can be reopened instantly instead of rescanned. Two are
          kept per volume and older ones are removed automatically — this is a cache, not a history.
          <strong className="font-medium text-foreground"> Export a scan to keep it.</strong>
        </p>
      </header>

      <section className="mb-4 rounded border border-border/60 p-3">
        <Location report={report} onReveal={onRevealDirectory} onChange={onChangeDirectory} />
        <dl className="mt-2 grid grid-cols-3 gap-2 text-xs">
          <Stat label="Saved scans" value={report.snapshots.length.toLocaleString()} />
          <Stat label="Disk used" value={formatSI(report.totalBytes)} />
          <Stat
            label="Unreadable"
            value={report.unreadable.length.toLocaleString()}
            alarming={report.unreadable.length > 0}
          />
        </dl>
      </section>

      {report.directoryState === "missing" ? (
        <p className="text-sm text-muted-foreground">
          {report.directorySource === "default"
            ? "Nothing saved yet. The first completed scan creates the store."
            : "That folder is not there. If it lives on a removable disk, connect it — scans cannot be saved until then."}
        </p>
      ) : report.snapshots.length === 0 && report.unreadable.length === 0 ? (
        <p className="text-sm text-muted-foreground">The store is empty.</p>
      ) : (
        <ul className="flex flex-col gap-2">
          {report.snapshots.map((snapshot) => (
            <li key={snapshot.path} className="rounded border border-border/60 p-3">
              <div className="flex items-baseline gap-2">
                <HardDrive aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />
                <span className="min-w-0 flex-1 truncate font-mono text-xs" title={snapshot.rootPath}>
                  {snapshot.rootPath}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {whenTaken(snapshot.takenUnixMs)}
                </span>
                {onExport !== undefined && (
                  <Button variant="outline" size="sm" onClick={() => onExport(snapshot)}>
                    <Download aria-hidden />
                    Export…
                  </Button>
                )}
              </div>

              {/* The bar compares snapshot FILE sizes to each other — what each
                * costs to keep. Deliberately not the scanned volume's size,
                * which is a different quantity three orders of magnitude
                * larger and would make every bar identical. */}
              <div
                className="mt-2 h-1.5 overflow-hidden rounded bg-muted"
                role="img"
                aria-label={`${formatSI(snapshot.bytes)} of ${formatSI(report.totalBytes)} total`}
              >
                <div
                  className="h-full bg-brand"
                  style={{ width: `${largest > 0 ? (snapshot.bytes / largest) * 100 : 0}%` }}
                />
              </div>

              <dl className="mt-2 grid grid-cols-4 gap-2 text-[11px]">
                <Stat label="File size" value={formatSI(snapshot.bytes)} />
                <Stat label="Items" value={snapshot.nodes.toLocaleString()} />
                <Stat label="Measured" value={formatSI(snapshot.allocated)} />
                <Stat label="Written by" value={snapshot.toolVersion} />
              </dl>
            </li>
          ))}

          {report.unreadable.map((entry) => (
            <li key={entry.path} className="rounded border border-destructive/40 p-3">
              <div className="flex items-baseline gap-2">
                <AlertTriangle aria-hidden className="size-3.5 shrink-0 text-destructive" />
                <span className="min-w-0 flex-1 truncate font-mono text-xs" title={entry.path}>
                  {entry.path}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground">{formatSI(entry.bytes)}</span>
              </div>
              <p className="mt-1 text-xs text-muted-foreground">
                This file cannot be read, but still occupies disk: {entry.reason}
              </p>
            </li>
          ))}
        </ul>
      )}

      {report.truncated && (
        <p className="mt-3 text-xs text-muted-foreground">
          More files were found than are listed. The totals above still count all of them.
        </p>
      )}
    </div>
  );
}

function Stat({ label, value, alarming = false }: { label: string; value: string; alarming?: boolean }) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5">
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className={cn("rds-numeric truncate text-sm", alarming && "text-pressure-critical")} title={value}>
        {value}
      </dd>
    </div>
  );
}
