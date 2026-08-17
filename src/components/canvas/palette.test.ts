/**
 * The fill table, and in particular what the category filter does to it.
 *
 * These run under `node --test --experimental-strip-types` with no DOM, which
 * is exactly the path `resolvePalette` is written to survive: `document` and
 * `getComputedStyle` are both absent, so every colour comes from the built-in
 * fallback table rather than from a theme token. That makes the assertions
 * below about *structure* — which index dims, which does not, and that a
 * filtered entry is never simply dropped — rather than about specific hexes,
 * which belong to `categories.css`.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { CATEGORIES } from "../../lib/categories.ts";
import { legendEntries, resolvePalette } from "./palette.ts";

test("with no filter every category paints its own colour", () => {
  const palette = resolvePalette(null, "category", null);
  assert.equal(palette.fills.length, 256);
  // Distinct categories must not collapse onto one string.
  assert.notEqual(palette.fills[11], palette.fills[22]);
});

test("a filter dims every category outside it and leaves the rest alone", () => {
  const unfiltered = resolvePalette(null, "category", null);
  const filtered = resolvePalette(null, "category", new Set([11]));

  // 11 is Video — the one the user selected in the report that prompted this.
  assert.equal(filtered.fills[11], unfiltered.fills[11], "the kept category must be untouched");
  assert.notEqual(filtered.fills[22], unfiltered.fills[22], "an excluded category must change");
});

test("a dimmed category is still painted, never blank or transparent", () => {
  // The excluded tiles are the context that makes the included ones legible, so
  // "dim" must mean pushed back, not erased. An empty string would leave the
  // previous fillStyle in place and paint the wrong category's colour.
  const filtered = resolvePalette(null, "category", new Set([11]));
  for (const category of CATEGORIES) {
    const fill = filtered.fills[category.id];
    assert.ok(typeof fill === "string" && fill.length > 0, `category ${category.id} has no fill`);
    assert.notEqual(fill, "transparent");
  }
});

test("filtering under family mode dims by family, not by category", () => {
  // Media is a family; 11 (Video) is one of its members. Filtering to the
  // family must leave every Media category at full strength.
  const media = CATEGORIES.filter((category) => category.family === "Media").map((c) => c.id);
  const unfiltered = resolvePalette(null, "family", null);
  const filtered = resolvePalette(null, "family", new Set(media));

  for (const id of media) {
    assert.equal(filtered.fills[id], unfiltered.fills[id], `Media member ${id} must stay lit`);
  }
  const outsider = CATEGORIES.find((category) => category.family !== "Media");
  assert.ok(outsider !== undefined);
  assert.notEqual(filtered.fills[outsider.id], unfiltered.fills[outsider.id]);
});

test("a legend row carries the category ids it stands for", () => {
  // This is the contract with the backend: rows expand to category ids here, so
  // nothing downstream ever needs to know families exist.
  const byCategory = legendEntries("category", null);
  for (const entry of byCategory) {
    assert.equal(entry.categoryIds.length, 1, `${entry.label} should stand for exactly one id`);
  }

  const byFamily = legendEntries("family", null);
  const covered = byFamily.flatMap((entry) => entry.categoryIds).sort((a, b) => a - b);
  assert.deepEqual(
    covered,
    CATEGORIES.map((category) => category.id),
    "the families between them must cover every category exactly once",
  );
});
