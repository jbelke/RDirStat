/**
 * Volumes grouped by physical disk, then by container.
 *
 * On APFS, capacity belongs to the **container**, not the volume. Every volume
 * in a container reports the container's size and its free space, so a flat
 * list showed `Macintosh HD`, `VM`, `Preboot` and `Update` as four separate
 * 995 GB volumes each with 26.3 GB free — four rows, one disk's worth of data,
 * four wrong answers. So the shared numbers are stated once, on the container,
 * and each volume carries only `usedBytes`, which the backend reads per mount
 * rather than deriving.
 *
 * The grouping is by **physical disk** above that, because "are these the same
 * physical device" is the question a person actually has when they see six
 * volumes and one Mac. The tree is `disk → container → volumes`, and every
 * level degrades: a volume whose topology `diskutil` could not report still
 * appears, in a group of its own, rather than being dropped or silently merged
 * into someone else's disk.
 *
 * This lives here rather than in a component because two surfaces render the
 * same tree — the volume picker and the menu-bar panel — and the panel used to
 * carry its own container-only copy, which showed a multi-container disk as
 * several disks that happened to share a name.
 */

import type { VolumeRow } from "@/lib/ipc";

export interface ContainerGroup {
  /** `disk3`, or the mount point when the topology is unknown. */
  readonly id: string;
  /** Shared capacity, taken from any member — they all report the same. */
  readonly totalBytes: number;
  readonly availableBytes: number;
  readonly importantAvailableBytes: number | null;
  readonly volumes: readonly VolumeRow[];
}

export interface DiskGroup {
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
export function groupVolumes(volumes: readonly VolumeRow[]): DiskGroup[] {
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
      containers: [...containers]
        .map(([containerId, volumesInContainer]) => {
          // Any member reports the container's numbers; they are the same by
          // construction, and taking the largest is a defence against a member
          // that failed to report rather than a real disagreement.
          const total = Math.max(...volumesInContainer.map((volume) => volume.totalBytes));
          const owner =
            volumesInContainer.find((volume) => volume.totalBytes === total) ??
            volumesInContainer[0];
          return {
            id: containerId,
            totalBytes: total,
            availableBytes: owner?.availableBytes ?? 0,
            importantAvailableBytes: owner?.importantAvailableBytes ?? null,
            volumes: [...volumesInContainer].sort(byUsageThenName),
          };
        })
        // The container someone came to see first: user data over the sealed
        // system slices, then simply the bigger one.
        .sort((a, b) => {
          const aUser = a.volumes.some((volume) => !volume.isSystem);
          const bUser = b.volumes.some((volume) => !volume.isSystem);
          if (aUser !== bUser) return aUser ? -1 : 1;
          return b.totalBytes - a.totalBytes;
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
