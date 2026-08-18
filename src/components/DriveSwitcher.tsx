/**
 * Choose which drive is on screen. Lives in the breadcrumb, between the app
 * name and the path — `RDirStat › [NATO ⌄] › /Volumes/NATO`.
 *
 * Without this, changing disks means navigating back to the Volumes route and
 * losing your place — which is a strange amount of ceremony for the question
 * "what about the other drive?", the single most common thing a person does
 * with a disk-usage tool once they have looked at one disk.
 *
 * **One click loads a drive; reading it is a different target.** The menu
 * used to treat the drive as the whole choice, and for a drive with no stored
 * snapshot that one click was a full disk read — minutes of it — which also
 * stopped whatever scan was already running. That is too much consequence to
 * hang on picking from a list.
 *
 * The fix is not to make everything cost two clicks. Restoring a stored
 * snapshot is instant and touches no disk, and making *that* a two-step reveal
 * charged the cheap path for the expensive one's sins. So:
 *
 *   - **Clicking the drive loads it.** With a stored snapshot it restores —
 *     near-instant, and the row says how old the snapshot is. Without one there
 *     is nothing to load, so the drive's path goes into the scan bar at the top
 *     instead, ready to run, and still nothing has been read.
 *   - **The ⟳ beside the row scans it.** A separate target, so a read can never
 *     be a mis-aimed click on the row, and it is the only item here that stops
 *     a scan in flight.
 *
 * The row is a `<div>` holding two menu items rather than one item containing a
 * button: a button cannot contain a button, and the scan control has to be
 * reachable on its own — by pointer and by keyboard.
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
  /**
   * Reads the disk. The expensive path, and the only one that stops a scan
   * already in flight — so it is never what clicking a row does.
   */
  onScan: (mountPoint: string) => void;
  /** Publishes a stored snapshot. The cheap path; touches no disk. */
  onRestore?: (mountPoint: string, device: number) => void;
  /**
   * Offers a drive to the scan bar without reading it.
   *
   * What clicking a drive with no stored snapshot does. There is nothing to put
   * on screen, so rather than doing nothing — or silently starting a multi-
   * minute read — the drive is handed to the control at the top that starts
   * scans, one keystroke from running.
   */
  onOfferToScanBar?: (mountPoint: string) => void;
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
  onScan,
  onRestore,
  onOfferToScanBar,
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
        // find Cancel in the status strip first. Opening this menu and picking
        // a row are both free now, so there is nothing to disable; the one item
        // that would stop the scan says so on itself.
        title="Choose a drive. Clicking one puts it on screen; the ⟳ beside it reads the disk."
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
          Click a drive to put it on screen — instant where there is a saved scan, and no disk is
          read either way. Use <RefreshCw aria-hidden className="inline size-3 align-[-2px]" /> to
          read the disk instead.
        </p>

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
              const canRestore = offer !== undefined && onRestore !== undefined && !isCurrent;
              return (
                // A row, not an item: the scan control has to be its own
                // target, and a menu item cannot contain a button.
                <div key={volume.mountPoint} className="flex items-center gap-0.5 pb-0.5">
                  <DropdownMenuItem
                    disabled={isCurrent}
                    onSelect={() => {
                      if (isCurrent) return;
                      if (canRestore && offer !== undefined && onRestore !== undefined) {
                        onRestore(volume.mountPoint, offer.device);
                      } else {
                        // Nothing stored to put on screen. Hand it to the scan
                        // bar rather than reading a whole disk on a single
                        // click the user may not have meant.
                        onOfferToScanBar?.(volume.mountPoint);
                      }
                    }}
                    className="min-w-0 flex-1 gap-2 py-1.5"
                  >
                    {isCurrent ? (
                      <Check aria-hidden className="text-foreground" />
                    ) : canRestore ? (
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
                      {/* What one click will do, stated on the thing that will
                        * do it. Restoring and "put it in the scan bar" are both
                        * free; neither reads the disk. */}
                      <span className="text-[10px] text-muted-foreground/80">
                        {isCurrent ? "" : canRestore ? `restores · ${age}` : "not scanned yet"}
                      </span>
                    </span>
                  </DropdownMenuItem>

                  {/* The expensive one, kept deliberately small and separate so
                    * it cannot be hit by a click aimed at the row. */}
                  <DropdownMenuItem
                    onSelect={() => onScan(volume.mountPoint)}
                    title={
                      busy
                        ? `Scan ${volume.name} — reads the disk, and stops the running scan`
                        : `Scan ${volume.name} — reads the disk`
                    }
                    className={cn(
                      "shrink-0 justify-center px-2 py-1.5",
                      busy && "text-pressure-warn",
                    )}
                  >
                    <RefreshCw aria-hidden className="size-3.5" />
                    <span className="sr-only">
                      {isCurrent ? `Rescan ${volume.name}` : `Scan ${volume.name}`}, reads the disk
                    </span>
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
