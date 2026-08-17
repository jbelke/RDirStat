/**
 * The frontend category table mirrors a Rust one by hand. This is the thing
 * that stops the two drifting.
 *
 * `src/lib/categories.ts` maps a `CategoryId` to a key, a label and a family;
 * `crates/rdirstat-classify/src/defaults.rs` is where those ids actually come
 * from. Nothing at runtime reconciles them — Rust sends a bare `u8` — so a
 * category appended on the Rust side is invisible here until someone notices
 * a tile painted in the "I have never heard of this index" colour.
 *
 * That is exactly what happened: the compiled table grew six macOS categories
 * (ids 19..24 — package, media-library, build-junk, cache, font, database) and
 * the frontend list stopped at 18, so `node_modules` and `DerivedData` painted
 * as an unlabelled `--cat-unknown` grey.
 *
 * So this test reads `defaults.rs` as text, pulls the `CategorySpec::new(...)`
 * calls out in source order, and asserts key-for-key equality with
 * `CATEGORIES`. Parsing Rust with a regex is ordinarily a bad idea; it is the
 * right one here because the alternative is no check at all — the table is not
 * exported through any interface this test could import, and the failure it
 * guards against is silent and cosmetic, which is the kind that survives.
 *
 * If this fails after you appended to the Rust table: append here too, in the
 * same order. Do not reorder either list — position IS the wire value.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { CATEGORIES, FAMILIES, categoryOf, familyKey } from "./categories.ts";

const DEFAULTS_RS = fileURLToPath(
  new URL("../../crates/rdirstat-classify/src/defaults.rs", import.meta.url),
);
const CATEGORIES_CSS = fileURLToPath(new URL("../styles/categories.css", import.meta.url));

/**
 * The `key` of every `CategorySpec::new(...)` in `defaults.rs`, in source
 * order. Position in the returned array is the `CategoryId`.
 *
 * Entry 0 is written `CategorySpec::new(UNCATEGORIZED_KEY, ...)` — a const
 * rather than a literal, because `CategoryId::UNCATEGORIZED == 0` is fixed by
 * the frozen contract and the Rust side refuses to spell it twice. It is
 * matched separately rather than by loosening the literal pattern, so a second
 * const-keyed category could never slip through unnoticed.
 */
function compiledKeys(source: string): string[] {
  const keys: string[] = [];
  const pattern = /CategorySpec::new\(\s*(UNCATEGORIZED_KEY|"([^"]+)")/g;
  for (const match of source.matchAll(pattern)) {
    keys.push(match[1] === "UNCATEGORIZED_KEY" ? "uncategorized" : match[2]);
  }
  return keys;
}

test("the frontend table matches the compiled table key-for-key", () => {
  const keys = compiledKeys(readFileSync(DEFAULTS_RS, "utf8"));

  // A regex that matched nothing would make every assertion below vacuously
  // pass, which is the one way this test could rot into a no-op.
  assert.ok(keys.length > 1, "parsed no categories out of defaults.rs — has the file moved?");

  assert.deepEqual(
    CATEGORIES.map((category) => category.key),
    keys,
    "src/lib/categories.ts and defaults.rs disagree; append to whichever is short",
  );
});

test("position in CATEGORIES is the CategoryId", () => {
  for (const [index, category] of CATEGORIES.entries()) {
    assert.equal(category.id, index, `${category.key} claims id ${category.id} at index ${index}`);
  }
});

test("every category resolves to a family the legend knows", () => {
  for (const category of CATEGORIES) {
    assert.ok(
      FAMILIES.includes(category.family),
      `${category.key} has family "${category.family}", which is not in FAMILIES`,
    );
  }
});

test("every category and family has a CSS token in every theme", () => {
  const css = readFileSync(CATEGORIES_CSS, "utf8");

  // Three blocks define the tokens: `:root`, the `prefers-color-scheme: dark`
  // media query, and `.dark`. A token added to only one of them produces a
  // category that is correct in light mode and invisible in dark, which no
  // amount of looking at one screenshot would catch.
  const expectedDefinitions = 3;

  for (const category of CATEGORIES) {
    const occurrences = css.split(`--cat-${category.key}:`).length - 1;
    assert.equal(
      occurrences,
      expectedDefinitions,
      `--cat-${category.key} is defined ${occurrences}x, expected ${expectedDefinitions} (:root, media, .dark)`,
    );
  }

  for (const family of FAMILIES) {
    const occurrences = css.split(`--fam-${familyKey(family)}:`).length - 1;
    assert.equal(
      occurrences,
      expectedDefinitions,
      `--fam-${familyKey(family)} is defined ${occurrences}x, expected ${expectedDefinitions}`,
    );
  }
});

test("an index beyond the table is explicit, never a wrong label", () => {
  const beyond = categoryOf(CATEGORIES.length + 5);
  assert.equal(beyond.key, "unknown");
  assert.match(beyond.label, /Category \d+/);
});
