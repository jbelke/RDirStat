/* =============================================================================
 * The hierarchy canvas — one <canvas>, three layouts, thousands of tiles.
 *
 * docs/01-ARCHITECTURE.md#frontend-boundary: "The hierarchy views are one
 * <canvas> fed by the Arrow tile batch — not SVG, not DOM nodes, not Recharts."
 *
 * Repaint policy, which is the whole performance argument:
 *   - repaint on batch change, resize, navigation, selection and theme change;
 *   - NEVER on mousemove (hover is a DOM overlay) and NEVER on a frame tick;
 *   - hit-testing is a reverse linear scan over the typed arrays, so hover costs
 *     one pass over `count` floats and zero allocations.
 *
 * Failure policy: any decode or transport error clears the batch AND the canvas
 * and shows an alert. There is no partial render.
 * ========================================================================== */

import { useCallback, useEffect, useImperativeHandle, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent, Ref } from "react";

import { cn } from "@/lib/utils";

import { CanvasContextMenu, type ContextMenuItem } from "./CanvasContextMenu.tsx";
import { CanvasTooltip } from "./CanvasTooltip.tsx";
import { LargestItemsList } from "./LargestItemsList.tsx";
import { LayoutKindToggle } from "./LayoutKindToggle.tsx";
import { MIN_TILE_PX, isVirtualGroup } from "./contract.ts";
import { formatSi } from "./format.ts";
import { NO_HIT, computeSunburstFrame, hitTest, layoutKindLabel, rankLargestTiles, rootMagnitude, tileMagnitude } from "./geometry.ts";
import { normalizeGeneration } from "./generation.ts";
import { useDevicePixelRatio, useElementSize, usePalette, usePrefersReducedMotion } from "./hooks.ts";
import { invokeLayout } from "./layoutSource.ts";
import { categoryLabel, type ColorBy } from "./palette.ts";
import { drawLayout, prepareContext, type DrawStats } from "./render.ts";
import type {
  CanvasContextAction,
  ContextActionRequest,
  Generation,
  HierarchyCanvasHandle,
  LayoutFetcher,
  LayoutKind,
  NodeDescriber,
  SelectionChange,
  SelectionSource,
} from "./types.ts";
import { useLayoutBatch } from "./useLayoutBatch.ts";
import { useNodeDescriptions } from "./useNodeDescriptions.ts";

export interface PaintReport extends DrawStats {
  readonly kind: LayoutKind;
  readonly tiles: number;
  readonly width: number;
  readonly height: number;
  readonly devicePixelRatio: number;
  /** Fetch + decode milliseconds for the batch that was painted. */
  readonly loadMs: number | null;
}

export interface HierarchyCanvasProps {
  /** `TreeGeneration` of the scan on screen. A change invalidates everything. */
  readonly generation: Generation;
  /** Subtree the view starts at. Changing it resets the navigation stack. */
  readonly root: number;

  /** Controlled layout kind. Omit to let the built-in segmented control own it. */
  readonly kind?: LayoutKind;
  readonly defaultKind?: LayoutKind;
  readonly onKindChange?: (kind: LayoutKind) => void;
  readonly showToggle?: boolean;

  /** Sub-pixel cutoff handed to the backend. Defaults to `MIN_TILE_PX` (3). */
  readonly minPx?: number;

  /** Transport seam. Defaults to `invoke("layout", …)`. */
  readonly fetchLayout?: LayoutFetcher;
  /** Resolves node ids to path/size for the tooltip and the accessible list. */
  readonly describeNode?: NodeDescriber;

  /** Controlled selection — this is the tree -> canvas half of the sync. */
  readonly selectedNodes?: readonly number[];
  /** Canvas -> tree half. Fires for every selection change the canvas makes. */
  readonly onSelectionChange?: (change: SelectionChange) => void;

