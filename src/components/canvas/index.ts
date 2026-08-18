/* =============================================================================
 * Public surface of the hierarchy canvas.
 *
 * The shell imports from `@/components/canvas` and nothing deeper. Everything
 * re-exported here is intended to be depended on; anything not listed is an
 * implementation detail that may move.
 * ========================================================================== */

export { HierarchyCanvas } from "./HierarchyCanvas.tsx";
export type { HierarchyCanvasProps, PaintReport } from "./HierarchyCanvas.tsx";

export { LayoutKindToggle } from "./LayoutKindToggle.tsx";
export type { LayoutKindToggleProps } from "./LayoutKindToggle.tsx";

export { LargestItemsList } from "./LargestItemsList.tsx";
export type { LargestItemsListProps } from "./LargestItemsList.tsx";

export { CanvasContextMenu } from "./CanvasContextMenu.tsx";
export type { CanvasContextMenuProps, ContextMenuItem } from "./CanvasContextMenu.tsx";

export type {
  CanvasContextAction,
  ContextActionRequest,
  Generation,
  HierarchyCanvasHandle,
  LayoutBatch,
  LayoutFetcher,
  LayoutKind,
  LayoutRequest,
  NodeDescriber,
  NodeDescription,
  SelectionChange,
  SelectionSource,
  SizeMetric,
  Viewport,
} from "./types.ts";
export { LAYOUT_KINDS } from "./types.ts";

export { decodeLayoutBatch, emptyLayoutBatch } from "./arrow.ts";
export type { LayoutExpectation } from "./arrow.ts";

export { LayoutError, isLayoutError, describeTransportFailure } from "./errors.ts";
export type { LayoutErrorCode } from "./errors.ts";

export { invokeLayout, serializeLayoutKind } from "./layoutSource.ts";

export { normalizeGeneration, isGenerationNone, GENERATION_NONE } from "./generation.ts";

export {
  computeSunburstFrame,
  hitTest,
  hitTestRect,
  hitTestSunburst,
  indexOfNode,
  layoutKindLabel,
  rankLargestTiles,
  rootMagnitude,
  tileMagnitude,
  NO_HIT,
} from "./geometry.ts";
export type { RankedTile, SunburstFrame } from "./geometry.ts";

export { categoryKey, categoryLabel, resolvePalette } from "./palette.ts";
export type { Palette } from "./palette.ts";

export { formatIec, formatPercent, formatShare, formatSi } from "./format.ts";

export { drawLayout, prepareContext } from "./render.ts";
export type { DrawOptions, DrawStats } from "./render.ts";

export {
  LAYOUT_COLUMNS,
  LAYOUT_SCHEMA_NAME,
  LAYOUT_SCHEMA_VERSION,
  MIN_TILE_PX,
  NODE_ID_NONE,
  NODE_ID_ROOT,
  PROTOCOL_VERSION,
  VIRTUAL_GROUP_BIT,
  groupOwner,
  isRealNode,
  isValidNodeId,
  isVirtualGroup,
} from "./contract.ts";
