/* =============================================================================
 * Arrow IPC -> typed arrays, with the schema as the contract.
 *
 * This is the only place a `layout` response becomes drawable data, and it is
 * deliberately paranoid: docs/01-ARCHITECTURE.md#ipc-contract requires that the
 * frontend "rejects a batch whose generation is not the one it asked for" and
 * that "a malformed or truncated buffer fails closed". Every early return in
 * this file throws `LayoutError`; none of them returns partial data.
 *
 * Cost: one pass to validate metadata (O(1)), one pass per float column to
 * reject non-finite geometry (O(rows), four columns). That runs once per
 * navigate/resize, never per frame.
 * ========================================================================== */

import { DataType, Precision, tableFromIPC, type Table, type Vector } from "apache-arrow";

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
import { LayoutError } from "./errors.ts";
import { normalizeGeneration } from "./generation.ts";
import type { Generation, LayoutBatch, LayoutKind } from "./types.ts";

/** What the caller asked for. A batch that does not match all of it is refused. */
export interface LayoutExpectation {
  readonly generation: Generation;
  readonly root: number;
  readonly kind: LayoutKind;
  readonly protocolVersion?: number;
  readonly schemaVersion?: number;
}

type ColumnKind = "u32" | "f32" | "u8";

const COLUMN_KINDS: readonly ColumnKind[] = ["u32", "u32", "f32", "f32", "f32", "f32", "u8"];

function toBytes(source: ArrayBuffer | Uint8Array | number[]): Uint8Array {
  if (source instanceof Uint8Array) return source;
  if (Array.isArray(source)) return Uint8Array.from(source);
  return new Uint8Array(source);
}

function requireMetadata(metadata: Map<string, string>, key: string): string {
  const value = metadata.get(key);
  if (value === undefined) {
    throw new LayoutError(
      "missing_metadata",
      "The backend sent a layout batch without its schema metadata.",
      `missing Arrow schema-metadata key ${key}`,
    );
  }
  return value;
}

function matchesColumnType(type: DataType, kind: ColumnKind): boolean {
  if (kind === "f32") {
    return DataType.isFloat(type) && type.precision === Precision.SINGLE;
  }
  if (!DataType.isInt(type)) return false;
  if (type.isSigned) return false;
  return kind === "u32" ? type.bitWidth === 32 : type.bitWidth === 8;
}

function typeLabel(kind: ColumnKind): string {
  return kind === "u32" ? "UInt32" : kind === "f32" ? "Float32" : "UInt8";
}

/**
 * Copy one Arrow column out as a contiguous typed array.
 *
 * Zero-copy when the column is a single unsliced chunk (the normal case for a
 * single-RecordBatch response); otherwise one allocation per column, which is
 * fine outside the paint loop.
 */