  readonly onContextAction?: (request: ContextActionRequest) => void;
  /**
   * Whether "Move to Trash…" is offered at all. Defaults to `false`: the canvas
   * is a view, and it does not get to decide that deletion is on. The shell
   * owns the arming switch and passes it down.
   */
  readonly trashEnabled?: boolean;
  /**
   * What a tile's fill encodes. `family` collapses the 25-entry taxonomy onto
   * docs/04's five headings, which stay distinguishable at tile size;
   * `category` keeps the full palette, which is the useful one once a
   * drill-down has cut the categories on screen down to a handful.
   */
  readonly colorBy?: ColorBy;
  /**
   * `CategoryId`s to keep at full strength; everything else paints dimmed.
   * `null` (the default) paints everything normally.
   *
   * This is the *immediate* half of category filtering — the tiles keep their
   * geometry and lose their colour. Re-proportioning the layout so the
   * filtered bytes own the whole rectangle is a backend concern and arrives
   * separately; the two compose, because this needs no round trip and so gives
   * the click instant feedback.
   */
  readonly categoryFilter?: readonly number[] | null;
  readonly onNavigate?: (node: number, stack: readonly number[]) => void;
  readonly onPaint?: (report: PaintReport) => void;

  readonly formatBytes?: (bytes: number) => string;
  readonly largestItemsLimit?: number;
  readonly accessibleListPresentation?: "auto" | "visible";

  readonly className?: string;
  readonly ref?: Ref<HierarchyCanvasHandle>;
}

interface HoverState {
  readonly index: number;
  readonly node: number;
}

interface MenuState {
  readonly x: number;
  readonly y: number;
  readonly node: number;
}

const EMPTY_SELECTION: readonly number[] = [];

