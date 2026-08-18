/**
 * Choose which drive is on screen. Lives in the breadcrumb, between the app
 * name and the path — `RDirStat › [NATO ⌄] › /Volumes/NATO`.
 *
 * Without this, changing disks means navigating back to the Volumes route and
 * losing your place — which is a strange amount of ceremony for the question
 * "what about the other drive?", the single most common thing a person does
 * with a disk-usage tool once they have looked at one disk.
 *
 * **Selecting a drive is not the same act as reading it.** The menu used to
 * treat the drive as the whole choice: one click put it on screen, and for a
 * drive with no stored snapshot that click was a full disk read — minutes of
 * it — and it stopped whatever scan was already running to get there. That is
 * a lot of consequence to hang on picking an item from a list, and it made the
 * cheap question ("what about the other drive?") indistinguishable from the
 * expensive one ("read that whole disk again").
 *
 * So the menu is two steps now. Clicking a drive **selects** it and nothing
 * else: the menu stays open, no disk is touched, no running scan is stopped,
 * and the tree on screen does not move. The selected drive then offers what
 * can be done with it, and those are the only things that act:
 *
 *   - **Put it on screen** — restores a stored snapshot. Near-instant, and it
 *     says how old the snapshot is, because a restore is not a fresh read.
 *   - **Scan** — reads the disk. Always available, always labelled with what
 *     it costs, and it is the only item that will stop a scan in flight.
 *
 * The consequence worth noticing is where the warning went. "A scan is running,
 * choosing a drive stops it first" used to sit at the top of the menu, because
 * with one-click-acts it was true of *opening* the menu. It now sits on the
 * scan item, because that is the only place it is true.
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

import { Check, ChevronsUpDown, HardDrive, History, Loader2, MonitorUp, RefreshCw } from "lucide-react";
import { useState } from "react";

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
   * already in flight — so it is never what merely selecting a drive does.
   */
  onScan: (mountPoint: string) => void;
  /** Publishes a stored snapshot. The cheap path; touches no disk. */
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
  onScan,
  onRestore,
  className,
}: DriveSwitcherProps) {
  const candidates = userVolumes(volumes);
  const current = volumeForPath(volumes, scanRootPath);

  /*
   * Which row is expanded, or `null` for "whatever is on screen".
   *
   * Held as an override rather than as the selection itself so the menu always
   * opens on the drive the user is actually looking at, without an effect to
   * keep the two in sync — a restore or a scan moves `current`, and this
   * resets to `null` on open, so the next open follows automatically.
   */
  const [picked, setPicked] = useState<string | null>(null);

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
    <DropdownMenu onOpenChange={(open) => open && setPicked(null)}>
      <DropdownMenuTrigger
        // Deliberately NOT disabled while a scan runs.
        //
        // It used to be, and that made the one thing a person wants during a
        // long scan of the wrong disk — go to the right one — unexpressible:
        // the control showed a spinner, took no clicks, and the way out was to
        // find Cancel in the status strip first. Opening this menu and picking
        // a row are both free now, so there is nothing to disable; the one item
        // that would stop the scan says so on itself.
        title="Choose a drive. Picking one shows what you can do with it; nothing is read until you choose."
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
          Pick a drive to see what you can do with it. Nothing is read, and nothing on screen
          changes, until you choose an action.
        </p>

        {[...groups.entries()].map(([disk, rows]) => (
          <div key={disk}>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-[10px] uppercase tracking-wide">
              {disk}
            </DropdownMenuLabel>

            {rows.map((volume) => {
              const isCurrent = volume.mountPoint === current?.mountPoint;
              const isPicked = (picked ?? current?.mountPoint ?? null) === volume.mountPoint;
              const offer = offerFor(volume);
              const age = describeAge(offer?.takenUnixMs ?? null, nowMs);
              // A stored snapshot is the cheap way onto the screen, but it is
              // now an *action*, not a side effect of picking the row.
              const canRestore = offer !== undefined && onRestore !== undefined && !isCurrent;
              return (
                <div key={volume.mountPoint} className="pb-1">
                  <DropdownMenuItem
                    // `preventDefault` keeps the menu open. This is the whole
                    // change: the click expresses "this one", and expressing
                    // that must not close the menu, move the tree, or touch a
                    // disk. Radix closes on select by default, which is right
                    // for an item that acts and wrong for one that chooses.
                    onSelect={(event) => {
                      event.preventDefault();
                      setPicked(volume.mountPoint);
                    }}
                    className={cn("gap-2 py-1.5", isPicked && "bg-accent")}
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
                      {/* What this drive HAS, not what clicking will do —
                        * because clicking no longer does anything. The cost of
                        * each action is stated on the action itself. */}
                      <span className="text-[10px] text-muted-foreground/80">
                        {canRestore ? `saved scan · ${age}` : isCurrent ? "" : "no saved scan"}
                      </span>
                    </span>
                  </DropdownMenuItem>

                  {/* Step two. Only these act. */}
                  {isPicked && (
                    <>
                      {canRestore && offer !== undefined && onRestore !== undefined && (
                        <DropdownMenuItem
                          onSelect={() => onRestore(volume.mountPoint, offer.device)}
                          className="pl-9"
                        >
                          <MonitorUp aria-hidden />
                          <span className="flex-1 text-xs">Put this drive on screen</span>
                          <span className="text-[10px] text-muted-foreground">
                            saved · {age}
                          </span>
                        </DropdownMenuItem>
                      )}
                      <DropdownMenuItem
                        onSelect={() => onScan(volume.mountPoint)}
                        className="pl-9"
                      >
                        <RefreshCw aria-hidden />
                        <span className="flex-1 text-xs">
                          {isCurrent ? "Rescan this drive" : "Scan this drive"}
                        </span>
                        {/* The warning belongs here and only here. Selecting a
                          * drive cannot stop a scan; this is the one item that
                          * can. */}
                        <span
                          className={cn(
                            "text-[10px]",
                            busy ? "text-pressure-warn" : "text-muted-foreground",
                          )}
                        >
                          {busy ? "reads the disk · stops the running scan" : "reads the disk"}
                        </span>
                      </DropdownMenuItem>
                    </>
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
