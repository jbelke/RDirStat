/**
 * Choose which drive is on screen. Lives in the breadcrumb, between the app
 * name and the path — `RDirStat › [NATO ⌄] › /Volumes/NATO`.
 *
 * Without this, changing disks means navigating back to the Volumes route and
 * losing your place — which is a strange amount of ceremony for the question
 * "what about the other drive?", the single most common thing a person does
 * with a disk-usage tool once they have looked at one disk.
 *
 * **The drive itself is the choice.** Picking a drive puts it on screen; the
 * row does not make the user choose a *mechanism* first. The mechanism is
 * still stated, because the two cost wildly different amounts and the
 * difference matters before the click, not after it:
 *
 *   - a drive with a stored snapshot restores it — near-instant, and the row
 *     says how old it is;
 *   - a drive without one has to be read, so the row says "reads the disk"
 *     and the click starts a scan of the drive it names.
 *
 * A *Rescan* item sits under any drive that would otherwise restore, so a
 * fresh read is never more than one extra click away, and the drive already on
 * screen offers only that.
 *
 * Switching replaces the tree along with the selection and the navigation
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

import { Check, ChevronsUpDown, HardDrive, History, Loader2, RefreshCw } from "lucide-react";

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
  /*
   * Matched on `device`, not on the mount-point string.
   *
   * The string match worked, but only by luck: `snapshot_offers` happens to
   * iterate `volumes::list()` and copy `volume.mount_point` through verbatim,
   * so both sides emit the same string because they come from the same call —
   * nothing enforces it. Build offers from the snapshot's own stored root
   * instead and `/` versus `/System/Volumes/Data` diverge, this returns
   * nothing, every drive quietly loses its restore option, and NOTHING errors.
   * A feature that disappears without complaining is worse than one that
   * breaks loudly.
   *
   * `st_dev` is the identity the snapshot store already keys on, so matching
   * it makes the join structural. The mount point stays what it should be: a
   * label to show the user, not a key.
   */
  const offerFor = (volume: VolumeRow): SnapshotOfferView | undefined =>
    offers.find((offer) => offer.hasSnapshot && offer.device === volume.device);

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
        // Deliberately NOT disabled while a scan runs.
        //
        // It used to be, and that made the one thing a person wants during a
        // long scan of the wrong disk — go to the right one — unexpressible:
        // the control showed a spinner, took no clicks, and the way out was to
        // find Cancel in the status strip first. Choosing a drive now stops the
        // running scan on the way, and the menu says so before anything is
        // clicked.
        title={
          busy
            ? "Choose which drive is on screen. A scan is running; choosing stops it."
            : "Choose which drive is on screen"
        }
        // Shaped like the crumbs it sits between — same padding, same muted
        // weight, same hover — because it *is* one level of the trail, not a
        // toolbar control that happens to be parked in it.
        className={cn(
          "flex shrink-0 items-center gap-1.5 rounded px-1.5 py-0.5 text-sm transition-colors",
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
        <span className="sr-only">Choose a drive</span>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" className="min-w-80">
        <DropdownMenuLabel>Choose a drive</DropdownMenuLabel>
        <p className="px-2 pb-1.5 text-xs text-muted-foreground">
          Puts that drive on screen, replacing the tree, the selection and the trail. A stored scan
          comes back instantly; a drive without one has to be read.
        </p>
        {/* Stated once, at the top, rather than on every row: the scan being
          * stopped is one fact about the app's state, not a property of any
          * particular drive. */}
        {busy && (
          <p className="px-2 pb-1.5 text-xs text-pressure-warn">
            A scan is running. Choosing a drive stops it first.
          </p>
        )}

        {[...groups.entries()].map(([disk, rows]) => (
          <div key={disk}>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-[10px] uppercase tracking-wide">
              {disk}
            </DropdownMenuLabel>

            {rows.map((volume) => {
              const isCurrent = volume.mountPoint === current?.mountPoint;
              const offer = offerFor(volume);
              const age = describeAge(offer?.takenUnixMs ?? null, nowMs);
              // Restoring is the cheap way to put a drive on screen, so it is
              // what choosing the drive does when a snapshot exists. Without
              // one there is nothing to show but what a read produces, and the
              // row says so before it is clicked.
              const restores = offer !== undefined && onRestore !== undefined && !isCurrent;
              return (
                <div key={volume.mountPoint} className="pb-1">
                  <DropdownMenuItem
                    // The drive on screen is not a destination. It stays
                    // listed, with a check, because a menu that hides where
                    // you already are makes you count to work out where you
                    // are.
                    disabled={isCurrent}
                    onSelect={() => {
                      if (isCurrent) return;
                      if (restores && offer !== undefined) onRestore(volume.mountPoint, offer.device);
                      else onSelect(volume.mountPoint);
                    }}
                    className="gap-2 py-1.5"
                  >
                    {isCurrent ? (
                      <Check aria-hidden className="text-foreground" />
                    ) : restores ? (
                      <History aria-hidden />
                    ) : (
                      <HardDrive aria-hidden />
                    )}
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate text-sm">
                        {volume.name}
                        {isCurrent && (
                          <span className="ml-1.5 text-[10px] text-muted-foreground">on screen</span>
                        )}
                      </span>
                      <span className="truncate text-[10px] text-muted-foreground">
                        {volume.mountPoint}
                      </span>
                    </span>
                    <span className="flex shrink-0 flex-col items-end">
                      <span className="text-[10px] text-muted-foreground">
                        {formatSI(volume.usedBytes)} used
                        {volume.fsType.toLowerCase() !== "apfs" && ` · ${volume.fsType.toUpperCase()}`}
                      </span>
                      {/* What clicking this row will cost, in the row itself.
                        * "restored from 25 min ago" and "reads the disk" are
                        * seconds versus minutes, and the difference has to be
                        * legible before the click rather than discovered after
                        * it. */}
                      <span className="text-[10px] text-muted-foreground/80">
                        {isCurrent ? "" : restores ? `restore · ${age}` : "scan · reads the disk"}
                      </span>
                    </span>
                  </DropdownMenuItem>

                  {/* The second, expensive answer. Offered under a drive that
                    * would otherwise restore — and under the current one,
                    * where it is the only thing left to do. */}
                  {(restores || isCurrent) && (
                    <DropdownMenuItem
                      onSelect={() => onSelect(volume.mountPoint)}
                      className="pl-9 text-muted-foreground"
                    >
                      <RefreshCw aria-hidden />
                      <span className="flex-1 text-xs">
                        {isCurrent ? "Rescan this drive" : "Scan instead of restoring"}
                      </span>
                      <span className="text-[10px]">reads the disk</span>
                    </DropdownMenuItem>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