function readColumn(table: Table, index: number, rows: number): Uint32Array | Float32Array | Uint8Array {
  const name = LAYOUT_COLUMNS[index];
  const kind = COLUMN_KINDS[index];
  const field = table.schema.fields[index];

  if (field === undefined || field.name !== name) {
    throw new LayoutError(
      "column_mismatch",
      "The backend sent a layout batch with the wrong columns.",
      `column ${index} should be "${name}", got ${field === undefined ? "<absent>" : `"${field.name}"`}`,
    );
  }
  if (field.nullable) {
    throw new LayoutError(
      "column_mismatch",
      "The backend sent a layout batch with a nullable column.",
      `column "${name}" is nullable; the pinned schema forbids it`,
    );
  }
  if (!matchesColumnType(field.type, kind)) {
    throw new LayoutError(
      "column_mismatch",
      "The backend sent a layout batch with the wrong column types.",
      `column "${name}" should be ${typeLabel(kind)}, got ${String(field.type)}`,
    );
  }

  const vector: Vector | null = table.getChildAt(index);
  if (vector === null) {
    throw new LayoutError("column_mismatch", "The backend sent a layout batch with a missing column.", `column "${name}" has no data`);
  }
  if (vector.nullCount !== 0) {
    throw new LayoutError(
      "column_mismatch",
      "The backend sent a layout batch containing nulls.",
      `column "${name}" has ${vector.nullCount} null entries`,
    );
  }

  const Ctor = kind === "u32" ? Uint32Array : kind === "f32" ? Float32Array : Uint8Array;

  let declared = 0;
  for (const chunk of vector.data) declared += chunk.length;
  if (declared !== rows) {
    throw new LayoutError(
      "length_mismatch",
      "The backend sent a layout batch whose columns disagree in length.",
      `column "${name}" holds ${declared} values but the batch declares ${rows} rows`,
    );
  }

  const slices: (Uint32Array | Float32Array | Uint8Array)[] = [];
  for (const chunk of vector.data) {
    const values: unknown = chunk.values;
    if (!(values instanceof Ctor)) {
      throw new LayoutError(
        "column_mismatch",
        "The backend sent a layout batch with an unreadable buffer.",
        `column "${name}" buffer is not a ${typeLabel(kind)} array`,
      );
    }
    const slice = values.subarray(chunk.offset, chunk.offset + chunk.length);
    // `subarray` clamps: a shorter result means the IPC buffer was truncated
    // relative to the length the schema claims.
    if (slice.length !== chunk.length) {
      throw new LayoutError(
        "malformed_buffer",
        "The backend sent a truncated layout batch.",
        `column "${name}" buffer holds ${slice.length} of ${chunk.length} declared values`,
      );
    }
    slices.push(slice);
  }

  if (slices.length === 1) {
    const only = slices[0];
    if (only !== undefined) return only;
  }

  const out = new Ctor(rows);
  const sink = out as { set(source: ArrayLike<number>, offset: number): void };
  let at = 0;
  for (const slice of slices) {
    sink.set(slice, at);
    at += slice.length;
  }
  return out;
}

/**
 * Reject non-finite or negative extents before anything is drawn. A `NaN` width
 * silently paints nothing and breaks hit-testing for every tile after it, so it
 * is cheaper to refuse the whole batch.
 */
function validateGeometry(x: Float32Array, y: Float32Array, w: Float32Array, h: Float32Array, rows: number): void {
  for (let i = 0; i < rows; i += 1) {
    const xi = x[i];
    const yi = y[i];
    const wi = w[i];
    const hi = h[i];
    if (!Number.isFinite(xi) || !Number.isFinite(yi) || !Number.isFinite(wi) || !Number.isFinite(hi)) {
      throw new LayoutError(
        "invalid_geometry",
        "The backend sent a layout batch with non-finite geometry.",
        `row ${i} is (${xi}, ${yi}, ${wi}, ${hi})`,
      );
    }
    if (wi < 0 || hi < 0) {
      throw new LayoutError(
        "invalid_geometry",
        "The backend sent a layout batch with a negative extent.",
        `row ${i} has extent (${wi}, ${hi})`,
      );
    }
  }
}

/**
 * Decode and validate one `layout` response.
 *
 * @throws LayoutError for every rejection. Callers must render the error, not a
 *   partial batch.
 */
