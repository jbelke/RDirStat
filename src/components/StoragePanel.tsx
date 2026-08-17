/**
 * What this app has stored on your disk, and how to get it out.
 *
 * ## The honest part
 *
 * The user asked to see "the database and the details for it (duckdb)". There
 * is no DuckDB in this build. `docs/06-DATA.md` describes a DuckDB/Parquet
 * catalog as an explicitly optional later phase, and nothing in the workspace
 * depends on it. So this panel does not draw an empty database and let it look
 * like a broken one — it says the catalog is not present, and shows the store
 * that actually exists.
 *
 * That store is the `*.rdstat` snapshots. From the user's point of view they
 * ARE the database: it is where scans live, it is what survives a relaunch, it
 * is what "back up the data-snapshot" refers to, and it is the app's entire
 * disk footprint. A tool that measures everyone else's disk usage owes an
 * especially straight answer about its own.
 *
 * ## Presentational on purpose
 *
 * Everything arrives as props. No fetching, no store reads. That keeps the
 * panel testable without a backend and — more usefully — means the shell
 * decides when this data is refreshed, which matters because reading it walks
 * a directory and peeks every file.
 */

import { AlertTriangle, Database, Download, FolderOpen, HardDrive, Info } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import { cn } from "@/lib/utils";

export interface StoredSnapshotView {
  readonly path: string;
  readonly rootPath: string;
  readonly device: number;
  readonly takenUnixMs: number;
  readonly nodes: number;
  readonly directories: number;
  /** What the snapshot file costs on disk. */
  readonly bytes: number;
  /** What the snapshot is *about* — the volume it measured. */
  readonly logical: number;
  readonly allocated: number;
  readonly toolVersion: string;
}

export interface UnreadableSnapshotView {
  readonly path: string;
  readonly bytes: number;
  readonly reason: string;
}

export interface StorageReportView {
  readonly directory: string;
  readonly directoryExists: boolean;
  readonly snapshots: readonly StoredSnapshotView[];
  readonly unreadable: readonly UnreadableSnapshotView[];
  readonly totalBytes: number;
  readonly truncated: boolean;
  readonly catalogPresent: boolean;
}

export interface StoragePanelProps {
  report: StorageReportView | null;
  loading?: boolean;
  /** Opens the store directory in Finder. */
  onRevealDirectory?: () => void;
  /** Copies one snapshot somewhere the user chooses. */
  onExport?: (snapshot: StoredSnapshotView) => void;
  className?: string;
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
          Every completed scan is saved so it can be reopened instantly instead of rescanned. This is
          everything RDirStat keeps on your disk.
        </p>
      </header>

      <section className="mb-4 rounded border border-border/60 p-3">
        <div className="flex items-baseline gap-2">
          <span className="text-xs text-muted-foreground">Location</span>
          <span className="min-w-0 flex-1 truncate font-mono text-xs" title={report.directory}>
            {report.directory}
          </span>
          {onRevealDirectory !== undefined && report.directoryExists && (
            <Button variant="ghost" size="sm" onClick={onRevealDirectory}>
              <FolderOpen aria-hidden />
              Reveal
            </Button>
          )}
        </div>
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

      {/* Stated rather than left as a blank panel. A "database" section that is
        * simply empty reads as broken; saying the catalog is a later phase
        * reads as a plan. */}
      {!report.catalogPresent && (
        <Alert className="mb-4">
          <Info aria-hidden />
          <AlertTitle>The query catalog is not part of this build</AlertTitle>
          <AlertDescription>
            docs/06-DATA.md specifies an optional DuckDB/Parquet catalog for cross-scan reporting. It
            is not implemented yet, and nothing here depends on it — scanning, browsing, and the
            saved scans below all work without it. The routes that would need it say so where they
            are disabled.
          </AlertDescription>
        </Alert>
      )}

      {!report.directoryExists ? (
        <p className="text-sm text-muted-foreground">
          Nothing saved yet. The first completed scan creates the store.
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
