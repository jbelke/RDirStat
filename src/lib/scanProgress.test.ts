/**
 * Fixtures for the scan-progress arithmetic.
 *
 * Numbers are taken from a real scan of this machine so the expectations mean
 * something: 348 GB observed of a 967 GB volume, 225.2K directories read with
 * 116.5K still pending, 1.3M entries at 16 seconds.
 *
 * ---------------------------------------------------------------------------
 * HOW TO RUN — there is no test runner in this project
 * ---------------------------------------------------------------------------
 * Same as `format.test.ts`: `package.json` has no vitest/jest. These use
 * `node:test`, which needs no dependency, plus Node's built-in TypeScript
 * stripping (default on Node >= 22.18):
 *
 *     node --test src/lib/*.test.ts
 *
 * `scanProgress.ts` is dependency-free and DOM-free on purpose, which is why
 * the arithmetic lives there rather than in the component.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  coveragePercent,
  isVolumeRoot,
  scanCoverage,
  scanRampClass,
  volumeForRoot,
  type CoverageProgress,
  type CoverageVolume,
} from "./scanProgress.ts";

const MACINTOSH_HD: CoverageVolume = { mountPoint: "/", usedBytes: 967_000_000_000 };
const ARCHIVE: CoverageVolume = { mountPoint: "/Volumes/Archive", usedBytes: 135_000_000_000 };
const VOLUMES = [MACINTOSH_HD, ARCHIVE];

function progress(overrides: Partial<CoverageProgress> = {}): CoverageProgress {
  return {
    allocatedBytes: 348_000_000_000,
    observedEntries: 1_300_000,
    directories: 225_200,
    pendingDirs: 116_500,
    elapsedMs: 16_000,
    ...overrides,
  };
}

test("volumeForRoot prefers the most specific mount over the root filesystem", () => {
  // "/" is a prefix of every path, so a naive prefix match would always answer
  // "Macintosh HD" and every external volume's coverage would be measured
  // against the wrong disk.
  assert.equal(volumeForRoot(VOLUMES, "/Volumes/Archive"), ARCHIVE);
  assert.equal(volumeForRoot(VOLUMES, "/Volumes/Archive/projects"), ARCHIVE);
});

test("volumeForRoot falls back to the root filesystem for a path on no other volume", () => {
  assert.equal(volumeForRoot(VOLUMES, "/Users/josh/Downloads"), MACINTOSH_HD);
});

test("volumeForRoot does not match a sibling whose name merely starts the same way", () => {
  // "/Volumes/Archive-backup" must not resolve to the "/Volumes/Archive" volume.
  assert.equal(volumeForRoot(VOLUMES, "/Volumes/Archive-backup"), MACINTOSH_HD);
});

test("volumeForRoot has no answer without a root", () => {
  assert.equal(volumeForRoot(VOLUMES, null), null);
  assert.equal(volumeForRoot(VOLUMES, ""), null);
});

test("isVolumeRoot distinguishes a whole volume from a directory inside it", () => {
  assert.equal(isVolumeRoot(MACINTOSH_HD, "/"), true);
  assert.equal(isVolumeRoot(MACINTOSH_HD, "/Users/josh"), false);
  assert.equal(isVolumeRoot(ARCHIVE, "/Volumes/Archive"), true);
  assert.equal(isVolumeRoot(ARCHIVE, "/Volumes/Archive/projects"), false);
});

test("isVolumeRoot tolerates a trailing slash", () => {
  assert.equal(isVolumeRoot(ARCHIVE, "/Volumes/Archive/"), true);
});

test("a whole-volume scan measures bytes against the volume", () => {
  const coverage = scanCoverage(progress(), MACINTOSH_HD, "/");
  assert.equal(coverage.basis, "bytes");
  assert.equal(coverage.targetBytes, 967_000_000_000);
  assert.ok(
    Math.abs((coverage.fraction ?? 0) - 0.3599) < 0.001,
    `348 of 967 GB should read ~36%, got ${coverage.fraction}`,
  );
});

test("a subtree scan refuses the byte denominator", () => {
  // The whole point: "what fraction of a 967 GB disk is ~/Downloads" reports
  // 36% of the disk while claiming to describe the folder. Directory progress
  // is the honest answer for a subtree.
  const coverage = scanCoverage(progress(), MACINTOSH_HD, "/Users/josh/Downloads");
  assert.equal(coverage.basis, "directories");
  assert.equal(coverage.targetBytes, null);
  assert.ok(
    Math.abs((coverage.fraction ?? 0) - 0.659) < 0.01,
    `225.2K of 341.7K directories should read ~66%, got ${coverage.fraction}`,
  );
});

test("a numerator that overshoots its denominator is clamped", () => {
  const coverage = scanCoverage(progress({ allocatedBytes: 2e12 }), MACINTOSH_HD, "/");
  assert.equal(coverage.fraction, 1);
});

test("no denominator means no fraction, not a fabricated one", () => {
  const coverage = scanCoverage(progress({ directories: 0, pendingDirs: 0 }), null, null);
  assert.equal(coverage.fraction, null);
  assert.equal(coverage.basis, "unknown");
});

test("the rate is withheld until enough time has passed to mean anything", () => {
  // A sample over 40 ms reports tens of millions of entries per second and then
  // collapses, which reads as a regression rather than as a warm-up.
  assert.equal(scanCoverage(progress({ elapsedMs: 40 }), MACINTOSH_HD, "/").entriesPerSecond, null);
  const rate = scanCoverage(progress(), MACINTOSH_HD, "/").entriesPerSecond ?? 0;
  assert.ok(Math.abs(rate - 81_250) < 1, `1.3M entries in 16 s is ~81K/s, got ${rate}`);
});

test("a null progress payload does not throw", () => {
  const coverage = scanCoverage(null, MACINTOSH_HD, "/");
  assert.equal(coverage.fraction, null);
  assert.equal(coverage.observedBytes, 0);
});

test("a zero-sized volume is not divided by", () => {
  const empty: CoverageVolume = { mountPoint: "/Volumes/Empty", usedBytes: 0 };
  const coverage = scanCoverage(progress(), empty, "/Volumes/Empty");
  assert.ok(!Number.isNaN(coverage.fraction), "a zero denominator produced NaN");
  assert.equal(coverage.basis, "directories");
});

test("the ramp walks red to green as the scan travels", () => {
  assert.equal(scanRampClass(0), "bg-scan-start");
  assert.equal(scanRampClass(0.3), "bg-scan-early");
  assert.equal(scanRampClass(0.6), "bg-scan-mid");
  assert.equal(scanRampClass(0.9), "bg-scan-late");
});

test("the ramp never borrows the capacity-pressure tokens", () => {
  // Reusing `bg-pressure-critical` would make red mean both "this disk is
  // nearly full" and "this scan just started" on the same screen.
  for (const fraction of [null, 0, 0.25, 0.5, 0.75, 1]) {
    assert.ok(
      !scanRampClass(fraction).includes("pressure"),
      `scanRampClass(${fraction}) reused a capacity token`,
    );
  }
});

test("the percentage never claims 100% while the walk is still running", () => {
  assert.equal(coveragePercent(1), 99);
  assert.equal(coveragePercent(0.999), 99);
});

test("the percentage floors rather than rounds, so it never overstates", () => {
  assert.equal(coveragePercent(0.359), 35);
});

test("no fraction means no percentage to show", () => {
  assert.equal(coveragePercent(null), null);
});

// The bug that only appeared when the UI was driven: a scan of /Applications
// announced itself as "Scanning Macintosh HD", because the heading used
// `volumeForRoot` (which answers "where does this path live") as if it answered
// "is this path a volume". The two questions have different answers for every
// subtree scan, which is most of them.
test("a subtree scan is not a whole-volume scan just because it resolves to one", () => {
  const volume = volumeForRoot(VOLUMES, "/Applications");
  assert.equal(volume, MACINTOSH_HD, "the containing volume is still the right lookup");
  assert.equal(
    isVolumeRoot(volume, "/Applications"),
    false,
    "/Applications must not be mistaken for the volume it sits on",
  );
});
