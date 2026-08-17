import assert from "node:assert/strict";
import { test } from "node:test";

import { decodeLayoutBatch } from "./arrow.ts";
import { SAMPLE_SUNBURST_ROWS, SAMPLE_TREEMAP_ROWS, buildLayoutIpc } from "./fixtures.ts";
import {
  NO_HIT,
  computeSunburstFrame,
  hitTest,
  hitTestRect,
  hitTestSunburst,
  indexOfNode,
  rankLargestTiles,
  rootMagnitude,
  tileMagnitude,
} from "./geometry.ts";
import { groupOwner, isRealNode, isValidNodeId, isVirtualGroup } from "./contract.ts";

const treemap = decodeLayoutBatch(buildLayoutIpc(SAMPLE_TREEMAP_ROWS), { generation: "1", root: 0, kind: "treemap" });
const sunburst = decodeLayoutBatch(buildLayoutIpc(SAMPLE_SUNBURST_ROWS), { generation: "1", root: 0, kind: "sunburst" });

test("the reverse scan returns the topmost tile, not the parent", () => {
  // (10, 10) is inside both the root (index 0) and the first child (index 1).
  assert.equal(hitTestRect(treemap, 10, 10), 1);
  assert.equal(hitTestRect(treemap, 70, 10), 2);
  assert.equal(hitTestRect(treemap, 70, 80), 3);
});

test("rectangle hit-testing is half-open on the far edges", () => {
  // The seam at x = 60 belongs to the tile that starts there, so adjacent tiles
  // never both claim a pixel.
  assert.equal(hitTestRect(treemap, 59.9, 50), 1);
  assert.equal(hitTestRect(treemap, 60, 50), 2);
  assert.equal(hitTestRect(treemap, 100, 50), NO_HIT);
  assert.equal(hitTestRect(treemap, -0.1, 50), NO_HIT);
});

test("a miss returns NO_HIT rather than 0", () => {
  assert.equal(hitTestRect(treemap, 500, 500), NO_HIT);
  assert.equal(hitTest(treemap, null, 500, 500), NO_HIT);
});

test("the sunburst frame normalises whatever units the backend sent", () => {
  const frame = computeSunburstFrame(sunburst, 200, 200);
  assert.equal(frame.cx, 100);
  assert.equal(frame.cy, 100);
  // Rows already span a full turn in radians, so the angle scale is ~1.
  assert.ok(Math.abs(frame.angleScale - 1) < 1e-6, `angleScale=${frame.angleScale}`);
  // Outermost radius is 40 units and the inscribed radius is 100px.
  assert.ok(Math.abs(frame.radiusScale - 2.5) < 1e-6, `radiusScale=${frame.radiusScale}`);
});

test("the same sunburst in turns instead of radians lands in the same place", () => {
  const turns = SAMPLE_SUNBURST_ROWS.map((row) => ({ ...row, x: row.x / (Math.PI * 2), w: row.w / (Math.PI * 2) }));
  const batch = decodeLayoutBatch(buildLayoutIpc(turns), { generation: "1", root: 0, kind: "sunburst" });
  const radiansFrame = computeSunburstFrame(sunburst, 200, 200);
  const turnsFrame = computeSunburstFrame(batch, 200, 200);
  for (const [px, py] of [
    [140, 120],
    [60, 140],
    [105, 100],
    [100, 30],
  ] as const) {
    assert.equal(
      hitTestSunburst(batch, turnsFrame, px, py),
      hitTestSunburst(sunburst, radiansFrame, px, py),
      `(${px}, ${py})`,
    );
  }
});

test("sunburst hit-testing respects the radius bands", () => {
  const frame = computeSunburstFrame(sunburst, 200, 200);
  // Dead centre is inside the innermost full ring (radius 0..50px).
  assert.equal(hitTestSunburst(sunburst, frame, 101, 100), 0);
  // The outer ring spans radius 50..100px, so 99px still hits it.
  assert.equal(hitTestSunburst(sunburst, frame, 199, 100), 1);
  // Past the outermost ring (radius > 100px) is a miss.
  assert.equal(hitTestSunburst(sunburst, frame, 205, 100), NO_HIT);
  // Beyond the inscribed circle is a miss even inside the bounding box.
  assert.equal(hitTestSunburst(sunburst, frame, 199, 199), NO_HIT);
});

test("sunburst wedges are found on both sides of the 3-o'clock seam", () => {
  const frame = computeSunburstFrame(sunburst, 200, 200);
  // Angle 0 is +x. Node 1 sweeps [0, PI): the lower half in screen coordinates.
  assert.equal(hitTestSunburst(sunburst, frame, 160, 105), 1);
  // Node 2 sweeps [PI, 1.5PI): upper-left.
  assert.equal(hitTestSunburst(sunburst, frame, 40, 95), 2);
  // Node 3 sweeps [1.5PI, 2PI): upper-right.
  assert.equal(hitTestSunburst(sunburst, frame, 160, 40), 3);
});

test("magnitude is area for rectangles and sweep for arcs", () => {
  assert.equal(tileMagnitude(treemap, 1), 60 * 100);
  assert.ok(Math.abs(tileMagnitude(sunburst, 1) - Math.PI) < 1e-5);
});

test("the drawn-root total sums the shallowest depth only", () => {
  // Depth 0 holds exactly the 100x100 root tile.
  assert.equal(rootMagnitude(treemap), 100 * 100);
});

test("rankLargestTiles is descending, capped, and shares sum sanely", () => {
  const ranked = rankLargestTiles(treemap, 3);
  assert.equal(ranked.length, 3);
  assert.deepEqual(
    ranked.map((item) => item.node),
    [0, 1, 2],
  );
  for (let i = 1; i < ranked.length; i += 1) {
    assert.ok(ranked[i - 1]!.magnitude >= ranked[i]!.magnitude);
  }
  assert.equal(ranked[0]!.share, 1);
  assert.ok(Math.abs(ranked[1]!.share - 0.6) < 1e-6);
});

test("rankLargestTiles copes with a limit above the tile count", () => {
  assert.equal(rankLargestTiles(treemap, 500).length, treemap.count);
});

test("indexOfNode finds a node and reports a miss", () => {
  assert.equal(indexOfNode(treemap, 3), 3);
  assert.equal(indexOfNode(treemap, 999), NO_HIT);
});

test("NodeId encoding is total and rejects the reserved value", () => {
  assert.ok(isRealNode(0));
  assert.ok(isRealNode(0x7fff_fffe));
  assert.ok(!isRealNode(0x7fff_ffff));
  assert.ok(!isVirtualGroup(0x7fff_ffff));
  assert.ok(isVirtualGroup(0x8000_0000));
  assert.ok(isVirtualGroup(0x8000_002a));
  assert.ok(!isVirtualGroup(0xffff_ffff));
  assert.ok(!isValidNodeId(0xffff_ffff));
  assert.ok(isValidNodeId(0x7fff_ffff));
  assert.equal(groupOwner(0x8000_002a), 42);
  assert.equal(groupOwner(42), null);
});
