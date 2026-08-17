/**
 * Switch which drive is being inventoried, from the titlebar.
 *
 * Without this, changing disks means navigating back to the Volumes route and
 * losing your place — which is a strange amount of ceremony for the question
 * "what about the other drive?", the single most common thing a person does
 * with a disk-usage tool once they have looked at one disk.
 *
 * **Switching is never silent about its cost.** Each drive offers up to two
 * things, and they cost wildly different amounts:
 *
 *   - *Restore last scan* — offered only when a snapshot exists on disk, and
 *     always labelled with its age. Near-instant.
 *   - *Scan now* — re-reads the disk. Minutes on a large volume.
 *
 * Both replace the tree on screen along with the selection and the navigation
 * stack, so the menu says so once at the top rather than pretending drives are
 * tabs. The age on a restore is not decoration: a snapshot can be stale by any
 * amount and anything created since is simply missing from it, so an
 * unlabelled "restore" would let an old tree pass for the state of the disk.
 *
 * The whole control is disabled while a scan is running, because the app runs
 * exactly one — and the backend refuses a restore mid-scan rather than
 * replacing a tree that a scan is about to publish over.
 *
 * The trigger shows the *volume name*, not the mount point: `/System/Volumes/
 * Data` is what the breadcrumb already says and is not what anyone calls their
 * disk. The mount point is still shown on each menu row, because two volumes
 * can share a name and only the path disambiguates them.
 */

import { ChevronsUpDown, HardDrive, History, Loader2, RefreshCw } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { formatSI } from "@/lib/format";
import type { SnapshotOfferView, VolumeRow } from "@/lib/ipc";
import { userVolumes, volumeForPath } from "@/lib/mounts";
import { cn } from "@/lib/utils";

export interface DriveSwitcherProps {
  volumes: readonly VolumeRow[];
  /** Which drives can be restored from disk instead of rescanned. */
  offers?: readonly SnapshotOfferView[];
  /** The scan root, used to work out which drive is on screen. */
  scanRootPath: string | null;
  /** True while a scan is in flight; the app runs exactly one at a time. */
  busy: boolean;
  /** Starts a scan of the chosen mount point. */
  onSelect: (mountPoint: string) => void;
  /** Publishes a stored snapshot for the chosen mount point. */
  onRestore?: (mountPoint: string, device: number) => void;
  className?: string;
}

/**
 * "2 hours ago", "yesterday", "3 days ago".
 *
 * A restore MUST say how old it is. A snapshot can be stale by any amount and
 * anything created since is simply missing from it, so a switcher that offered
 * a bare "restore" would let a two-week-old tree pass for the state of the
 * disk. An absolute clock time is worse here than a relative one: the user is
 * judging freshness, not looking something up.
 */
