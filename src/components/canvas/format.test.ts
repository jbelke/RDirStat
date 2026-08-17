import assert from "node:assert/strict";
import { test } from "node:test";

import { formatIec, formatPercent, formatShare, formatSi } from "./format.ts";
import { normalizeGeneration } from "./generation.ts";

/* The formatter itself is `src/lib/format.ts`'s and is tested there. These are
 * the cross-checks that keep the canvas from drifting away from it: if the
 * shared module ever stops matching `rdirstat_core::format_si`, the tooltip and
 * the details panel would disagree about the same file, and that is a bug the
 * canvas owner needs to notice too. The vectors are copied from the core
 * contract's doc comment. */

test("the shared formatter still matches the core contract's examples", () => {
  assert.equal(formatSi(0), "0 B");
  assert.equal(formatSi(999), "999 B");
  assert.equal(formatSi(1000), "1.00 kB");
  assert.equal(formatSi(2_410_000_000), "2.41 GB");
  assert.equal(formatSi(926_000_000_000), "926 GB");
  assert.equal(formatSi(1_000_000_000_000), "1.00 TB");
});

test("rounding still carries the unit rather than printing 1000 GB", () => {
  assert.equal(formatSi(999_600_000_000), "1.00 TB");
  assert.equal(formatSi(999_400_000_000), "999 GB");
});

test("IEC and percent still behave", () => {
  assert.equal(formatIec(1024), "1.00 KiB");
  assert.equal(formatIec(5 * 1024 ** 3), "5.00 GiB");
  assert.equal(formatPercent(1, 3), "33.3%");
  assert.equal(formatPercent(1, 0), "0.0%");
});

test("formatShare renders a float share", () => {
  assert.equal(formatShare(1), "100.0%");
  assert.equal(formatShare(0.6), "60.0%");
  assert.equal(formatShare(0.3333), "33.3%");
  assert.equal(formatShare(0), "0.0%");
  assert.equal(formatShare(Number.NaN), "0.0%");
  assert.equal(formatShare(Number.NEGATIVE_INFINITY), "0.0%");
});

test("normalizeGeneration accepts every wire form the u64 newtype can take", () => {
  assert.equal(normalizeGeneration(7), "7");
  assert.equal(normalizeGeneration(7n), "7");
  assert.equal(normalizeGeneration("7"), "7");
  assert.equal(normalizeGeneration("007"), "7");
  assert.equal(normalizeGeneration(" gen#7 "), "7");
  assert.equal(normalizeGeneration("18446744073709551615"), "18446744073709551615");
});

test("normalizeGeneration refuses values a u64 cannot hold", () => {
  assert.throws(() => normalizeGeneration(-1), RangeError);
  assert.throws(() => normalizeGeneration(1.5), RangeError);
  assert.throws(() => normalizeGeneration(-1n), RangeError);
  assert.throws(() => normalizeGeneration("abc"), RangeError);
  assert.throws(() => normalizeGeneration(""), RangeError);
});
