/**
 * Scan-progress arithmetic, kept out of the component so it can be tested
 * without a DOM — same split as `capacity.ts`.
 *
 * ## Why there are two different progress measures
 *
 * A scan has no honest single percentage. The two candidates fail in different
 * places, so this module picks between them rather than averaging them into
 * something meaningless:
 *
 * - **Bytes covered.** `allocated` observed so far against the volume's
 *   `usedBytes`. Meaningful *only* when the scan root is the volume itself. Ask
 *   it about `~/Downloads` and it reports 2% forever, because the denominator is
 *   the whole disk.
 * - **Directories read.** `directories` against `directories + pendingDirs`.
 *   Always available and always about the actual work queue, but the denominator
 *   grows as the walk discovers more of the tree, so it is not monotonic — it
 *   can and does go backwards.
 *
 * So: bytes when the root is a volume, directories otherwise, and the label says
 * which one you are looking at. A progress bar that does not say what it is a
 * fraction *of* is decoration.
 *
 * ## Everything here is a floor
 *
 * `allocated` excludes what the default exclusions skipped and what permissions
 * refused, and counts hard-linked content once. So the numerator converges
 * *below* `usedBytes` on a real volume rather than to it — the bar reaching ~94%
 * and stopping is the correct outcome, not a stall. Nothing here is presented as
 * an estimate of time remaining, because none of these numbers support one.
 */

/** The shape this module needs from a `VolumeInfo`. Structural, so tests need no IPC. */
export interface CoverageVolume {
  readonly mountPoint: string;
  readonly usedBytes: number;
}

/** The shape this module needs from a `ScanProgressView`. */
export interface CoverageProgress {
  readonly allocatedBytes: number;
  readonly observedEntries: number;
  readonly directories: number;
  readonly pendingDirs: number;
  readonly elapsedMs: number;
}

/** Which denominator the fraction is against. Drives the label, so it is never implied. */
export type CoverageBasis = "bytes" | "directories" | "unknown";

export interface ScanCoverage {
  /** `[0, 1]`, clamped. `null` when nothing trustworthy can be computed yet. */
  readonly fraction: number | null;
  readonly basis: CoverageBasis;
  /** Entries per second over the scan so far. `null` until enough time has passed to mean anything. */
  readonly entriesPerSecond: number | null;
  /** Bytes seen so far. Always a floor. */
  readonly observedBytes: number;
  /** The byte denominator, when there is a legitimate one. */
  readonly targetBytes: number | null;
}

function clamp01(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return value < 0 ? 0 : value > 1 ? 1 : value;
}

/**
 * The volume a scan root belongs to, matched by longest mount-point prefix.
 *
 * Returns the volume only when it is a *containing* volume; whether the root IS
 * that volume is a separate question, answered by `isVolumeRoot`, because the
 * two have different consequences for the denominator.
 */
export function volumeForRoot<V extends CoverageVolume>(
  volumes: readonly V[],
  root: string | null,
): V | null {
  if (root === null || root.length === 0) return null;
  let best: V | null = null;
  for (const volume of volumes) {
    const mount = volume.mountPoint;
    if (mount.length === 0) continue;
    // `/` is a prefix of everything, so it is the fallback rather than a match
    // that beats a more specific mount.
    const matches = mount === "/" ? root.startsWith("/") : root === mount || root.startsWith(`${mount}/`);
    if (!matches) continue;
    if (best === null || mount.length > best.mountPoint.length) best = volume;
  }
  return best;
}

/** Whether `root` is a volume's mount point rather than a directory inside it. */
export function isVolumeRoot(volume: CoverageVolume | null, root: string | null): boolean {
  if (volume === null || root === null) return false;
  // Trailing-slash tolerant: "/" and "/Volumes/NATO/" both name their mount.
  const strip = (path: string) => (path.length > 1 && path.endsWith("/") ? path.slice(0, -1) : path);
  return strip(root) === strip(volume.mountPoint);
}

/**
 * Rate is meaningless over a few milliseconds — an early sample reports
 * millions of entries per second and then collapses, which reads as a
 * regression rather than as a warm-up.
 */
const MIN_MS_FOR_RATE = 1500;

export function scanCoverage(
  progress: CoverageProgress | null,
  volume: CoverageVolume | null,
  root: string | null,
): ScanCoverage {
  if (progress === null) {
    return {
      fraction: null,
      basis: "unknown",
      entriesPerSecond: null,
      observedBytes: 0,
      targetBytes: null,
    };
  }

  const entriesPerSecond =
    progress.elapsedMs >= MIN_MS_FOR_RATE
      ? (progress.observedEntries * 1000) / progress.elapsedMs
      : null;

  // Bytes, but only against a denominator that is actually about this scan.
  const wholeVolume = isVolumeRoot(volume, root);
  if (wholeVolume && volume !== null && volume.usedBytes > 0) {
    return {
      fraction: clamp01(progress.allocatedBytes / volume.usedBytes),
      basis: "bytes",
      entriesPerSecond,
      observedBytes: progress.allocatedBytes,
      targetBytes: volume.usedBytes,
    };
  }

  // Otherwise the work queue. Known-but-unfinished directories are the only
  // denominator available for a subtree scan.
  const known = progress.directories + progress.pendingDirs;
  return {
    fraction: known > 0 ? clamp01(progress.directories / known) : null,
    basis: known > 0 ? "directories" : "unknown",
    entriesPerSecond,
    observedBytes: progress.allocatedBytes,
    targetBytes: null,
  };
}

/**
 * Four steps, red through green, because this bar is read as *travel* rather
 * than as a threshold being crossed.
 *
 * Deliberately NOT `pressureClass`'s tokens: on the same screen a red capacity
 * bar means "this disk is nearly full", and a scan that has just started is not
 * a problem. Two opposite meanings for one colour a few pixels apart is how a
 * status colour stops being read at all.
 */
export function scanRampClass(fraction: number | null): string {
  if (fraction === null) return "bg-scan-start";
  if (fraction >= 0.75) return "bg-scan-late";
  if (fraction >= 0.5) return "bg-scan-mid";
  if (fraction >= 0.25) return "bg-scan-early";
  return "bg-scan-start";
}

/**
 * The percentage to display, or `null` to display none.
 *
 * Capped below 100 while the scan is still running: the walk is by definition
 * unfinished, and a bar that reads "100%" for the last thirty seconds of a
 * five-minute scan trains the user to distrust it.
 */
export function coveragePercent(fraction: number | null): number | null {
  if (fraction === null) return null;
  return Math.min(99, Math.floor(fraction * 100));
}