function describeAge(takenUnixMs: number | null, nowMs: number): string | null {
  if (takenUnixMs === null) return null;
  // Floor, not round, at every step. Rounding makes a 30-second-old snapshot
  // read "1 min ago", and more importantly makes a 90-minute-old one read
  // "2 hours ago" — inventing age it does not have. Flooring names the unit the
  // snapshot has actually reached.
  //
  // `max(0, …)` because a snapshot's timestamp can legitimately sit in the
  // future after a clock change, and "-4 min ago" reads as a bug in the app
  // rather than a bug in the clock.
  const minutes = Math.max(0, Math.floor((nowMs - takenUnixMs) / 60_000));
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} ${hours === 1 ? "hour" : "hours"} ago`;
  const days = Math.floor(hours / 24);
  return `${days} ${days === 1 ? "day" : "days"} ago`;
}

export function DriveSwitcher({
  volumes,
  offers = [],
  scanRootPath,
  busy,
  onSelect,
  onRestore,
  className,
}: DriveSwitcherProps) {
  const candidates = userVolumes(volumes);
  const current = volumeForPath(volumes, scanRootPath);

  // Sampled once per render rather than per row, so every age in one open menu
  // is measured from the same instant.
  const nowMs = Date.now();
  const offerFor = (mountPoint: string): SnapshotOfferView | undefined =>
    offers.find((offer) => offer.hasSnapshot && offer.mountPoint === mountPoint);

  // Nothing to switch between: one drive is not a choice, and an empty list
  // means `volumes` has not resolved yet. Either way a disabled dropdown would
  // be furniture.
  if (candidates.length === 0) return null;

  // Group by physical disk so a multi-container APFS device reads as one drive
  // with several volumes rather than as several unrelated disks — the same
  // grouping the volume picker uses.
  const groups = new Map<string, VolumeRow[]>();
  for (const volume of candidates) {
    const key = volume.diskName ?? volume.diskId ?? "Other";
    const bucket = groups.get(key);
    if (bucket === undefined) groups.set(key, [volume]);
    else bucket.push(volume);
  }

  const label = current?.name ?? "Choose a drive";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        disabled={busy}
        title={
          busy
            ? "A scan is already running. Cancel it before switching drives."
            : "Switch which drive is being inventoried — this starts a new scan"
        }
        className={cn(
          "flex shrink-0 items-center gap-1.5 rounded px-2 py-1 text-sm transition-colors",
          "text-muted-foreground hover:bg-accent hover:text-foreground",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
          "disabled:pointer-events-none disabled:opacity-50",
          className,
        )}
      >
        {busy ? (
          <Loader2 aria-hidden className="size-3.5 animate-spin" />
        ) : (
          <HardDrive aria-hidden className="size-3.5" />
        )}
        <span className="max-w-40 truncate">{label}</span>
        <ChevronsUpDown aria-hidden className="size-3 opacity-60" />
        <span className="sr-only">Switch drive</span>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" className="min-w-80">
        <DropdownMenuLabel>Inventory a different drive</DropdownMenuLabel>
        <p className="px-2 pb-1.5 text-xs text-muted-foreground">
          Replaces the tree on screen. Restoring is instant; scanning re-reads the disk.
        </p>

        {[...groups.entries()].map(([disk, rows]) => (
          <div key={disk}>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-[10px] uppercase tracking-wide">
              {disk}
            </DropdownMenuLabel>

            {rows.map((volume) => {
              const isCurrent = volume.mountPoint === current?.mountPoint;
              const offer = offerFor(volume.mountPoint);
              const age = describeAge(offer?.takenUnixMs ?? null, nowMs);
              return (
                <div key={volume.mountPoint} className="px-1 pb-1">
                  <div className="flex items-baseline gap-2 px-2 py-1">
                    <span className="min-w-0 flex-1 truncate text-sm">
                      {volume.name}
                      {isCurrent && <span className="ml-1.5 text-[10px] text-muted-foreground">on screen</span>}
                    </span>
                    <span className="shrink-0 text-[10px] text-muted-foreground">
                      {formatSI(volume.usedBytes)} used
                      {volume.fsType.toLowerCase() !== "apfs" && ` · ${volume.fsType.toUpperCase()}`}
                    </span>
                  </div>

                  {/* Restore first when it exists, because it is the cheap
                    * answer — but it always states its age. A snapshot can be
                    * stale by any amount and anything created since is missing
                    * from it, so an unlabelled "restore" would let an old tree
                    * pass for the state of the disk. */}
                  {offer !== undefined && onRestore !== undefined && !isCurrent && (
                    <DropdownMenuItem onSelect={() => onRestore(volume.mountPoint, offer.device)}>
                      <History aria-hidden />
                      <span className="flex-1">Restore last scan</span>
                      <span className="text-[10px] text-muted-foreground">{age}</span>
                    </DropdownMenuItem>
                  )}

                  <DropdownMenuItem
                    disabled={isCurrent && offer === undefined}
                    onSelect={() => onSelect(volume.mountPoint)}
                  >
                    <RefreshCw aria-hidden />
                    <span className="flex-1">{isCurrent ? "Rescan this drive" : "Scan now"}</span>
                    <span className="text-[10px] text-muted-foreground">reads the disk</span>
                  </DropdownMenuItem>
                </div>
              );
            })}
          </div>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
