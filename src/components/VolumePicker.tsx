/**
 * The launch screen.
 *
 * docs/05-UI.md, "Scan UX": "**The launch screen is the volume picker**,
 * following SquirrelDisk: one row per mounted volume with name, capacity, a
 * pressure-colored bar, and free space … Selecting a volume opens a preflight
 * with estimated scope, permissions, exclusions, and a Scan button; **merely
 * clicking a row never starts a 69M-entry operation**."
 *
 * That last clause is the whole interaction design, so it is enforced
 * structurally rather than by discipline: a row's click handler can only reach
 * `setPreflight`. `onScan` is reachable from exactly one button, inside the
 * expanded preflight, and that button names the volume it will walk.
 *
 * ---------------------------------------------------------------------------
 * KNOWN GAP — "Scan a folder…" is not an NSOpenPanel
 * ---------------------------------------------------------------------------
 * docs/05 wants the secondary action to open a native folder picker, because a
 * folder pick is macOS's explicit-consent path and often avoids needing a broad
 * Full Disk Access grant. That needs `@tauri-apps/plugin-dialog`, which is not
 * in this project's dependency set and cannot be added from here (package.json
 * is owned by another agent).
 *
 * Rather than ship a dead button, the action expands a path field that feeds
 * the same `scan_start`. It works, and it is honestly labelled as the fallback
 * it is. Swapping in `open({ directory: true })` is a three-line change in this
 * file once the plugin lands.
 */

import { FolderOpen, HardDrive, Loader, RefreshCw, ShieldAlert } from "lucide-react";
import { useState } from "react";

import { CapacityBar, capacitySegments } from "@/components/CapacityBar";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatPercent, formatSI } from "@/lib/format";
import { useVolumes } from "@/lib/queries";
import type { VolumeRow } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface VolumePickerProps {
  /** Receives a filesystem path. The backend re-validates it; this is a request, not authority. */
  onScan: (root: string) => void;
  /** True while `scan_start` is in flight or a scan is already running. */
  busy?: boolean;
}