export function HierarchyCanvas({
  generation,
  root,
  kind: controlledKind,
  defaultKind = "treemap",
  onKindChange,
  showToggle = true,
  minPx = MIN_TILE_PX,
  fetchLayout = invokeLayout,
  describeNode,
  selectedNodes,
  onSelectionChange,
  onContextAction,
  trashEnabled = false,
  colorBy = "category",
  categoryFilter = null,
  onNavigate,
  onPaint,
  formatBytes = formatSi,
  largestItemsLimit = 25,
  accessibleListPresentation = "auto",
  className,
  ref,
}: HierarchyCanvasProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const tooltipRef = useRef<HTMLDivElement | null>(null);
  const hoverIndexRef = useRef<number>(NO_HIT);

  const size = useElementSize(surfaceRef);
  const devicePixelRatio = useDevicePixelRatio();
  // A stable Set identity, or `usePalette`'s effect re-runs every render.
  const filterSet = useMemo(
    () => (categoryFilter === null || categoryFilter.length === 0 ? null : new Set(categoryFilter)),
    [categoryFilter],
  );
  const palette = usePalette(colorBy, filterSet);
  const reducedMotion = usePrefersReducedMotion();

  const [uncontrolledKind, setUncontrolledKind] = useState<LayoutKind>(defaultKind);
  const kind = controlledKind ?? uncontrolledKind;

  const [stack, setStack] = useState<number[]>([root]);
  useEffect(() => {
    setStack([root]);
  }, [root]);
  const currentRoot = stack[stack.length - 1] ?? root;

  const [internalSelection, setInternalSelection] = useState<readonly number[]>(EMPTY_SELECTION);
  const selection = selectedNodes ?? internalSelection;
  const selectionSet = useMemo(() => new Set(selection), [selection]);

  const [hover, setHover] = useState<HoverState | null>(null);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [focusedNode, setFocusedNode] = useState<number | null>(null);

  const normalizedGeneration = useMemo(() => {
    try {
      return normalizeGeneration(generation);
    } catch {
      return "0";
    }
  }, [generation]);

  const { batch, error, loading, loadMs, refetch } = useLayoutBatch({
    generation,
    root: currentRoot,
    kind,
    width: size.width,
    height: size.height,
    devicePixelRatio,
    minPx,
    fetchLayout,
    enabled: size.width > 0 && size.height > 0,
  });

  const descriptions = useNodeDescriptions(describeNode, normalizedGeneration);

  const frame = useMemo(
    () => (batch !== null && batch.kind === "sunburst" ? computeSunburstFrame(batch, size.width, size.height) : null),
    [batch, size.width, size.height],
  );

  const ranked = useMemo(() => (batch === null ? [] : rankLargestTiles(batch, largestItemsLimit)), [batch, largestItemsLimit]);

  /* ---- paint ------------------------------------------------------------- */

  const onPaintRef = useRef(onPaint);
  onPaintRef.current = onPaint;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null || size.width <= 0 || size.height <= 0) return;

    if (batch === null) {
      // Fail closed: never leave the previous scan's tiles on screen.
      const context = prepareContext(canvas, size.width, size.height, devicePixelRatio);
      if (context !== null) {
        context.fillStyle = palette.background;
        context.fillRect(0, 0, size.width, size.height);
      }
      return;
    }

    const stats = drawLayout(canvas, {
      batch,
      palette,
      width: size.width,
      height: size.height,
      devicePixelRatio,
      selected: selectionSet,
      frame,
    });
    if (stats !== null) {
      onPaintRef.current?.({
        ...stats,
        kind: batch.kind,
        tiles: batch.count,
        width: size.width,
        height: size.height,
        devicePixelRatio,
        loadMs,
      });
    }
  }, [batch, palette, size.width, size.height, devicePixelRatio, selectionSet, frame, loadMs]);

  /* ---- selection --------------------------------------------------------- */

  const emitSelection = useCallback(
    (nodes: readonly number[], primary: number | null, additive: boolean, source: SelectionSource): void => {
      if (selectedNodes === undefined) setInternalSelection(nodes);
      onSelectionChange?.({ nodes, primary, additive, source });
    },
    [onSelectionChange, selectedNodes],
  );

  const selectNode = useCallback(
    (node: number, additive: boolean, source: SelectionSource): void => {
      if (!additive) {
        emitSelection([node], node, false, source);
        return;
      }
      const next = selection.includes(node) ? selection.filter((entry) => entry !== node) : [...selection, node];
      emitSelection(next, node, true, source);
    },
    [emitSelection, selection],
  );

  /* ---- navigation -------------------------------------------------------- */

  const zoomTo = useCallback(
    (node: number): void => {
      if (isVirtualGroup(node)) return; // a <Files> group is not a layout root
      setStack((previous) => {
        if (previous[previous.length - 1] === node) return previous;
        const next = [...previous, node];
        onNavigate?.(node, next);
        return next;
      });
    },
    [onNavigate],
  );

  const goBack = useCallback((): void => {
    setStack((previous) => {
      if (previous.length <= 1) return previous;
      const next = previous.slice(0, -1);
      const target = next[next.length - 1];
      if (target !== undefined) onNavigate?.(target, next);
      return next;
    });
  }, [onNavigate]);

  const resetNavigation = useCallback((): void => {
    setStack((previous) => {
      if (previous.length <= 1) return previous;
      const next = [previous[0] ?? root];
      onNavigate?.(next[0] ?? root, next);
      return next;
    });
  }, [onNavigate, root]);

  const redraw = useCallback((): void => {
    const canvas = canvasRef.current;
    if (canvas === null || batch === null || size.width <= 0) return;
    drawLayout(canvas, {
      batch,
      palette,
      width: size.width,
      height: size.height,
      devicePixelRatio,
      selected: selectionSet,
      frame,
    });
  }, [batch, palette, size.width, size.height, devicePixelRatio, selectionSet, frame]);

  useImperativeHandle(
    ref,
    (): HierarchyCanvasHandle => ({
      back: goBack,
      reset: resetNavigation,
      zoomTo,
      stack,
      redraw,
      refetch,
    }),
    [goBack, resetNavigation, zoomTo, stack, redraw, refetch],
  );

  /* ---- pointer ----------------------------------------------------------- */

  const pick = useCallback(
    (event: { clientX: number; clientY: number }): { index: number; px: number; py: number } => {
      const canvas = canvasRef.current;
      if (canvas === null || batch === null) return { index: NO_HIT, px: 0, py: 0 };
      const rect = canvas.getBoundingClientRect();
      const px = event.clientX - rect.left;
      const py = event.clientY - rect.top;
      return { index: hitTest(batch, frame, px, py), px, py };
    },
    [batch, frame],
  );

  const handlePointerMove = useCallback(
    (event: ReactPointerEvent<HTMLCanvasElement>): void => {
      const { index, px, py } = pick(event);

      const tip = tooltipRef.current;
      if (tip !== null) {
        // Imperative: following the cursor must not cost a React render.
        const width = tip.offsetWidth;
        const height = tip.offsetHeight;
        const left = px + 14 + width > size.width ? Math.max(0, px - 14 - width) : px + 14;
        const top = py + 16 + height > size.height ? Math.max(0, py - 16 - height) : py + 16;
        tip.style.transform = `translate3d(${left}px, ${top}px, 0)`;
      }

      if (index === hoverIndexRef.current) return;
      hoverIndexRef.current = index;
      if (index === NO_HIT || batch === null) {
        setHover(null);
        return;
      }
      const node = batch.node[index];
      setHover({ index, node });
      descriptions.request(node);
    },
    [batch, descriptions, pick, size.height, size.width],
  );

  const handlePointerLeave = useCallback((): void => {
    hoverIndexRef.current = NO_HIT;
    setHover(null);
  }, []);

  const handleClick = useCallback(
    (event: ReactMouseEvent<HTMLCanvasElement>): void => {
      if (batch === null) return;
      const { index } = pick(event);
      const additive = event.metaKey || event.shiftKey;
      if (index === NO_HIT) {
        if (!additive) emitSelection(EMPTY_SELECTION, null, false, "canvas");
        return;
      }
      const node = batch.node[index];
      setFocusedNode(node);
      selectNode(node, additive, "canvas");
    },
    [batch, emitSelection, pick, selectNode],
  );

  const handleDoubleClick = useCallback(
    (event: ReactMouseEvent<HTMLCanvasElement>): void => {
      if (batch === null) return;
      const { index } = pick(event);
      if (index === NO_HIT) return;
      zoomTo(batch.node[index]);
    },
    [batch, pick, zoomTo],
  );

  const handleContextMenu = useCallback(
    (event: ReactMouseEvent<HTMLCanvasElement>): void => {
      event.preventDefault();
      if (batch === null) return;
      const { index } = pick(event);
      if (index === NO_HIT) {
        setMenu(null);
        return;
      }
      const node = batch.node[index];
      if (!selectionSet.has(node)) selectNode(node, false, "context-menu");
      descriptions.request(node);
      setMenu({ x: event.clientX, y: event.clientY, node });
    },
    [batch, descriptions, pick, selectNode, selectionSet],
  );

  const runContextAction = useCallback(
    (action: CanvasContextAction): void => {
      const target = menu?.node;
      setMenu(null);
      if (target === undefined) return;
      const nodes = selectionSet.has(target) ? selection : [target];

      if (action === "zoom") zoomTo(target);

      if (onContextAction !== undefined) {
        onContextAction({ action, nodes, primary: target });
        return;
      }
      if (action === "copy-path") {
        // Only a fallback: the shell owns `path_of`, which is the authority.
        const path = descriptions.get(target)?.path;
        if (path !== undefined && typeof navigator !== "undefined" && navigator.clipboard !== undefined) {
          void navigator.clipboard.writeText(path);
        }
      }
    },
    [descriptions, menu, onContextAction, selection, selectionSet, zoomTo],
  );

  /* ---- derived text ------------------------------------------------------ */

  const hoveredDescription = hover === null ? undefined : descriptions.get(hover.node);
  const hoveredCategory = hover === null || batch === null ? 0 : batch.category[hover.index];
  // The denominator for "share of view". Computed once per batch, not per hover.
  const viewMagnitude = useMemo(() => (batch === null ? 0 : rootMagnitude(batch)), [batch]);
  const hoveredShare =
    hover === null || batch === null || viewMagnitude <= 0 ? 0 : tileMagnitude(batch, hover.index) / viewMagnitude;

  const hoverTitle =
    hover === null
      ? ""
      : (hoveredDescription?.path ??
        hoveredDescription?.name ??
        (isVirtualGroup(hover.node) ? "‹files› (direct files in this folder)" : `Item ${hover.node}`));

  const menuItems: readonly ContextMenuItem[] = useMemo(() => {
    const node = menu?.node;
    const virtualGroup = node !== undefined && isVirtualGroup(node);
    const isScanRoot = node !== undefined && node === stack[0];
    const reason = virtualGroup ? "not a path" : undefined;
    return [
      { action: "zoom", label: "Zoom to Item", disabled: virtualGroup, disabledReason: reason },
      { action: "reveal", label: "Reveal in Finder", disabled: virtualGroup, disabledReason: reason },
      { action: "copy-path", label: "Copy Path", disabled: virtualGroup, disabledReason: reason },
      {
        action: "trash",
        label: "Move to Trash…",
        // `trashEnabled` is the app-wide arming switch. It is listed last of the
        // three reasons so the more specific ones still explain themselves.
        disabled: virtualGroup || isScanRoot || !trashEnabled,
        disabledReason: virtualGroup ? reason : isScanRoot ? "scan root" : !trashEnabled ? "deletion off" : undefined,
        destructive: true,
      },
    ];
  }, [menu, stack, trashEnabled]);

  const canvasLabel =
    batch === null
      ? `${layoutKindLabel(kind)} — no data`
      : `${layoutKindLabel(kind)} of ${batch.count} items. Use the largest-items list after this graphic to navigate by keyboard.`;

  return (
    <div ref={containerRef} className={cn("flex h-full min-h-0 w-full flex-col", className)}>
      {showToggle ? (
        <div className="flex items-center justify-between gap-2 px-2 py-1.5">
          <LayoutKindToggle
            value={kind}
            onChange={(next) => {
              if (controlledKind === undefined) setUncontrolledKind(next);
              onKindChange?.(next);
            }}
          />
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            {stack.length > 1 ? (
              <button
                type="button"
                onClick={goBack}
                className="rounded-sm border border-border px-2 py-1 hover:bg-accent hover:text-accent-foreground"
              >
                Back
              </button>
            ) : null}
            <span aria-live="polite" className="tabular-nums">
              {loading ? "Laying out…" : batch === null ? "" : `${batch.count} tiles`}
            </span>
          </div>
        </div>
      ) : null}

      <div ref={surfaceRef} className="relative min-h-0 flex-1 overflow-hidden">
        <canvas
          ref={canvasRef}
          role="img"
          aria-label={canvasLabel}
          className="block size-full"
          style={{ width: "100%", height: "100%", cursor: hover === null ? "default" : "pointer" }}
          onPointerMove={handlePointerMove}
          onPointerLeave={handlePointerLeave}
          onClick={handleClick}
          onDoubleClick={handleDoubleClick}
          onContextMenu={handleContextMenu}
        />

        <CanvasTooltip
          ref={tooltipRef}
          title={hoverTitle}
          categoryLabel={categoryLabel(hoveredCategory)}
          categoryColor={palette.fills[hoveredCategory] ?? "#6b7280"}
          logical={hoveredDescription?.logical === undefined ? null : formatBytes(hoveredDescription.logical)}
          allocated={hoveredDescription?.allocated === undefined ? null : formatBytes(hoveredDescription.allocated)}
          share={hoveredShare}
          visible={hover !== null && menu === null}
          reducedMotion={reducedMotion}
        />

        {error !== null ? (
          <div
            role="alert"
            className="absolute inset-0 z-40 flex flex-col items-center justify-center gap-3 bg-background/95 p-6 text-center"
          >
            <p className="text-sm font-medium text-destructive">{error.message}</p>
            <p className="max-w-lg text-xs text-muted-foreground">
              <span className="font-mono">{error.code}</span>
              {error.detail === undefined ? null : <> — {error.detail}</>}
            </p>
            <button
              type="button"
              onClick={refetch}
              className="rounded-md border border-border px-3 py-1.5 text-xs hover:bg-accent hover:text-accent-foreground"
            >
              Try again
            </button>
          </div>
        ) : null}

        {menu === null ? null : (
          <CanvasContextMenu x={menu.x} y={menu.y} items={menuItems} onAction={runContextAction} onClose={() => setMenu(null)} />
        )}

        <LargestItemsList
          items={ranked}
          descriptions={descriptions}
          selected={selectionSet}
          palette={palette}
          focusedNode={focusedNode}
          onFocusNode={setFocusedNode}
          onSelect={(node, additive) => selectNode(node, additive, "canvas-list")}
          onZoomIn={zoomTo}
          onZoomOut={goBack}
          formatBytes={formatBytes}
          presentation={accessibleListPresentation}
        />
      </div>
    </div>
  );
}
