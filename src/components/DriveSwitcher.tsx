/**
 * Switch which drive is being inventoried, from the titlebar.
 *
 * Without this, changing disks means navigating back to the Volumes route and
 * losing your place — which is a strange amount of ceremony for the question
 * "what about the other drive?", the single most common thing a person does
 * with a disk-usage tool once they have looked at one disk.
 *
 * **Picking a drive starts a new scan.** That is not a cheap toggle: it costs
 * minutes on a large volume and it discards the tree currently on screen, along
 * with the selection and the navigation stack. So the menu is honest about it
 * rather than presenting drives as tabs — the item for the current drive is
 * checked, every other item says it will rescan, and the whole control is
 * disabled while a scan is already running, because the app runs exactly one.
 *
 * The trigger shows the *volume name*, not the mount point: `/System/Volumes/
 * Data` is what the breadcrumb already says and is not what anyone calls their
 * disk. The mount point is still shown on each menu row, because two volumes
 * can share a name and only the path disambiguates them.
 */

import { ChevronsUpDown, HardDrive, Loader2 } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { formatSI } from "@/lib/format";
import type { VolumeRow } from "@/lib/ipc";
import { userVolumes, volumeForPath } from "@/lib/mounts";
import { cn } from "@/lib/utils";

export interface DriveSwitcherProps {
  volumes: readonly VolumeRow[];
  /** The scan root, used to work out which drive is on screen. */
  scanRootPath: string | null;
  /** True while a scan is in flight; the app runs exactly one at a time. */
  busy: boolean;
  /** Starts a scan of the chosen mount point. */
  onSelect: (mountPoint: string) => void;
  className?: string;
}

export function DriveSwitcher({
  volumes,
  scanRootPath,
  busy,
  onSelect,
  className,
}: DriveSwitcherProps) {
  const candidates = userVolumes(volumes);
  const current = volumeForPath(volumes, scanRootPath);

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

      <DropdownMenuContent align="start" className="min-w-72">
        <DropdownMenuLabel>Inventory a different drive</DropdownMenuLabel>
        <p className="px-2 pb-1.5 text-xs text-muted-foreground">
          Starts a new scan. The tree on screen is replaced.
        </p>
        <DropdownMenuSeparator />

        <DropdownMenuRadioGroup
          value={current?.mountPoint ?? ""}
          onValueChange={(mountPoint) => {
            // Re-selecting the drive already on screen would throw away a
            // finished scan to produce the same one. Nothing to do.
            if (mountPoint === current?.mountPoint) return;
            onSelect(mountPoint);
          }}
        >
          {[...groups.entries()].map(([disk, rows], index) => (
            <div key={disk}>
              {index > 0 && <DropdownMenuSeparator />}
              <DropdownMenuLabel className="text-[10px] uppercase tracking-wide">
                {disk}
              </DropdownMenuLabel>
              {rows.map((volume) => (
                <DropdownMenuRadioItem key={volume.mountPoint} value={volume.mountPoint}>
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className="truncate">{volume.name}</span>
                    <span className="truncate font-mono text-[10px] text-muted-foreground">
                      {volume.mountPoint}
                    </span>
                  </span>
                  <span className="shrink-0 text-right text-[10px] text-muted-foreground">
                    <span className="block">{formatSI(volume.usedBytes)} used</span>
                    {volume.fsType.toLowerCase() !== "apfs" && (
                      <span className="block uppercase">{volume.fsType}</span>
                    )}
                  </span>
                </DropdownMenuRadioItem>
              ))}
            </div>
          ))}
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
