/**
 * The disk → container → volume grouping that both the volume picker and the
 * menu-bar panel render.
 *
 * The failure this guards against is the one the panel actually shipped with:
 * it grouped by container, so a multi-container disk appeared as several disks
 * that happened to share a name. The invariants worth pinning are the
 * *identity* ones — what merges, what never merges, and which member's numbers
 * a container states — because a wrong merge is silent: every byte is still on
 * screen, just attributed to the wrong device.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { groupVolumes } from "./disks.ts";
import type { VolumeRow } from "./ipc.ts";

function makeVolume(overrides: Partial<VolumeRow>): VolumeRow {
  return {
    name: "Volume",
    mountPoint: "/Volumes/Volume",
    device: 1,
    fsType: "apfs",
    totalBytes: 1000,
    availableBytes: 400,
    usedBytes: 500,
    deviceNode: "/dev/disk3s1",
    containerId: "disk3",
    diskId: "disk0",
    diskName: "APPLE SSD",
    diskSizeBytes: 2000,
    isInternal: true,
    isSystem: false,
    importantAvailableBytes: null,
    isRootVolume: false,
    isRemovable: false,
    hasLocalSnapshots: false,
    ...overrides,
  };
}

test("a multi-container disk is ONE group, one bar per container", () => {
  const groups = groupVolumes([
    makeVolume({ name: "Data", containerId: "disk3", isRootVolume: true }),
    makeVolume({ name: "Preboot", containerId: "disk3", isSystem: true, usedBytes: 10 }),
    makeVolume({
      name: "Recovery",
      containerId: "disk1",
      isSystem: true,
      totalBytes: 500,
      availableBytes: 480,
      usedBytes: 20,
      mountPoint: "/System/Volumes/iSCPreboot",
    }),
  ]);

  assert.equal(groups.length, 1);
  assert.equal(groups[0]?.label, "APPLE SSD");
  assert.deepEqual(
    groups[0]?.containers.map((container) => container.id),
    // The user-data container leads; the sealed system slice follows.
    ["disk3", "disk1"],
  );
});

test("system volumes sort after user volumes within a container", () => {
  const [disk] = groupVolumes([
    makeVolume({ name: "VM", isSystem: true, usedBytes: 900 }),
    makeVolume({ name: "Data", usedBytes: 100 }),
  ]);
  assert.deepEqual(
    disk?.containers[0]?.volumes.map((volume) => volume.name),
    ["Data", "VM"],
  );
});

test("a container states its numbers once, from the largest-reporting member", () => {
  const [disk] = groupVolumes([
    makeVolume({ name: "Data", totalBytes: 995, availableBytes: 273 }),
    // A member that failed to report capacity must not drag the container down.
    makeVolume({ name: "Preboot", isSystem: true, totalBytes: 0, availableBytes: 0 }),
  ]);
  assert.equal(disk?.containers.length, 1);
  assert.equal(disk?.containers[0]?.totalBytes, 995);
  assert.equal(disk?.containers[0]?.availableBytes, 273);
});

test("unknown topology degrades to its own group, never a merge", () => {
  const groups = groupVolumes([
    makeVolume({
      name: "Mystery",
      diskId: null,
      diskName: null,
      containerId: null,
      mountPoint: "/Volumes/Mystery",
      isInternal: false,
    }),
    makeVolume({
      name: "AlsoMystery",
      diskId: null,
      diskName: null,
      containerId: null,
      mountPoint: "/Volumes/AlsoMystery",
      isInternal: false,
    }),
  ]);
  assert.equal(groups.length, 2);
});

test("boot disk first, then internal, then the biggest", () => {
  const groups = groupVolumes([
    makeVolume({
      name: "Big External",
      diskId: "disk5",
      diskName: "WD_BLACK",
      diskSizeBytes: 8_000,
      isInternal: false,
      containerId: "disk6",
      mountPoint: "/Volumes/tuf8tb",
    }),
    makeVolume({
      name: "Small External",
      diskId: "disk7",
      diskName: "PSSD",
      diskSizeBytes: 2_000,
      isInternal: false,
      containerId: "disk8",
      mountPoint: "/Volumes/NATO",
    }),
    makeVolume({ name: "Data", isRootVolume: true, diskSizeBytes: 1_000 }),
  ]);
  assert.deepEqual(
    groups.map((group) => group.label),
    ["APPLE SSD", "WD_BLACK", "PSSD"],
  );
});
