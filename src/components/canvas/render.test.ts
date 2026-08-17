/* =============================================================================
 * Paint tests against a recording 2D context.
 *
 * Node has no canvas, but `drawLayout` only ever talks to the
 * `CanvasRenderingContext2D` interface, so a recorder proves the parts that
 * actually matter: what is filled, in what order, with which colour, what is
 * culled, and that `fillStyle` is not reassigned per tile.
 * ========================================================================== */

import assert from "node:assert/strict";
import { test } from "node:test";

import { decodeLayoutBatch } from "./arrow.ts";
import { SAMPLE_SUNBURST_ROWS, SAMPLE_TREEMAP_ROWS, buildLayoutIpc } from "./fixtures.ts";
import { computeSunburstFrame } from "./geometry.ts";
import type { Palette } from "./palette.ts";
import { drawLayout } from "./render.ts";

interface Call {
  readonly op: string;
  readonly args: readonly unknown[];
}

class RecordingContext {
  readonly calls: Call[] = [];
  #fillStyle: string = "#000000";
  strokeStyle = "#000000";
  lineWidth = 1;

  get fillStyle(): string {
    return this.#fillStyle;
  }

  set fillStyle(value: string) {
    this.#fillStyle = value;
    this.calls.push({ op: "fillStyle", args: [value] });
  }

