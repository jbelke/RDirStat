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
 * WHY THIS IS GROUPED, AND WHERE EACH NUMBER COMES FROM
 * ---------------------------------------------------------------------------
 * On APFS, capacity belongs to the **container**, not the volume. Every volume
 * in a container reports the container's size and its free space, so a flat
 * list showed `Macintosh HD`, `VM`, `Preboot` and `Update` as four separate
 * 995 GB volumes each with 26.3 GB free and — because the old row derived usage
 * as `total - available` — each with 968 GB used. Four rows, one disk's worth
 * of data, four wrong answers.
 *
 * So the shared numbers are stated once, on the container, and each volume row
 * carries only `usedBytes`, which the backend reads per mount rather than
 * deriving. The grouping is by **physical disk** above that, because "are these
 * the same physical device" is the question a person actually has when they see
 * six volumes and one Mac.
 *
 * The tree is `disk → container → volumes`, and every level degrades: a volume
 * whose topology `diskutil` could not report still appears, in a group of its
 * own, rather than being dropped or silently merged into someone else's disk.
 *
 * ---------------------------------------------------------------------------
 * KNOWN GAP — "Scan a folder…" is not an NSOpenPanel
 * ---------------------------------------------------------------------------
 * docs/05 wants the secondary action to open a native folder picker, because a
 * folder pick is macOS's explicit-consent path and often avoids needing a broad
 * Full Disk Access grant. That needs `@tauri-apps/plugin-dialog`, which is not
 * in this project's dependency set yet.
 *
 * Rather than ship a dead button, the action expands a path field that feeds
 * the same `scan_start`. It works, and it is honestly labelled as the fallback
 * it is. Swapping in `open({ directory: true })` is a three-line change here
 * once the plugin lands.
 */

import { ChevronDown, ChevronRight, FolderOpen, HardDrive, Loader, RefreshCw, ShieldAlert } from "lucide-react";
import { useMemo, useState } from "react";

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

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

interface ContainerGroup {
  /** `disk3`, or the mount point when the topology is unknown. */
  readonly id: string;
  /** Shared capacity, taken from any member — they all report the same. */
  readonly totalBytes: number;
  readonly availableBytes: number;
  readonly importantAvailableBytes: number | null;
  readonly volumes: readonly VolumeRow[];
}

interface DiskGroup {
  readonly id: string;
  readonly label: string;
  readonly sizeBytes: number | null;
  readonly isInternal: boolean;
  readonly isRemovable: boolean;
  /** True when any member is the boot volume. Sorts this group first. */
  readonly hasRootVolume: boolean;
  readonly containers: readonly ContainerGroup[];
}

/**
 * Volumes, grouped by physical disk and then by container.
 *
 * Every key falls back rather than merging: an unknown `diskId` groups under
 * the container, an unknown `containerId` groups under the mount point. Two
 * volumes never end up in the same group unless macOS said they share
 * something.
 */
function groupVolumes(volumes: readonly VolumeRow[]): DiskGroup[] {
  const disks = new Map<string, VolumeRow[]>();
  for (const volume of volumes) {
    const key = volume.diskId ?? volume.containerId ?? volume.mountPoint;
    const bucket = disks.get(key);
    if (bucket === undefined) disks.set(key, [volume]);
    else bucket.push(volume);
  }

  const groups: DiskGroup[] = [];
  for (const [id, members] of disks) {
    const containers = new Map<string, VolumeRow[]>();
    for (const volume of members) {
      const key = volume.containerId ?? volume.mountPoint;
      const bucket = containers.get(key);
      if (bucket === undefined) containers.set(key, [volume]);
      else bucket.push(volume);
    }

    const first = members[0];
    groups.push({
      id,
      label: first?.diskName ?? (first?.diskId !== null && first?.diskId !== undefined ? `Disk ${first.diskId}` : (first?.name ?? id)),
      sizeBytes: first?.diskSizeBytes ?? null,
      isInternal: first?.isInternal ?? false,
      isRemovable: members.some((volume) => volume.isRemovable),
      hasRootVolume: members.some((volume) => volume.isRootVolume),
      containers: [...containers].map(([containerId, volumesInContainer]) => {
        // Any member reports the container's numbers; they are the same by
        // construction, and taking the largest is a defence against a member
        // that failed to report rather than a real disagreement.
        const total = Math.max(...volumesInContainer.map((volume) => volume.totalBytes));
        const owner =
          volumesInContainer.find((volume) => volume.totalBytes === total) ?? volumesInContainer[0];
        return {
          id: containerId,
          totalBytes: total,
          availableBytes: owner?.availableBytes ?? 0,
          importantAvailableBytes: owner?.importantAvailableBytes ?? null,
          volumes: [...volumesInContainer].sort(byUsageThenName),
        };
      }),
    });
  }

  // Boot disk first, then internal disks, then the biggest.
  return groups.sort((a, b) => {
    if (a.hasRootVolume !== b.hasRootVolume) return a.hasRootVolume ? -1 : 1;
    if (a.isInternal !== b.isInternal) return a.isInternal ? -1 : 1;
    return (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0);
  });
}

