/**
 * Wire-level constants and coercions shared by everything that touches IPC.
 *
 * Two jobs:
 *
 * 1. Mirror the crate-root constants of `rdirstat-core` that the frontend has
 *    to agree with (page limit, protocol version, the `NodeId` encoding, the
 *    flag bits). These are duplicated here because they are `pub const` in
 *    Rust, not command return values — there is nothing for tauri-specta to
 *    generate. Each one names its Rust source so a drift is greppable.
 *
 * 2. Absorb the one thing about the generated bindings that could not be
 *    verified: how `u64` is exported. `specta` can emit `number`, `string`, or
 *    `bigint` depending on the exporter's `BigIntExportBehavior`, and the
 *    choice belongs to `src-tauri`. Everything downstream of `num()` is a
 *    plain JS `number`, which is exact for byte counts below 2^53 (9.0 PB) —
 *    three orders of magnitude past the largest volume this app targets.
 */

/** `rdirstat_core::PROTOCOL_VERSION` */
export const PROTOCOL_VERSION = 1;

/** `rdirstat_core::MAX_CHILD_PAGE` — the backend clamps to this; asking for more is an error. */
export const MAX_CHILD_PAGE = 500;

/** `rdirstat_core::PROGRESS_MAX_HZ` */
export const PROGRESS_MAX_HZ = 10;

/** `rdirstat_core::MIN_TILE_PX` */
export const MIN_TILE_PX = 3.0;

/** `rdirstat_core::SCAN_PROGRESS_EVENT` */
export const SCAN_PROGRESS_EVENT = "scan:progress";

/** `rdirstat_core::VIRTUAL_GROUP_BIT` — the `<Files>` pseudo-child of a directory. */
export const VIRTUAL_GROUP_BIT = 0x8000_0000;

/** `rdirstat_core::NodeId::NONE`. Note it is `0x7FFF_FFFF`, **not** `u32::MAX`. */
export const NODE_ID_NONE = 0x7fff_ffff;

/** `rdirstat_core::NodeId::ROOT` */
export const NODE_ID_ROOT = 0;

/** `rdirstat_core::TreeGeneration::NONE` — "no scan is loaded". */
export const GENERATION_NONE = 0;

/**
 * `rdirstat_core::flags` — 16 bits, 13 assigned.
 *
 * The frontend reads these to annotate rows; it never *computes* with them.
 * In particular `HARD_LINK_REPEAT` is presentational here only: the "counts
 * zero bytes" policy lives in `Node::contributed_size()` in Rust and the sizes
 * on the wire already have it applied.
 */
export const FLAGS = {
  NONE: 0,
  UNREADABLE: 1 << 0,
  MOUNT_POINT: 1 << 1,
  FIRMLINK: 1 << 2,
  EXCLUDED: 1 << 3,
  HARD_LINK: 1 << 4,
  HARD_LINK_REPEAT: 1 << 5,
  SPARSE: 1 << 6,
  EXECUTABLE: 1 << 7,
  PACKAGE: 1 << 8,
  AGGREGATED: 1 << 9,
  MUTATED: 1 << 10,
  INCOMPLETE: 1 << 11,
  BROKEN_SYMLINK: 1 << 12,
} as const;

/**
 * `rdirstat_core::flags::INCOMPLETE_SUBTREE` — the set that makes a subtree's
 * totals a *floor* rather than a measurement. A row carrying any of these must
 * not be presented as an exact number.
 */
export const INCOMPLETE_SUBTREE =
  FLAGS.UNREADABLE | FLAGS.EXCLUDED | FLAGS.INCOMPLETE | FLAGS.AGGREGATED;

export function hasFlag(flags: number, bit: number): boolean {
  return (flags & bit) !== 0;
}

/** True when the node is the synthetic `<Files>` group of some directory. */
export function isVirtualGroup(node: number): boolean {
  // `>>> 0` because JS bitwise operators produce a signed int32 and the group
  // bit is the sign bit.
  return (node >>> 0) >= VIRTUAL_GROUP_BIT && (node >>> 0) !== 0xffff_ffff;
}

/** True for a real arena slot: not NONE, not a virtual group, not the reserved value. */
export function isRealNode(node: number): boolean {
  const raw = node >>> 0;
  return raw < NODE_ID_NONE;
}

/**
 * A value that a `u64` field may arrive as, depending on how `src-tauri`
 * configured `BigIntExportBehavior`.
 */
export type NumLike = number | bigint | string;

/**
 * Normalize a wire scalar to a JS `number`.
 *
 * Returns 0 rather than NaN for junk, because every consumer of this is a
 * width, a ratio, or a formatted byte count, and `NaN%` on screen is worse
 * than a visible zero.
 */
export function num(value: NumLike | null | undefined): number {
  if (typeof value === "number") return Number.isFinite(value) ? value : 0;
  if (typeof value === "bigint") return Number(value);
  if (typeof value === "string") {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : 0;
  }
  return 0;
}

/** Unwrap the tauri-specta `Result` union, throwing the typed error payload. */
export type WireResult<T, E> = { status: "ok"; data: T } | { status: "error"; error: E };

/**
 * The error a failed command throws. Carries the typed variant so a caller can
 * branch on `kind` (e.g. `stale_generation` -> refetch, not "show a toast").
 */
export class IpcError<E = unknown> extends Error {
  readonly payload: E;
  readonly command: string;

  constructor(command: string, payload: E) {
    super(`${command}: ${describeError(payload)}`);
    this.name = "IpcError";
    this.command = command;
    this.payload = payload;
  }
}

/** Best-effort human string for an adjacently-tagged core error. */
export function describeError(payload: unknown): string {
  if (payload == null) return "unknown error";
  if (typeof payload === "string") return payload;
  if (typeof payload !== "object") return String(payload);

  const tagged = payload as { kind?: unknown; detail?: unknown };
  if (typeof tagged.kind !== "string") {
    return payload instanceof Error ? payload.message : JSON.stringify(payload);
  }

  const label = tagged.kind.replace(/_/g, " ");
  const detail = tagged.detail;
  if (detail === undefined) return label;
  if (typeof detail === "string") return `${label}: ${detail}`;
  if (typeof detail === "object" && detail !== null) {
    const entries = Object.entries(detail as Record<string, unknown>)
      .map(([key, value]) => `${key}=${String(value)}`)
      .join(", ");
    return entries.length > 0 ? `${label} (${entries})` : label;
  }
  return `${label}: ${String(detail)}`;
}

/** `true` when a query error means "your generation is gone", i.e. refetch, do not report. */
export function isStaleGeneration(error: unknown): boolean {
  return (
    error instanceof IpcError &&
    typeof error.payload === "object" &&
    error.payload !== null &&
    (error.payload as { kind?: string }).kind === "stale_generation"
  );
}

/** Unwrap a command result or throw an {@link IpcError}. */
export async function unwrap<T, E>(
  command: string,
  call: Promise<WireResult<T, E>>,
): Promise<T> {
  const result = await call;
  if (result.status === "ok") return result.data;
  throw new IpcError(command, result.error);
}