export function VolumePicker({ onScan, busy = false }: VolumePickerProps) {
  const { data, error, isLoading, refetch, isFetching } = useVolumes();
  const [preflight, setPreflight] = useState<string | null>(null);
  const [folderPath, setFolderPath] = useState("");
  const [folderOpen, setFolderOpen] = useState(false);

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-6 px-8 py-10">
      <div className="flex items-baseline justify-between">
        <div>
          <h1 className="text-lg font-medium tracking-tight">Choose a volume to inventory</h1>
          <p className="text-sm text-muted-foreground">
            Sizes are decimal SI, matching Finder. Purgeable space is drawn inside capacity, never
            added beside it.
          </p>
        </div>
        <Button
          variant="ghost"
          size="sm"
          onClick={() => void refetch()}
          disabled={isFetching}
          title="Re-read mounted volumes"
        >
          <RefreshCw aria-hidden className={cn(isFetching && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {isLoading && (
        <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground">
          <Loader aria-hidden className="size-4 animate-spin" />
          Reading mounted volumes…
        </div>
      )}

      {error !== null && (
        <Alert variant="destructive">
          <ShieldAlert aria-hidden />
          <AlertTitle>Could not list volumes</AlertTitle>
          <AlertDescription>{error.message}</AlertDescription>
        </Alert>
      )}

      <ul className="flex flex-col gap-2">
        {(data ?? []).map((volume) => (
          <VolumeCard
            key={`${volume.device}:${volume.mountPoint}`}
            volume={volume}
            expanded={preflight === volume.mountPoint}
            busy={busy}
            onSelect={() =>
              setPreflight((current) => (current === volume.mountPoint ? null : volume.mountPoint))
            }
            onScan={() => onScan(volume.mountPoint)}
          />
        ))}
      </ul>

      {data !== undefined && data.length === 0 && !isLoading && (
        <p className="py-6 text-center text-sm text-muted-foreground">No volumes reported.</p>
      )}

      <div className="flex flex-col items-center gap-3 border-t border-border/60 pt-6">
        <Button variant="outline" onClick={() => setFolderOpen((open) => !open)}>
          <FolderOpen aria-hidden />
          Scan a folder…
        </Button>
        {folderOpen && (
          <form
            className="flex w-full max-w-xl items-center gap-2"
            onSubmit={(event) => {
              event.preventDefault();
              const trimmed = folderPath.trim();
              if (trimmed.length > 0) onScan(trimmed);
            }}
          >
            <input
              value={folderPath}
              onChange={(event) => setFolderPath(event.currentTarget.value)}
              placeholder="/Users/you/Movies"
              aria-label="Folder to scan"
              spellCheck={false}
              autoComplete="off"
              className="h-9 flex-1 rounded-md border border-input bg-transparent px-3 font-mono text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring"
            />
            <Button type="submit" disabled={busy || folderPath.trim().length === 0}>
              Scan
            </Button>
          </form>
        )}
        {folderOpen && (
          <p className="max-w-xl text-center text-xs text-muted-foreground">
            A native folder picker is the intended path here — it is macOS&rsquo;s explicit-consent
            flow and often avoids needing Full Disk Access. It requires
            <code className="mx-1 font-mono">@tauri-apps/plugin-dialog</code>, which this build does
            not bundle yet.
          </p>
        )}
      </div>
    </div>
  );
}

interface VolumeCardProps {
  volume: VolumeRow;
  expanded: boolean;
  busy: boolean;
  onSelect: () => void;
  onScan: () => void;
}

function VolumeCard({ volume, expanded, busy, onSelect, onScan }: VolumeCardProps) {
  const segments = capacitySegments(
    volume.totalBytes,
    volume.availableBytes,
    volume.importantAvailableBytes,
  );

  return (
    <li className="overflow-hidden rounded-lg border border-border bg-card">
      <button
        type="button"
        onClick={onSelect}
        aria-expanded={expanded}
        className="flex w-full flex-col gap-2 px-4 py-3 text-left transition-colors hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
      >
        <div className="flex items-center gap-3">
          <HardDrive aria-hidden className="size-4 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate font-medium">{volume.name}</span>
          {volume.isRemovable && <Tag>removable</Tag>}
          {volume.hasLocalSnapshots && <Tag title="Local Time Machine snapshots are present. v1 does not claim their size.">snapshots</Tag>}
          <span className="rds-numeric shrink-0 text-sm text-muted-foreground">
            {formatSI(volume.totalBytes)}
          </span>
          <span className="rds-numeric w-14 shrink-0 text-right text-sm tabular-nums">
            {formatPercent(segments.used, segments.total)}
          </span>
          <span className="rds-numeric w-28 shrink-0 text-right text-sm text-muted-foreground">
            {formatSI(segments.available)} free
          </span>
        </div>
        <CapacityBar
          total={volume.totalBytes}
          available={volume.availableBytes}
          importantAvailable={volume.importantAvailableBytes}
        />
      </button>

      {expanded && (
        <div className="flex flex-col gap-3 border-t border-border/60 bg-background/40 px-4 py-3">
          <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-xs sm:grid-cols-3">
            <Field label="Mount point" value={volume.mountPoint} mono />
            <Field label="Filesystem" value={volume.fsType} />
            <Field label="Device" value={String(volume.device)} />
            <Field label="Capacity" value={formatSI(volume.totalBytes)} />
            <Field label="Used" value={formatSI(segments.used)} />
            <Field
              label="Purgeable"
              value={segments.purgeable > 0 ? formatSI(segments.purgeable) : "—"}
            />
          </dl>
          <p className="text-xs text-muted-foreground">
            Default exclusions apply; other filesystems are not crossed, and hard links are counted
            once. Protected directories may still refuse to open — those are reported as unreadable
            rather than silently skipped, and the totals become a floor.
          </p>
          <div className="flex justify-end">
            <Button onClick={onScan} disabled={busy}>
              {busy && <Loader aria-hidden className="animate-spin" />}
              Scan {volume.name}
            </Button>
          </div>
        </div>
      )}
    </li>
  );
}

function Tag({ children, title }: { children: React.ReactNode; title?: string }) {
  return (
    <span
      title={title}
      className="shrink-0 rounded-full border border-border px-2 py-0.5 text-[10px] uppercase tracking-wide text-muted-foreground"
    >
      {children}
    </span>
  );
}

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="min-w-0">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className={cn("truncate", mono === true && "font-mono")} data-selectable>
        {value}
      </dd>
    </div>
  );
}
