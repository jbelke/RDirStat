/* =============================================================================
 * Mirror of the `rdirstat-core` crate-root constants that cross the wire.
 *
 * These are NOT generated: `tauri-specta` emits types for command signatures and
 * DTOs, not for `pub const` items. Every value here is a hand-mirrored copy of a
 * constant in `crates/rdirstat-core/src/lib.rs` and must be changed in lockstep
 * with it. `assertContractConstants()` at the bottom is the cheap tripwire that
 * a fixture test can call.
 * ========================================================================== */

/** `rdirstat_core::PROTOCOL_VERSION`. */
export const PROTOCOL_VERSION = 1;

/** `rdirstat_core::LAYOUT_SCHEMA_NAME`. */
export const LAYOUT_SCHEMA_NAME = "layout";

/** `rdirstat_core::LAYOUT_SCHEMA_VERSION`. */
export const LAYOUT_SCHEMA_VERSION = 1;

/**
 * `rdirstat_core::LAYOUT_COLUMNS`. Order is part of the contract: the decoder
 * rejects a batch whose fields are not exactly this sequence.
 */
export const LAYOUT_COLUMNS = ["node", "depth", "x", "y", "w", "h", "category"] as const;

/** Arrow schema-metadata keys (`rdirstat_core::ARROW_META_*`). */
export const ARROW_META_PROTOCOL_VERSION = "rdirstat.protocol_version";
export const ARROW_META_GENERATION = "rdirstat.generation";
export const ARROW_META_SCHEMA_NAME = "rdirstat.schema";
export const ARROW_META_SCHEMA_VERSION = "rdirstat.schema_version";

/** `rdirstat_core::MIN_TILE_PX` — the sub-pixel cutoff the backend applies. */
export const MIN_TILE_PX = 3;

/* -----------------------------------------------------------------------------
 * `NodeId` encoding. Three disjoint cases, total over u32:
 *   0..=MAX_NODE_INDEX  -> arena slot
 *   NODE_ID_NONE        -> absent link (0x7FFF_FFFF, NOT u32::MAX)
 *   VIRTUAL_GROUP_BIT|i -> the `<Files>` group owned by directory index `i`
 * 0xFFFF_FFFF is reserved and rejected.
 *
 * Written as hex literals, never as `1 << 31`: JavaScript's `<<` yields a signed
 * int32, so `1 << 31` is -2147483648 and would compare wrong against the
 * unsigned values that arrive in the Arrow `node` column.
 * -------------------------------------------------------------------------- */
export const MAX_NODE_INDEX = 0x7fff_fffe;
export const NODE_ID_NONE = 0x7fff_ffff;
export const VIRTUAL_GROUP_BIT = 0x8000_0000;
export const NODE_ID_RESERVED = 0xffff_ffff;
export const NODE_ID_ROOT = 0;

/** True for an ordinary arena node (addressable, actionable). */
export function isRealNode(id: number): boolean {
  return Number.isInteger(id) && id >= 0 && id <= MAX_NODE_INDEX;
}

/** True for the synthetic `<Files>` group of some directory. */
export function isVirtualGroup(id: number): boolean {
  return Number.isInteger(id) && id >= VIRTUAL_GROUP_BIT && id < NODE_ID_RESERVED;
}

/** True for any id this build is willing to send back to the backend. */
export function isValidNodeId(id: number): boolean {
  return isRealNode(id) || isVirtualGroup(id) || id === NODE_ID_NONE;
}

/**
 * The directory that owns a virtual `<Files>` group, or `null` if `id` is not a
 * group. A group is never a filesystem path: it cannot be revealed or trashed.
 */
export function groupOwner(id: number): number | null {
  return isVirtualGroup(id) ? id - VIRTUAL_GROUP_BIT : null;
}

/**
 * Cheap self-check for the mirrored constants. Called from the decoder fixtures
 * so a careless edit here fails a test rather than a user's render.
 */
export function assertContractConstants(): void {
  const problems: string[] = [];
  if (PROTOCOL_VERSION !== 1) problems.push("PROTOCOL_VERSION");
  if (LAYOUT_SCHEMA_VERSION !== 1) problems.push("LAYOUT_SCHEMA_VERSION");
  if (LAYOUT_SCHEMA_NAME !== "layout") problems.push("LAYOUT_SCHEMA_NAME");
  if (LAYOUT_COLUMNS.length !== 7) problems.push("LAYOUT_COLUMNS");
  if (MAX_NODE_INDEX + 1 !== NODE_ID_NONE) problems.push("NODE_ID_NONE");
  if (NODE_ID_NONE + 1 !== VIRTUAL_GROUP_BIT) problems.push("VIRTUAL_GROUP_BIT");
  if (problems.length > 0) {
    throw new Error(`contract constants drifted from rdirstat-core: ${problems.join(", ")}`);
  }
}
