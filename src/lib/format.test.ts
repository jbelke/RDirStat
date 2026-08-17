/**
 * Fixtures for the byte formatter.
 *
 * Every expectation below is copied from the frozen contract's own doc comment
 * for `rdirstat_core::format_si` / `format_iec` / `format_percent`. That is the
 * point: these two implementations must agree digit for digit, because the
 * Rust one formats the CLI's output and this one formats the window's, and a
 * user comparing them is entitled to see the same string.
 *
 * ---------------------------------------------------------------------------
 * HOW TO RUN — there is no test runner in this project
 * ---------------------------------------------------------------------------
 * `package.json` has no vitest/jest, and adding one is outside this agent's
 * ownership. These use `node:test`, which needs no dependency, and Node's
 * built-in TypeScript stripping (default on Node >= 22.18):
 *
 *     node --test src/lib/*.test.ts
 *
 * Every module under test here is dependency-free on purpose, so stripping is
 * sufficient and no bundler is involved. Component tests would need a DOM and a
 * real runner; that is a named gap, not an oversight.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { capacitySegments, pressureClass } from "./capacity.ts";
import { categoryColorVar, categoryOf } from "./categories.ts";
import { formatCount, formatDuration, formatIEC, formatMtime, formatPercent, formatSI, shareOf } from "./format.ts";
import { FLAGS, hasFlag, INCOMPLETE_SUBTREE, isRealNode, isVirtualGroup, num } from "./wire.ts";

test("format_si matches the contract's documented fixtures", () => {
  assert.equal(formatSI(0), "0 B");
  assert.equal(formatSI(999), "999 B");
  assert.equal(formatSI(1000), "1.00 kB");
  assert.equal(formatSI(2_410_000_000), "2.41 GB");
  assert.equal(formatSI(926_000_000_000), "926 GB");
  assert.equal(formatSI(1_000_000_000_000), "1.00 TB");
});

test("format_si picks decimals from the UNROUNDED integer part", () => {
  // <10 -> 2 decimals, <100 -> 1, else 0.
  assert.equal(formatSI(9_990_000_000), "9.99 GB");
  assert.equal(formatSI(99_900_000_000), "99.9 GB");
  assert.equal(formatSI(999_000_000_000), "999 GB");
});

test("format_si carries the unit rather than printing four digits", () => {
  // The contract calls this one out by name: 999.6 GB must become 1.00 TB, and
  // must never render as "1000 GB".
  assert.equal(formatSI(999_600_000_000), "1.00 TB");
  assert.equal(formatSI(999_999), "1.00 MB");
});

test("format_si rounds half away from zero", () => {
  // 1_005 / 1000 = 1.005 kB; half-away-from-zero gives 1.01, banker's would give 1.00.
  assert.equal(formatSI(1_005), "1.01 kB");
  assert.equal(formatSI(1_004), "1.00 kB");
});

test("format_iec is binary and always labelled", () => {
  assert.equal(formatIEC(1024), "1.00 KiB");
  assert.equal(formatIEC(5 * 1024 ** 3), "5.00 GiB");
  assert.equal(formatIEC(1023), "1023 B");
});

test("SI and IEC disagree, which is exactly why the unit is always printed", () => {
  assert.notEqual(formatSI(1024), formatIEC(1024));
  assert.equal(formatSI(1024), "1.02 kB");
});

test("no float ever touches a byte count", () => {
  // 2^53 + 1 is not representable as an f64, so a float implementation cannot
  // produce this string. BigInt math can.
  assert.equal(formatSI(9_007_199_254_740_993n), "9.01 PB");
});

test("format_percent is one decimal, and a zero denominator is 0.0%", () => {
  assert.equal(formatPercent(1, 3), "33.3%");
  assert.equal(formatPercent(0, 0), "0.0%");
  assert.equal(formatPercent(5, 0), "0.0%");
  assert.equal(formatPercent(1, 1), "100.0%");
});

test("shareOf clamps, because a partial subtree can exceed its parent's floor", () => {
  assert.equal(shareOf(50, 100), 0.5);
  assert.equal(shareOf(150, 100), 1);
  assert.equal(shareOf(1, 0), 0);
  assert.equal(shareOf(-1, 100), 0);
});

test("mtime is signed seconds and the sentinel renders as a dash", () => {
  // i64::MIN is DirTotals' "nothing observed" sentinel.
  assert.equal(formatMtime(-9_223_372_036_854_775_808n as unknown as number), "—");
  assert.equal(formatMtime(Number.NaN), "—");
  // A pre-1970 timestamp is real data on a real disk, not corruption.
  assert.notEqual(formatMtime(-86_400), "—");
});

test("duration formats as m:ss and h:mm:ss", () => {
  assert.equal(formatDuration(41_000), "0:41");
  assert.equal(formatDuration(61_000), "1:01");
  assert.equal(formatDuration(3_661_000), "1:01:01");
  assert.equal(formatDuration(-1), "0:00");
});

test("counts render compactly", () => {
  assert.equal(formatCount(0), "0");
  assert.equal(formatCount(12_000_000), "12M");
});

test("capacity segments never sum past the total", () => {
  // The Macintosh HD figures from docs/05-UI.md: 995 GB total, 26 GB available,
  // 39 GB purgeable inside capacity.
  const total = 995_000_000_000;
  const available = 26_000_000_000;
  const important = 65_000_000_000; // available + purgeable

  const segments = capacitySegments(total, available, important);
  assert.equal(segments.available, available);
  assert.equal(segments.purgeable, 39_000_000_000);
  assert.equal(segments.used, 930_000_000_000);
  assert.equal(segments.used + segments.purgeable + segments.available, total);
});

test("capacity invents no purgeable segment when the platform supplies none", () => {
  const segments = capacitySegments(1000, 250, null);
  assert.equal(segments.purgeable, 0);
  assert.equal(segments.used, 750);
  assert.equal(segments.used + segments.available, 1000);
});

test("capacity survives nonsense input without producing a bar past 100%", () => {
  const zero = capacitySegments(0, 100, 50);
  assert.deepEqual(zero, { total: 0, used: 0, purgeable: 0, available: 0, pressure: 0 });

  // available > total, and important < available: both are impossible, both
  // must clamp rather than render a negative segment.
  const clamped = capacitySegments(1000, 5000, 10);
  assert.equal(clamped.available, 1000);
  assert.equal(clamped.purgeable, 0);
  assert.equal(clamped.used, 0);
  assert.ok(clamped.pressure >= 0 && clamped.pressure <= 1);
});

test("pressure is three thresholds, not a gradient", () => {
  assert.equal(pressureClass(0), "bg-pressure-ok");
  assert.equal(pressureClass(0.749), "bg-pressure-ok");
  assert.equal(pressureClass(0.75), "bg-pressure-warn");
  assert.equal(pressureClass(0.899), "bg-pressure-warn");
  assert.equal(pressureClass(0.9), "bg-pressure-critical");
});

test("category 0 is Uncategorized and unknown indices fail visibly", () => {
  assert.equal(categoryOf(0).key, "uncategorized");
  assert.equal(categoryOf(11).key, "video");
  assert.equal(categoryColorVar(11), "var(--cat-video)");

  const unknown = categoryOf(200);
  assert.equal(unknown.key, "unknown");
  assert.equal(unknown.label, "Category 200");
  // Distinct from Uncategorized on purpose: this is version skew, not a decision.
  assert.notEqual(categoryColorVar(200), categoryColorVar(0));
});

test("NodeId encoding: NONE is 0x7FFFFFFF, not u32::MAX", () => {
  assert.equal(isRealNode(0), true);
  assert.equal(isRealNode(0x7fff_fffe), true);
  assert.equal(isRealNode(0x7fff_ffff), false, "NodeId::NONE is not a slot");
  assert.equal(isVirtualGroup(0x8000_0000), true);
  assert.equal(isVirtualGroup(0x8000_0005), true);
  assert.equal(isVirtualGroup(0xffff_ffff), false, "0xFFFFFFFF is reserved and rejected");
  assert.equal(isVirtualGroup(5), false);
});

test("INCOMPLETE_SUBTREE is the four flags that make a total a floor", () => {
  assert.equal(
    INCOMPLETE_SUBTREE,
    FLAGS.UNREADABLE | FLAGS.EXCLUDED | FLAGS.INCOMPLETE | FLAGS.AGGREGATED,
  );
  assert.equal(hasFlag(FLAGS.UNREADABLE, INCOMPLETE_SUBTREE), true);
  assert.equal(hasFlag(FLAGS.SPARSE, INCOMPLETE_SUBTREE), false);
  assert.equal(hasFlag(FLAGS.SPARSE | FLAGS.EXCLUDED, INCOMPLETE_SUBTREE), true);
});

test("u64 normalisation accepts every BigIntExportBehavior", () => {
  assert.equal(num(42), 42);
  assert.equal(num(42n), 42);
  assert.equal(num("42"), 42);
  assert.equal(num(null), 0);
  assert.equal(num(undefined), 0);
  assert.equal(num("not a number"), 0, "junk must not become NaN on screen");
  assert.equal(num(Number.NaN), 0);
});
