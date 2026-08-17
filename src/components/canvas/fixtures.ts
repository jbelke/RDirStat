/* =============================================================================
 * Golden-fixture builder for the `layout` Arrow batch.
 *
 * docs/01-ARCHITECTURE.md#ipc-contract: "Rust and TypeScript still share golden
 * fixtures for the empty, one-row, many-row, and deliberately-corrupted cases.
 * That test stays." This is the TypeScript half — it produces byte-identical
 * shapes to what `rdirstat-treemap` must emit, so the decoder can be tested
 * without a running backend, and so the Rust side has a reference to diff
 * against once its Arrow writer lands.
 *
 * Kept in `src/` rather than a test folder on purpose: the shell can use it to
 * render the canvas from a fixture with no Tauri process attached. Nothing in
 * the app imports it, so it is tree-shaken out of the production bundle.
 * ========================================================================== */

import { Field, Float32, RecordBatch, Schema, Struct, Table, Uint32, Uint8, makeData, tableToIPC } from "apache-arrow";

import {
  ARROW_META_GENERATION,
  ARROW_META_PROTOCOL_VERSION,
  ARROW_META_SCHEMA_NAME,
  ARROW_META_SCHEMA_VERSION,
  LAYOUT_COLUMNS,
  LAYOUT_SCHEMA_NAME,
  LAYOUT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
} from "./contract.ts";

export interface LayoutRow {
  readonly node: number;
  readonly depth: number;
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  readonly category: number;
}

export interface FixtureOptions {
  readonly generation?: string;
  readonly protocolVersion?: string;
  readonly schemaName?: string;
  readonly schemaVersion?: string;
  /** Metadata keys to leave out entirely. */
  readonly omitMetadata?: readonly string[];
  /** Override the column names, to exercise the schema gate. */
  readonly columnNames?: readonly string[];
  /** Mark every column nullable, to exercise the nullability gate. */
  readonly nullable?: boolean;
}

/** Build one Arrow IPC stream carrying the `layout` schema. */
export function buildLayoutIpc(rows: readonly LayoutRow[], options: FixtureOptions = {}): Uint8Array {
  const names = options.columnNames ?? LAYOUT_COLUMNS;
  const nullable = options.nullable ?? false;
  const length = rows.length;

  const nodeType = new Uint32();
  const depthType = new Uint32();
  const xType = new Float32();
  const yType = new Float32();
  const wType = new Float32();
  const hType = new Float32();
  const categoryType = new Uint8();

  const fields = [
    new Field(names[0] ?? "node", nodeType, nullable),
    new Field(names[1] ?? "depth", depthType, nullable),
    new Field(names[2] ?? "x", xType, nullable),
    new Field(names[3] ?? "y", yType, nullable),
    new Field(names[4] ?? "w", wType, nullable),
    new Field(names[5] ?? "h", hType, nullable),
    new Field(names[6] ?? "category", categoryType, nullable),
  ];

  const children = [
    makeData({ type: nodeType, length, data: Uint32Array.from(rows, (row) => row.node) }),
    makeData({ type: depthType, length, data: Uint32Array.from(rows, (row) => row.depth) }),
    makeData({ type: xType, length, data: Float32Array.from(rows, (row) => row.x) }),
    makeData({ type: yType, length, data: Float32Array.from(rows, (row) => row.y) }),
    makeData({ type: wType, length, data: Float32Array.from(rows, (row) => row.w) }),
    makeData({ type: hType, length, data: Float32Array.from(rows, (row) => row.h) }),
    makeData({ type: categoryType, length, data: Uint8Array.from(rows, (row) => row.category) }),
  ];

  const metadata = new Map<string, string>();
  const omit = new Set(options.omitMetadata ?? []);
  if (!omit.has(ARROW_META_PROTOCOL_VERSION)) {
    metadata.set(ARROW_META_PROTOCOL_VERSION, options.protocolVersion ?? String(PROTOCOL_VERSION));
  }
  if (!omit.has(ARROW_META_SCHEMA_NAME)) {
    metadata.set(ARROW_META_SCHEMA_NAME, options.schemaName ?? LAYOUT_SCHEMA_NAME);
  }
  if (!omit.has(ARROW_META_SCHEMA_VERSION)) {
    metadata.set(ARROW_META_SCHEMA_VERSION, options.schemaVersion ?? String(LAYOUT_SCHEMA_VERSION));
  }
  if (!omit.has(ARROW_META_GENERATION)) {
    metadata.set(ARROW_META_GENERATION, options.generation ?? "1");
  }

  const schema = new Schema(fields, metadata);
  const structData = makeData({ type: new Struct(fields), length, nullCount: 0, children });
  const table = new Table(new RecordBatch(schema, structData));
  return tableToIPC(table, "stream");
}

/** A 4-tile treemap that fills a 100x100 viewport. Deterministic. */
export const SAMPLE_TREEMAP_ROWS: readonly LayoutRow[] = [
  { node: 0, depth: 0, x: 0, y: 0, w: 100, h: 100, category: 0 },
  { node: 1, depth: 1, x: 0, y: 0, w: 60, h: 100, category: 11 },
  { node: 2, depth: 1, x: 60, y: 0, w: 40, h: 55, category: 8 },
  { node: 3, depth: 1, x: 60, y: 55, w: 40, h: 45, category: 14 },
];

/** The same shape as a sunburst: (start_angle, inner_radius, sweep, thickness). */
export const SAMPLE_SUNBURST_ROWS: readonly LayoutRow[] = [
  { node: 0, depth: 0, x: 0, y: 0, w: Math.PI * 2, h: 20, category: 0 },
  { node: 1, depth: 1, x: 0, y: 20, w: Math.PI, h: 20, category: 11 },
  { node: 2, depth: 1, x: Math.PI, y: 20, w: Math.PI / 2, h: 20, category: 8 },
  { node: 3, depth: 1, x: (Math.PI * 3) / 2, y: 20, w: Math.PI / 2, h: 20, category: 14 },
];