  setTransform(...args: number[]): void {
    this.calls.push({ op: "setTransform", args });
  }
  fillRect(...args: number[]): void {
    this.calls.push({ op: "fillRect", args: [...args, this.#fillStyle] });
  }
  rect(...args: number[]): void {
    this.calls.push({ op: "rect", args });
  }
  beginPath(): void {
    this.calls.push({ op: "beginPath", args: [] });
  }
  closePath(): void {
    this.calls.push({ op: "closePath", args: [] });
  }
  moveTo(...args: number[]): void {
    this.calls.push({ op: "moveTo", args });
  }
  arc(...args: unknown[]): void {
    this.calls.push({ op: "arc", args });
  }
  fill(): void {
    this.calls.push({ op: "fill", args: [this.#fillStyle] });
  }
  stroke(): void {
    this.calls.push({ op: "stroke", args: [this.strokeStyle] });
  }
}

interface FakeCanvas {
  width: number;
  height: number;
  getContext(id: string, options?: unknown): RecordingContext;
}

function makeCanvas(): { canvas: HTMLCanvasElement; context: RecordingContext } {
  const context = new RecordingContext();
  const canvas: FakeCanvas = {
    width: 0,
    height: 0,
    getContext: () => context,
  };
  return { canvas: canvas as unknown as HTMLCanvasElement, context };
}

const palette: Palette = {
  fills: Array.from({ length: 256 }, (_unused, index) => `cat-${index}`),
  border: "border",
  selection: "selection",
  hover: "hover",
  background: "background",
  revision: 1,
};

const treemap = decodeLayoutBatch(buildLayoutIpc(SAMPLE_TREEMAP_ROWS), { generation: "1", root: 0, kind: "treemap" });
const sunburst = decodeLayoutBatch(buildLayoutIpc(SAMPLE_SUNBURST_ROWS), { generation: "1", root: 0, kind: "sunburst" });

function fillRects(context: RecordingContext): Call[] {
  return context.calls.filter((call) => call.op === "fillRect");
}

test("the backing store is sized for the device pixel ratio", () => {
  const { canvas, context } = makeCanvas();
  drawLayout(canvas, {
    batch: treemap,
    palette,
    width: 100,
    height: 100,
    devicePixelRatio: 2,
    selected: new Set(),
    frame: null,
  });
  assert.equal(canvas.width, 200);
  assert.equal(canvas.height, 200);
  assert.deepEqual(context.calls[0], { op: "setTransform", args: [2, 0, 0, 2, 0, 0] });
});

test("every tile is filled, parents first, in its category colour", () => {
  const { canvas, context } = makeCanvas();
  const stats = drawLayout(canvas, {
    batch: treemap,
    palette,
    width: 100,
    height: 100,
    devicePixelRatio: 1,
    selected: new Set(),
    frame: null,
  });

  assert.ok(stats !== null);
  assert.equal(stats.drawn, 4);
  assert.equal(stats.culled, 0);

  const fills = fillRects(context);
  // One background clear plus four tiles.
  assert.equal(fills.length, 5);
  assert.deepEqual(fills[0]?.args, [0, 0, 100, 100, "background"]);
  assert.deepEqual(fills[1]?.args, [0, 0, 100, 100, "cat-0"]);
  assert.deepEqual(fills[2]?.args, [0, 0, 60, 100, "cat-11"]);
  assert.deepEqual(fills[3]?.args, [60, 0, 40, 55, "cat-8"]);
  assert.deepEqual(fills[4]?.args, [60, 55, 40, 45, "cat-14"]);
});

test("fillStyle is assigned once per colour run, not once per tile", () => {
  const rows = [
    { node: 1, depth: 1, x: 0, y: 0, w: 10, h: 10, category: 5 },
    { node: 2, depth: 1, x: 10, y: 0, w: 10, h: 10, category: 5 },
    { node: 3, depth: 1, x: 20, y: 0, w: 10, h: 10, category: 5 },
    { node: 4, depth: 1, x: 30, y: 0, w: 10, h: 10, category: 6 },
  ];
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "treemap" });
  const { canvas, context } = makeCanvas();
  drawLayout(canvas, { batch, palette, width: 40, height: 10, devicePixelRatio: 1, selected: new Set(), frame: null });

  const assignments = context.calls.filter((call) => call.op === "fillStyle").map((call) => call.args[0]);
  // background, then cat-5 once for the run of three, then cat-6.
  assert.deepEqual(assignments, ["background", "cat-5", "cat-6"]);
});

test("zero-extent tiles are culled rather than drawn", () => {
  const rows = [
    { node: 1, depth: 1, x: 0, y: 0, w: 10, h: 10, category: 1 },
    { node: 2, depth: 1, x: 10, y: 0, w: 0, h: 10, category: 2 },
    { node: 3, depth: 1, x: 10, y: 0, w: 10, h: 0, category: 3 },
  ];
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "treemap" });
  const { canvas } = makeCanvas();
  const stats = drawLayout(canvas, {
    batch,
    palette,
    width: 20,
    height: 10,
    devicePixelRatio: 1,
    selected: new Set(),
    frame: null,
  });
  assert.equal(stats?.drawn, 1);
  assert.equal(stats?.culled, 2);
});

test("an empty batch clears the surface and draws nothing", () => {
  const batch = decodeLayoutBatch(buildLayoutIpc([]), { generation: "1", root: 0, kind: "treemap" });
  const { canvas, context } = makeCanvas();
  const stats = drawLayout(canvas, {
    batch,
    palette,
    width: 50,
    height: 50,
    devicePixelRatio: 1,
    selected: new Set(),
    frame: null,
  });
  assert.equal(stats?.drawn, 0);
  assert.equal(fillRects(context).length, 1);
});

test("selected tiles get an outline pass", () => {
  const { canvas, context } = makeCanvas();
  drawLayout(canvas, {
    batch: treemap,
    palette,
    width: 100,
    height: 100,
    devicePixelRatio: 1,
    selected: new Set([2]),
    frame: null,
  });
  const strokes = context.calls.filter((call) => call.op === "stroke");
  assert.ok(strokes.some((call) => call.args[0] === "selection"));
  // The outline is inset by 1px on each side of the (60, 0, 40, 55) tile.
  assert.ok(context.calls.some((call) => call.op === "rect" && JSON.stringify(call.args) === JSON.stringify([61, 1, 38, 53])));
});

test("no selection means no selection stroke", () => {
  const { canvas, context } = makeCanvas();
  drawLayout(canvas, {
    batch: treemap,
    palette,
    width: 100,
    height: 100,
    devicePixelRatio: 1,
    selected: new Set(),
    frame: null,
  });
  assert.ok(!context.calls.some((call) => call.op === "stroke" && call.args[0] === "selection"));
});

test("the sunburst draws arcs, not rectangles", () => {
  const { canvas, context } = makeCanvas();
  const frame = computeSunburstFrame(sunburst, 200, 200);
  const stats = drawLayout(canvas, {
    batch: sunburst,
    palette,
    width: 200,
    height: 200,
    devicePixelRatio: 1,
    selected: new Set(),
    frame,
  });

  assert.equal(stats?.drawn, 4);
  assert.equal(fillRects(context).length, 1, "only the background clear uses fillRect");
  assert.equal(context.calls.filter((call) => call.op === "fill").length, 4);

  // The innermost full ring is drawn as a disc (moveTo centre), the rest as annuli.
  assert.ok(context.calls.some((call) => call.op === "moveTo" && JSON.stringify(call.args) === JSON.stringify([100, 100])));
  const arcs = context.calls.filter((call) => call.op === "arc");
  assert.ok(arcs.length >= 7);
});

test("sub-pixel arcs are culled", () => {
  const rows = [
    { node: 0, depth: 0, x: 0, y: 0, w: Math.PI * 2, h: 10, category: 0 },
    { node: 1, depth: 1, x: 0, y: 10, w: 1e-6, h: 10, category: 1 },
  ];
  const batch = decodeLayoutBatch(buildLayoutIpc(rows), { generation: "1", root: 0, kind: "sunburst" });
  const { canvas } = makeCanvas();
  const frame = computeSunburstFrame(batch, 200, 200);
  const stats = drawLayout(canvas, {
    batch,
    palette,
    width: 200,
    height: 200,
    devicePixelRatio: 1,
    selected: new Set(),
    frame,
  });
  assert.equal(stats?.drawn, 1);
  assert.equal(stats?.culled, 1);
});