export function decodeLayoutBatch(source: ArrayBuffer | Uint8Array | number[], expected: LayoutExpectation): LayoutBatch {
  const bytes = toBytes(source);
  if (bytes.byteLength === 0) {
    throw new LayoutError("empty_payload", "The backend sent an empty layout response.", "zero-length buffer");
  }

  const wantGeneration = normalizeGeneration(expected.generation);
  const wantProtocol = expected.protocolVersion ?? PROTOCOL_VERSION;
  const wantSchemaVersion = expected.schemaVersion ?? LAYOUT_SCHEMA_VERSION;

  let table: Table;
  try {
    table = tableFromIPC(bytes);
  } catch (cause) {
    throw new LayoutError(
      "malformed_buffer",
      "The backend sent a layout batch this build could not read.",
      cause instanceof Error ? cause.message : String(cause),
      { cause },
    );
  }

  const metadata = table.schema.metadata;

  const protocolText = requireMetadata(metadata, ARROW_META_PROTOCOL_VERSION);
  const protocol = Number.parseInt(protocolText, 10);
  if (!Number.isInteger(protocol) || protocol !== wantProtocol) {
    throw new LayoutError(
      "protocol_mismatch",
      `This build speaks layout protocol ${wantProtocol}; the backend sent ${protocolText}.`,
      `${ARROW_META_PROTOCOL_VERSION}=${protocolText}`,
    );
  }

  const schemaName = requireMetadata(metadata, ARROW_META_SCHEMA_NAME);
  if (schemaName !== LAYOUT_SCHEMA_NAME) {
    throw new LayoutError(
      "schema_mismatch",
      "The backend answered the layout request with a different dataset.",
      `${ARROW_META_SCHEMA_NAME}=${schemaName}, expected ${LAYOUT_SCHEMA_NAME}`,
    );
  }

  const schemaVersionText = requireMetadata(metadata, ARROW_META_SCHEMA_VERSION);
  const schemaVersion = Number.parseInt(schemaVersionText, 10);
  if (!Number.isInteger(schemaVersion) || schemaVersion !== wantSchemaVersion) {
    throw new LayoutError(
      "schema_version_mismatch",
      `This build reads layout schema v${wantSchemaVersion}; the backend sent v${schemaVersionText}.`,
      `${ARROW_META_SCHEMA_VERSION}=${schemaVersionText}`,
    );
  }

  // The generation gate. This is the check that keeps a batch computed against
  // the previous scan from being painted over the current one.
  const generationText = requireMetadata(metadata, ARROW_META_GENERATION);
  let generation: string;
  try {
    generation = normalizeGeneration(generationText);
  } catch (cause) {
    throw new LayoutError(
      "stale_generation",
      "The backend stamped the layout batch with an unreadable generation.",
      `${ARROW_META_GENERATION}=${generationText}`,
      { cause },
    );
  }
  if (generation !== wantGeneration) {
    throw new LayoutError(
      "stale_generation",
      "The layout data is from a different scan than the one on screen.",
      `requested generation ${wantGeneration}, batch carries ${generation}`,
    );
  }

  if (table.schema.fields.length !== LAYOUT_COLUMNS.length) {
    throw new LayoutError(
      "column_mismatch",
      "The backend sent a layout batch with the wrong number of columns.",
      `expected ${LAYOUT_COLUMNS.length} columns (${LAYOUT_COLUMNS.join(", ")}), got ${table.schema.fields.length}`,
    );
  }

  const rows = table.numRows;
  if (!Number.isInteger(rows) || rows < 0) {
    throw new LayoutError("length_mismatch", "The backend sent a layout batch with an impossible row count.", `numRows=${String(rows)}`);
  }

  const node = readColumn(table, 0, rows) as Uint32Array;
  const depth = readColumn(table, 1, rows) as Uint32Array;
  const x = readColumn(table, 2, rows) as Float32Array;
  const y = readColumn(table, 3, rows) as Float32Array;
  const w = readColumn(table, 4, rows) as Float32Array;
  const h = readColumn(table, 5, rows) as Float32Array;
  const category = readColumn(table, 6, rows) as Uint8Array;

  validateGeometry(x, y, w, h, rows);

  return {
    generation,
    protocolVersion: protocol,
    schemaVersion,
    kind: expected.kind,
    root: expected.root,
    count: rows,
    node,
    depth,
    x,
    y,
    w,
    h,
    category,
  };
}

/** An empty batch. Used as the initial state so the renderer never sees `null`. */
export function emptyLayoutBatch(generation: string, root: number, kind: LayoutKind): LayoutBatch {
  return {
    generation,
    protocolVersion: PROTOCOL_VERSION,
    schemaVersion: LAYOUT_SCHEMA_VERSION,
    kind,
    root,
    count: 0,
    node: new Uint32Array(0),
    depth: new Uint32Array(0),
    x: new Float32Array(0),
    y: new Float32Array(0),
    w: new Float32Array(0),
    h: new Float32Array(0),
    category: new Uint8Array(0),
  };
}
