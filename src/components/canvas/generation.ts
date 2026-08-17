/* =============================================================================
 * `TreeGeneration` normalisation.
 *
 * The Rust side is `#[serde(transparent)] pub struct TreeGeneration(u64)`, so
 * what actually reaches JavaScript depends on how `specta` was told to export
 * big integers: a `number`, a `bigint`, or a decimal `string`. Arrow schema
 * metadata is a string map, so the generation stamped into a batch is ALWAYS a
 * string. Comparing those two forms directly is how a stale batch slips through,
 * so everything is normalised to a decimal string exactly once, here.
 * ========================================================================== */

import type { Generation } from "./types.ts";

const DECIMAL = /^\d+$/;

/**
 * Canonicalise a generation to its decimal-string form.
 *
 * @throws RangeError when the value cannot be a `u64` (negative, fractional,
 *   beyond `Number.MAX_SAFE_INTEGER` as a `number`, or non-numeric text).
 */
export function normalizeGeneration(value: Generation): string {
  if (typeof value === "bigint") {
    if (value < 0n) throw new RangeError(`generation must be non-negative, got ${value}`);
    return value.toString(10);
  }
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new RangeError(`generation ${value} is not a safe non-negative integer`);
    }
    return value.toString(10);
  }
  // `TreeGeneration`'s Display is "gen#N"; accept it so a debug string pasted
  // into a prop fails loudly at the backend rather than silently mismatching.
  const text = value.trim().replace(/^gen#/, "");
  if (!DECIMAL.test(text)) throw new RangeError(`generation ${JSON.stringify(value)} is not a decimal integer`);
  // Strip leading zeros so "007" and "7" compare equal.
  const stripped = text.replace(/^0+(?=\d)/, "");
  return stripped;
}

/** `TreeGeneration::NONE` — no scan loaded. */
export const GENERATION_NONE = "0";

/** True when no scan has been loaded yet and a `layout` call would be pointless. */
export function isGenerationNone(value: Generation): boolean {
  return normalizeGeneration(value) === GENERATION_NONE;
}
