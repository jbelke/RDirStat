/* =============================================================================
 * Decoder tests — the empty, one-row, many-row and corrupted cases.
 *
 * Run with:  node --test 'src/components/canvas/*.test.ts'
 * Node ships type stripping and `node:test`, so these need no extra dependency
 * and no config in a package.json this agent does not own.
 * ========================================================================== */

import assert from "node:assert/strict";
import { test } from "node:test";

import { decodeLayoutBatch, emptyLayoutBatch } from "./arrow.ts";
import {
  ARROW_META_GENERATION,
  ARROW_META_PROTOCOL_VERSION,
  ARROW_META_SCHEMA_NAME,
  assertContractConstants,
} from "./contract.ts";
import { LayoutError } from "./errors.ts";
import { SAMPLE_TREEMAP_ROWS, buildLayoutIpc, type LayoutRow } from "./fixtures.ts";

const EXPECT = { generation: "1", root: 0, kind: "treemap" } as const;

function refusal(fn: () => unknown): LayoutError {
  try {
    fn();
  } catch (error) {
    assert.ok(error instanceof LayoutError, `expected LayoutError, got ${String(error)}`);
    return error;
  }
  throw new assert.AssertionError({ message: "expected a LayoutError, but the call succeeded" });
}

test("the mirrored contract constants have not drifted", () => {
  assertContractConstants();
});

test("decodes the many-row case into typed arrays", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS);
  const batch = decodeLayoutBatch(bytes, EXPECT);

  assert.equal(batch.count, 4);
  assert.equal(batch.generation, "1");
  assert.equal(batch.kind, "treemap");
  assert.ok(batch.node instanceof Uint32Array);
  assert.ok(batch.x instanceof Float32Array);
  assert.ok(batch.category instanceof Uint8Array);
  assert.deepEqual(Array.from(batch.node), [0, 1, 2, 3]);
  assert.deepEqual(Array.from(batch.depth), [0, 1, 1, 1]);
  assert.deepEqual(Array.from(batch.category), [0, 11, 8, 14]);
  assert.deepEqual(Array.from(batch.w), [100, 60, 40, 40]);
});

test("decodes the empty case without error", () => {
  const batch = decodeLayoutBatch(buildLayoutIpc([]), EXPECT);
  assert.equal(batch.count, 0);
  assert.equal(batch.node.length, 0);
});

test("decodes the one-row case", () => {
  const row: LayoutRow = { node: 7, depth: 0, x: 1.5, y: 2.5, w: 3.5, h: 4.5, category: 3 };
  const batch = decodeLayoutBatch(buildLayoutIpc([row]), EXPECT);
  assert.equal(batch.count, 1);
  assert.equal(batch.node[0], 7);
  assert.equal(batch.x[0], 1.5);
  assert.equal(batch.h[0], 4.5);
});

test("accepts an ArrayBuffer, a Uint8Array and a number[]", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS);
  const copy = bytes.slice();
  const buffer = copy.buffer.slice(copy.byteOffset, copy.byteOffset + copy.byteLength) as ArrayBuffer;
  assert.equal(decodeLayoutBatch(buffer, EXPECT).count, 4);
  assert.equal(decodeLayoutBatch(bytes, EXPECT).count, 4);
  assert.equal(decodeLayoutBatch(Array.from(bytes), EXPECT).count, 4);
});

test("rejects a batch from a different generation", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { generation: "2" });
  const error = refusal(() => decodeLayoutBatch(bytes, EXPECT));
  assert.equal(error.code, "stale_generation");
});

test("generation comparison is form-insensitive", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { generation: "0000042" });
  assert.equal(decodeLayoutBatch(bytes, { ...EXPECT, generation: 42 }).generation, "42");
  assert.equal(decodeLayoutBatch(bytes, { ...EXPECT, generation: 42n }).generation, "42");
  assert.equal(decodeLayoutBatch(bytes, { ...EXPECT, generation: "42" }).generation, "42");
});