function byUsageThenName(a: VolumeRow, b: VolumeRow): number {
  if (a.isSystem !== b.isSystem) return a.isSystem ? 1 : -1;
  if (a.usedBytes !== b.usedBytes) return b.usedBytes - a.usedBytes;
  return a.name.localeCompare(b.name);
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

export function VolumePicker({ onScan, busy = false }: VolumePickerProps) {
  const { data, error, isLoading, refetch, isFetching } = useVolumes();
  const [preflight, setPreflight] = useState<string | null>(null);
  const [folderPath, setFolderPath] = useState("");
  const [folderOpen, setFolderOpen] = useState(false);

  const disks = useMemo(() => groupVolumes(data ?? []), [data]);

  return (
    // `min-h-0` + `overflow-y-auto`: this screen is the scroll container. A Mac
    // with a dozen mounts is ordinary, and the picker used to clip everything
    // below the fold with no way to reach it.
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex w-full max-w-3xl flex-col gap-6 px-8 py-10">
        <div className="flex items-baseline justify-between">
          <div>
            <h1 className="text-lg font-medium tracking-tight">Choose a volume to inventory</h1>
            <p className="text-sm text-muted-foreground">
              Sizes are decimal SI, matching Finder. Volumes are grouped by the disk they sit on:
              on APFS the capacity and free space belong to the container, and only the used figure
              is the volume&rsquo;s own.
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

        <div className="flex flex-col gap-5">
          {disks.map((disk) => (
            <DiskCard
              key={disk.id}
              disk={disk}
              busy={busy}
              preflight={preflight}
              onSelect={(mountPoint) =>
                setPreflight((current) => (current === mountPoint ? null : mountPoint))
              }
              onScan={onScan}
            />
          ))}
        </div>

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
    </div>
  );
}

// ---------------------------------------------------------------------------
// One physical disk
// ---------------------------------------------------------------------------

interface DiskCardProps {
  disk: DiskGroup;
  busy: boolean;
  preflight: string | null;
  onSelect: (mountPoint: string) => void;
  onScan: (root: string) => void;
}

function DiskCard({ disk, busy, preflight, onSelect, onScan }: DiskCardProps) {
  return (
    <section className="flex flex-col gap-2">
      <header className="flex items-baseline gap-2 px-1">
        <HardDrive aria-hidden className="size-4 shrink-0 translate-y-0.5 text-muted-foreground" />
        <h2 className="min-w-0 truncate text-sm font-medium">{disk.label}</h2>
        {disk.sizeBytes !== null && (
          <span className="rds-numeric shrink-0 text-xs text-muted-foreground">
            {formatSI(disk.sizeBytes)}
          </span>
        )}
        <Tag>{disk.isInternal ? "internal" : "external"}</Tag>
        {disk.isRemovable && <Tag>removable</Tag>}
        {disk.containers.length > 1 && (
          <span className="text-[10px] text-muted-foreground">
            {disk.containers.length} containers
          </span>
        )}
      </header>

      {disk.containers.map((container) => (
        <ContainerCard
          key={container.id}
          container={container}
          busy={busy}
          preflight={preflight}
          onSelect={onSelect}
          onScan={onScan}
        />
      ))}
    </section>
  );
}

// ---------------------------------------------------------------------------
// One container: the level that actually owns the capacity
// ---------------------------------------------------------------------------

interface ContainerCardProps {
  container: ContainerGroup;
  busy: boolean;
  preflight: string | null;
  onSelect: (mountPoint: string) => void;
  onScan: (root: string) => void;
}

function ContainerCard({ container, busy, preflight, onSelect, onScan }: ContainerCardProps) {
  const [systemOpen, setSystemOpen] = useState(false);
  const segments = capacitySegments(
    container.totalBytes,
    container.availableBytes,
    container.importantAvailableBytes,
  );

  const userVolumes = container.volumes.filter((volume) => !volume.isSystem);
  const systemVolumes = container.volumes.filter((volume) => volume.isSystem);
  const shared = container.volumes.length > 1;

  return (
    <div className="overflow-hidden rounded-lg border border-border bg-card">
      <div className="flex flex-col gap-2 px-4 py-3">
        <div className="flex items-baseline gap-3 text-sm">
          <span className="rds-numeric shrink-0 font-medium">{formatSI(container.totalBytes)}</span>
          <span className="rds-numeric shrink-0 tabular-nums">
            {formatPercent(segments.used, segments.total)}
          </span>
          <span className="rds-numeric min-w-0 flex-1 truncate text-muted-foreground">
            {formatSI(segments.available)} free
          </span>
          {shared && (
            <span className="shrink-0 text-[10px] text-muted-foreground">
              shared by {container.volumes.length} volumes
            </span>
          )}
        </div>
        <CapacityBar
          total={container.totalBytes}
          available={container.availableBytes}
          importantAvailable={container.importantAvailableBytes}
        />
      </div>

      <ul className="border-t border-border/60">
        {userVolumes.map((volume) => (
          <VolumeRowItem
            key={`${volume.device}:${volume.mountPoint}`}
            volume={volume}
            container={container}
            expanded={preflight === volume.mountPoint}
            busy={busy}
            onSelect={() => onSelect(volume.mountPoint)}
            onScan={() => onScan(volume.mountPoint)}
          />
        ))}
      </ul>

      {systemVolumes.length > 0 && (
        <div className="border-t border-border/60">
          <button
            type="button"
            aria-expanded={systemOpen}
            onClick={() => setSystemOpen((open) => !open)}
            className="flex w-full items-center gap-1.5 px-4 py-2 text-left text-[11px] text-muted-foreground hover:bg-accent/40"
          >
            {systemOpen ? (
              <ChevronDown aria-hidden className="size-3" />
            ) : (
              <ChevronRight aria-hidden className="size-3" />
            )}
            {systemVolumes.length} system volume{systemVolumes.length === 1 ? "" : "s"} — macOS owns
            these
          </button>
          {systemOpen && (
            <ul>
              {systemVolumes.map((volume) => (
                <VolumeRowItem
                  key={`${volume.device}:${volume.mountPoint}`}
                  volume={volume}
                  container={container}
                  expanded={preflight === volume.mountPoint}
                  busy={busy}
                  onSelect={() => onSelect(volume.mountPoint)}
                  onScan={() => onScan(volume.mountPoint)}
                />
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// One volume
// ---------------------------------------------------------------------------

interface VolumeRowItemProps {
  volume: VolumeRow;
  container: ContainerGroup;
  expanded: boolean;
  busy: boolean;
  onSelect: () => void;
  onScan: () => void;
}

function VolumeRowItem({ volume, container, expanded, busy, onSelect, onScan }: VolumeRowItemProps) {
  return (
    <li className="border-b border-border/40 last:border-b-0">
      <button
        type="button"
        onClick={onSelect}
        aria-expanded={expanded}
        className="flex w-full items-baseline gap-3 px-4 py-2 text-left transition-colors hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
      >
        {expanded ? (
          <ChevronDown aria-hidden className="size-3 shrink-0 translate-y-0.5 text-muted-foreground" />
        ) : (
          <ChevronRight aria-hidden className="size-3 shrink-0 translate-y-0.5 text-muted-foreground" />
        )}
        <span className="min-w-0 flex-1 truncate text-sm">{volume.name}</span>
        {volume.isRootVolume && <Tag title="The volume macOS booted from">boot</Tag>}
        {/* The filesystem, on the row rather than only inside the preflight,
          * whenever it is not the one everything else on this Mac uses. It is
          * the fact that decides what a volume can *hold*: exFAT and MS-DOS
          * store no extended attributes or ACLs, so a copy onto one loses them
          * silently, and network mounts have their own rules. Reading "exfat"
          * before picking beats discovering it afterwards. */}
        {volume.fsType !== "apfs" && (
          <Tag title={fsTypeHint(volume.fsType)}>{volume.fsType}</Tag>
        )}
        {volume.hasLocalSnapshots && (
          <Tag title="Local Time Machine snapshots are present. v1 does not claim their size.">
            snapshots
          </Tag>
        )}
        {/* The one number that belongs to this volume alone. */}
        <span className="rds-numeric w-28 shrink-0 text-right text-sm text-muted-foreground">
          {formatSI(volume.usedBytes)} used
        </span>
      </button>

      {expanded && (
        <div className="flex flex-col gap-3 bg-background/40 px-4 py-3">
          <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-xs sm:grid-cols-3">
            <Field label="Mount point" value={volume.mountPoint} mono />
            <Field label="Device" value={volume.deviceNode} mono />
            <Field label="Filesystem" value={volume.fsType} />
            <Field label="This volume uses" value={formatSI(volume.usedBytes)} />
            <Field
              label="Container capacity"
              value={`${formatSI(container.totalBytes)} · ${formatSI(container.availableBytes)} free`}
            />
            <Field label="Container" value={volume.containerId ?? "—"} mono />
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

/**
 * What a non-APFS filesystem means for the data put on it.
 *
 * Only the properties that change what a volume can hold, stated for the
 * filesystems macOS actually mounts. Anything unrecognised gets the neutral
 * line rather than a guess: a wrong reassurance about metadata is worse than
 * no reassurance.
 */
function fsTypeHint(fsType: string): string {
  switch (fsType) {
    case "exfat":
    case "msdos":
      return `${fsType}: no extended attributes, ACLs, or POSIX ownership. Files copied here lose that metadata silently.`;
    case "hfs":
      return "HFS+: extended attributes and ACLs are supported, but it is not the format macOS installs on any more.";
    case "smbfs":
    case "afpfs":
    case "nfs":
    case "webdav":
      return `${fsType}: a network mount. Metadata support depends on the server, and it can disappear mid-scan.`;
    default:
      return `Filesystem type reported by the mount: ${fsType}.`;
  }
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
