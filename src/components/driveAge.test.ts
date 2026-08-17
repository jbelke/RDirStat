import assert from "node:assert/strict";
import { test } from "node:test";

/**
 * The snapshot-age label the drive switcher puts on a restore.
 *
 * Duplicated from `DriveSwitcher.tsx` rather than imported: the component is
 * JSX and `node --test` has no bundler, and this project has no test runner
 * that would give it one. The duplication is deliberate and small; if the two
 * drift, the behaviour under test is the one documented here.
 *
 * Worth testing at all because the label is a SAFETY claim, not decoration. A
 * restore replaces the tree on screen with one read off disk, and a snapshot
 * can be stale by any amount — anything created since is simply missing. An
 * age that reads fresher than reality is the one failure mode that matters.
 */
function describeAge(takenUnixMs: number | null, nowMs: number): string | null {
  if (takenUnixMs === null) return null;
  const minutes = Math.max(0, Math.floor((nowMs - takenUnixMs) / 60_000));
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} ${hours === 1 ? "hour" : "hours"} ago`;
  const days = Math.floor(hours / 24);
  return `${days} ${days === 1 ? "day" : "days"} ago`;
}

const NOW = 1_800_000_000_000;

test("no snapshot has no age", () => {
  assert.equal(describeAge(null, NOW), null);
});

test("ages read the way a person judges freshness", () => {
  assert.equal(describeAge(NOW - 30_000, NOW), "just now");
  assert.equal(describeAge(NOW - 5 * 60_000, NOW), "5 min ago");
  assert.equal(describeAge(NOW - 2 * 3_600_000, NOW), "2 hours ago");
  // 90 minutes is one hour old, not two. Rounding would invent age.
  assert.equal(describeAge(NOW - 90 * 60_000, NOW), "1 hour ago");
  assert.equal(describeAge(NOW - 3_600_000, NOW), "1 hour ago");
  assert.equal(describeAge(NOW - 3 * 86_400_000, NOW), "3 days ago");
  assert.equal(describeAge(NOW - 86_400_000, NOW), "1 day ago");
});

test("a clock skewed into the future never reads as negative", () => {
  // Snapshot mtime can legitimately be ahead of `Date.now()` after a clock
  // change; "-4 min ago" would look like a bug in the app rather than the clock.
  assert.equal(describeAge(NOW + 4 * 60_000, NOW), "just now");
});

test("a two-week-old snapshot says so rather than rounding away", () => {
  // The whole point of the label: an old tree must not pass for the disk's
  // current state.
  assert.equal(describeAge(NOW - 14 * 86_400_000, NOW), "14 days ago");
});