test("rejects a truncated buffer instead of rendering part of it", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS);
  for (const fraction of [0.25, 0.5, 0.75, 0.95]) {
    const cut = bytes.subarray(0, Math.floor(bytes.length * fraction));
    const error = refusal(() => decodeLayoutBatch(cut, EXPECT));
    assert.ok(
      error.code === "malformed_buffer" || error.code === "length_mismatch" || error.code === "column_mismatch",
      `unexpected code ${error.code} at ${fraction}`,
    );
  }
});

test("rejects an empty buffer", () => {
  assert.equal(refusal(() => decodeLayoutBatch(new Uint8Array(0), EXPECT)).code, "empty_payload");
});

test("rejects garbage bytes", () => {
  const garbage = Uint8Array.from({ length: 512 }, (_unused, index) => (index * 37) % 251);
  assert.equal(refusal(() => decodeLayoutBatch(garbage, EXPECT)).code, "malformed_buffer");
});

test("rejects a missing metadata key", () => {
  for (const key of [ARROW_META_PROTOCOL_VERSION, ARROW_META_SCHEMA_NAME, ARROW_META_GENERATION]) {
    const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { omitMetadata: [key] });
    assert.equal(refusal(() => decodeLayoutBatch(bytes, EXPECT)).code, "missing_metadata", key);
  }
});

test("rejects a protocol version this build does not speak", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { protocolVersion: "2" });
  assert.equal(refusal(() => decodeLayoutBatch(bytes, EXPECT)).code, "protocol_mismatch");
});

test("rejects another schema answering the layout request", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { schemaName: "category_totals" });
  assert.equal(refusal(() => decodeLayoutBatch(bytes, EXPECT)).code, "schema_mismatch");
});

test("rejects a newer schema version", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { schemaVersion: "2" });
  assert.equal(refusal(() => decodeLayoutBatch(bytes, EXPECT)).code, "schema_version_mismatch");
});

test("rejects renamed or reordered columns", () => {
  const renamed = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { columnNames: ["node", "depth", "x", "y", "width", "h", "category"] });
  assert.equal(refusal(() => decodeLayoutBatch(renamed, EXPECT)).code, "column_mismatch");

  const reordered = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { columnNames: ["depth", "node", "x", "y", "w", "h", "category"] });
  assert.equal(refusal(() => decodeLayoutBatch(reordered, EXPECT)).code, "column_mismatch");
});

test("rejects nullable columns", () => {
  const bytes = buildLayoutIpc(SAMPLE_TREEMAP_ROWS, { nullable: true });
  assert.equal(refusal(() => decodeLayoutBatch(bytes, EXPECT)).code, "column_mismatch");
});

test("rejects non-finite and negative geometry", () => {
  const nan = buildLayoutIpc([{ node: 1, depth: 0, x: Number.NaN, y: 0, w: 1, h: 1, category: 0 }]);
  assert.equal(refusal(() => decodeLayoutBatch(nan, EXPECT)).code, "invalid_geometry");

  const negative = buildLayoutIpc([{ node: 1, depth: 0, x: 0, y: 0, w: -1, h: 1, category: 0 }]);
  assert.equal(refusal(() => decodeLayoutBatch(negative, EXPECT)).code, "invalid_geometry");

  const infinite = buildLayoutIpc([{ node: 1, depth: 0, x: 0, y: 0, w: Number.POSITIVE_INFINITY, h: 1, category: 0 }]);
  assert.equal(refusal(() => decodeLayoutBatch(infinite, EXPECT)).code, "invalid_geometry");
});

test("emptyLayoutBatch is a usable zero state", () => {
  const batch = emptyLayoutBatch("5", 0, "icicle");
  assert.equal(batch.count, 0);
  assert.equal(batch.generation, "5");
  assert.equal(batch.kind, "icicle");
});
