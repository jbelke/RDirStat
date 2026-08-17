/* =============================================================================
 * Fail-closed error type for the layout binary channel.
 *
 * docs/01-ARCHITECTURE.md#ipc-contract: "a malformed or truncated buffer fails
 * closed". Every rejection below produces a *visible* error and NO tiles. There
 * is deliberately no partial-render path — half a treemap is a wrong answer that
 * looks like a right one.
 * ========================================================================== */

/** Machine-readable reason a layout batch was refused. */
export type LayoutErrorCode =
  /** The buffer could not be parsed as Arrow IPC at all (truncated, garbage). */
  | "malformed_buffer"
  /** The buffer parsed but carried no record batch / zero columns. */
  | "empty_payload"
  /** A required `rdirstat.*` schema-metadata key is absent. */
  | "missing_metadata"
  /** `rdirstat.protocol_version` is not the version this build speaks. */
  | "protocol_mismatch"
  /** `rdirstat.schema` is not `layout`. */
  | "schema_mismatch"
  /** `rdirstat.schema_version` is not the version this build speaks. */
  | "schema_version_mismatch"
  /** The batch answers a generation other than the one that was requested. */
  | "stale_generation"
  /** Column names, order, types, or nullability do not match the pinned schema. */
  | "column_mismatch"
  /** Columns disagree with each other or with `numRows`. */
  | "length_mismatch"
  /** A non-finite or negative geometry value made it into the batch. */
  | "invalid_geometry"
  /** The `layout` command itself failed (IPC error, backend `QueryError`). */
  | "transport_failure";

/**
 * A refusal to render. `code` is stable and testable; `message` is the user-
 * visible sentence; `detail` carries the specifics for the console.
 */
export class LayoutError extends Error {
  readonly code: LayoutErrorCode;
  readonly detail: string | undefined;

  constructor(code: LayoutErrorCode, message: string, detail?: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "LayoutError";
    this.code = code;
    this.detail = detail;
  }
}

/** Narrowing helper — `instanceof` across a bundle boundary is not reliable. */
export function isLayoutError(value: unknown): value is LayoutError {
  return value instanceof LayoutError || (typeof value === "object" && value !== null && "code" in value && (value as { name?: unknown }).name === "LayoutError");
}

/**
 * Best-effort human text for anything thrown by the transport. Backend errors
 * cross IPC as adjacently-tagged discriminated unions (`{kind, detail}`), so
 * that shape is unwrapped first; a bare string or `Error` is the fallback.
 */
export function describeTransportFailure(cause: unknown): string {
  if (typeof cause === "string") return cause;
  if (cause instanceof Error) return cause.message;
  if (typeof cause === "object" && cause !== null) {
    const tagged = cause as { kind?: unknown; detail?: unknown };
    if (typeof tagged.kind === "string") {
      const detail = tagged.detail;
      if (detail === undefined || detail === null) return tagged.kind;
      if (typeof detail === "string") return `${tagged.kind}: ${detail}`;
      return `${tagged.kind}: ${JSON.stringify(detail)}`;
    }
    try {
      return JSON.stringify(cause);
    } catch {
      return "unserializable error";
    }
  }
  return String(cause);
}
