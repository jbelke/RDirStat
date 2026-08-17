/* =============================================================================
 * Invariants for the reference layouts, plus the evidence behind "a reverse
 * linear scan is fast enough and a quadtree is premature".
 * ========================================================================== */

import assert from "node:assert/strict";
import { test } from "node:test";

import { decodeLayoutBatch } from "./arrow.ts";
import { buildFakeTree, icicleRows, sunburstRows, treeDepth, treemapRows } from "./devFixtures.ts";
import { buildLayoutIpc } from "./fixtures.ts";
import { NO_HIT, hitTestRect, rankLargestTiles } from "./geometry.ts";

const tree = buildFakeTree(20_260_817, 10, 3);
const depth = treeDepth(tree);

test("the fake tree is deterministic across builds", () => {
  const again = buildFakeTree(20_260_817, 10, 3);
  assert.equal(again.size, tree.size);
  assert.equal(again.children.length, tree.children.length);
});

test("treemap tiles stay inside their parent and inside the viewport", () => {
  const rows = treemapRows(tree, 800, 600, 3);
  assert.ok(rows.length > 50, `expected a substantial tile count, got ${rows.length}`);
  for (const row of rows) {
    assert.ok(row.x >= -0.01 && row.y >= -0.01, `tile ${row.node} starts outside the viewport`);
    assert.ok(row.x + row.w <= 800.01, `tile ${row.node} overflows on x`);
    assert.ok(row.y + row.h <= 600.01, `tile ${row.node} overflows on y`);
    assert.ok(row.w >= 3 && row.h >= 3, `tile ${row.node} is below the min_px cutoff`);
  }
});

test("treemap siblings do not overlap", () => {
  const rows = treemapRows(tree, 800, 600, 3);
  const byDepth = new Map<number, typeof rows>();
  for (const row of rows) {
    const bucket = byDepth.get(row.depth) ?? [];
    bucket.push(row);
    byDepth.set(row.depth, bucket);
  }
  // Depth 1 is the root's direct children: the strictest non-overlap case.
  const level = byDepth.get(1) ?? [];
  assert.ok(level.length > 1);
  for (let i = 0; i < level.length; i += 1) {
    for (let j = i + 1; j < level.length; j += 1) {
      const a = level[i];
      const b = level[j];
      if (a === undefined || b === undefined) continue;
      const disjoint = a.x + a.w <= b.x + 0.01 || b.x + b.w <= a.x + 0.01 || a.y + a.h <= b.y + 0.01 || b.y + b.h <= a.y + 0.01;
      assert.ok(disjoint, `tiles ${a.node} and ${b.node} overlap`);
    }
  }
});

test("parents are emitted before their children so the reverse scan finds the child", () => {
  const rows = treemapRows(tree, 800, 600, 3);
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "treemap" });
  const seen = new Set<number>();
  for (let i = 0; i < batch.count; i += 1) {
    if (batch.depth[i] > 0) assert.ok(seen.size > 0);
    seen.add(batch.node[i]);
  }
  // A point inside a deep tile must resolve to that tile, not to the root.
  const deepest = rows.reduce((best, row) => (row.depth > best.depth ? row : best), rows[0]!);
  const hit = hitTestRect(batch, deepest.x + deepest.w / 2, deepest.y + deepest.h / 2);
  assert.notEqual(hit, NO_HIT);
  assert.ok(batch.depth[hit] >= deepest.depth, "the reverse scan returned an ancestor");
});

test("icicle rows fill each depth band without exceeding the viewport", () => {
  const rows = icicleRows(tree, 800, 600, depth, 1);
  const rowHeight = 600 / (depth + 1);
  for (const row of rows) {
    assert.ok(Math.abs(row.h - rowHeight) < 1e-6);
    assert.equal(row.y, row.depth * rowHeight);
    assert.ok(row.x + row.w <= 800.01);
  }
});

test("sunburst rows cover exactly one turn at the root", () => {
  const rows = sunburstRows(tree, depth, 300, 0);
  const root = rows[0]!;
  assert.equal(root.x, 0);
  assert.ok(Math.abs(root.w - Math.PI * 2) < 1e-9);
  const firstRing = rows.filter((row) => row.depth === 1);
  const covered = firstRing.reduce((sum, row) => sum + row.w, 0);
  assert.ok(Math.abs(covered - Math.PI * 2) < 1e-6, `ring 1 covers ${covered}`);
});

test("hit-testing thousands of tiles is microseconds, so a quadtree is premature", () => {
  const rows = treemapRows(buildFakeTree(99, 16, 5), 1600, 1000, 1);
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "treemap" });
  assert.ok(batch.count >= 2000, `expected a realistic tile count, got ${batch.count}`);

  const probes = 2000;
  const started = performance.now();
  let hits = 0;
  for (let i = 0; i < probes; i += 1) {
    const px = ((i * 37) % 1600) + 0.5;
    const py = ((i * 53) % 1000) + 0.5;
    if (hitTestRect(batch, px, py) !== NO_HIT) hits += 1;
  }
  const perProbeUs = ((performance.now() - started) / probes) * 1000;

  assert.ok(hits > 0, "the probe grid never hit a tile, so the timing is meaningless");
  // Generous bound: this is a regression guard, not a published benchmark.
  assert.ok(perProbeUs < 500, `hit test took ${perProbeUs.toFixed(1)}us per probe over ${batch.count} tiles`);
  console.log(`      hit-test: ${perProbeUs.toFixed(1)}us/probe over ${batch.count} tiles`);
});

test("the accessible list is bounded regardless of tile count", () => {
  const rows = treemapRows(buildFakeTree(7, 16, 5), 1600, 1000, 1);
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "treemap" });
  assert.equal(rankLargestTiles(batch, 25).length, 25);
});
