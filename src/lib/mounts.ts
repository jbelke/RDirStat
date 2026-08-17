/**
 * Which mounted volume a path belongs to.
 *
 * Two surfaces need this and were about to grow two copies of it: the relocate
 * dialog ranks the source's own volume last among destinations, and the drive
 * switcher has to show which drive is currently scanned. Both start from a path
 * and a volume list, and both get the answer wrong in the same way if they take
 * the naive route.
 */

import type { VolumeRow } from "@/lib/ipc";

/**
 * The volume whose mount point is the longest prefix of `path`.
 *
 * Longest wins because mounts nest: `/` is a prefix of literally every path, so
 * a path under `/Volumes/tuf8tb` has to prefer the deeper mount or every file
 * on every external disk would be reported as living on the boot volume.
 *
 * Matching is on whole path segments, not on raw string prefixes — otherwise
 * `/Volumes/Backup2/x` matches the mount `/Volumes/Backup` and the file is
 * attributed to the wrong disk. `/` is special-cased because it is the one
 * mount point that does not gain a separator when a child is appended.
 */
export function volumeForPath(
  volumes: readonly VolumeRow[],
  path: string | null,
): VolumeRow | null {
  if (path === null) return null;
  let best: VolumeRow | null = null;
  for (const volume of volumes) {
    const mount = volume.mountPoint;
    const matches =
      path === mount || mount === "/" || path.startsWith(mount.endsWith("/") ? mount : `${mount}/`);
    if (matches && (best === null || mount.length > best.mountPoint.length)) best = volume;
  }
  return best;
}

/**
 * Volumes worth offering as a scan target or a relocation destination.
 *
 * Drops the macOS-owned volumes (`Preboot`, `VM`, `Update`, the sealed system
 * snapshot) — scanning them tells the user nothing they can act on, and every
 * one of them is a row that pushes the disk they actually care about further
 * down the list.
 */
export function userVolumes(volumes: readonly VolumeRow[]): VolumeRow[] {
  return volumes.filter((volume) => !volume.isSystem);
}
