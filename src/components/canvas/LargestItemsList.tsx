/* =============================================================================
 * The accessible equivalent of the canvas.
 *
 * docs/05-UI.md#accessibility-and-performance: "Each hierarchy view has an
 * adjacent accessible list of the largest currently drawn items; focus,
 * selection, and hover are distinct; and keyboard navigation supports arrows,
 * Return, Space, Command, and Shift with macOS semantics. VoiceOver names
 * include path, logical/allocated value, share, category, and state."
 *
 * A `<canvas>` is one opaque node to a screen reader, so this list is not a
 * courtesy: it is the only way the hierarchy view is operable without a mouse.
 * It is built from exactly the tiles that were painted, so it can never
 * describe something that is not on screen.
 *
 * Keys: Up/Down move focus, Home/End jump, Return selects (⌘/⇧ extend),
 * Space toggles, Right drills into the item, Left goes back up.
 * ========================================================================== */

import { useEffect, useRef } from "react";

import { cn } from "@/lib/utils";

import { formatShare, formatSi } from "./format.ts";
import type { RankedTile } from "./geometry.ts";
import { categoryLabel } from "./palette.ts";
import type { Palette } from "./palette.ts";
import type { NodeDescriptionStore } from "./useNodeDescriptions.ts";
import { groupOwner, isVirtualGroup } from "./contract.ts";

export interface LargestItemsListProps {
  readonly items: readonly RankedTile[];
  readonly descriptions: NodeDescriptionStore;
  readonly selected: ReadonlySet<number>;
  readonly palette: Palette;
  readonly focusedNode: number | null;
  readonly onFocusNode: (node: number) => void;
  readonly onSelect: (node: number, additive: boolean) => void;
  readonly onZoomIn: (node: number) => void;
  readonly onZoomOut: () => void;
  readonly formatBytes?: (bytes: number) => string;
  /** `sr-only` until it receives focus (default), or always visible. */
  readonly presentation?: "auto" | "visible";
  readonly className?: string;
}

function itemName(node: number, described: { name?: string; path?: string } | undefined): string {
  if (described?.name !== undefined && described.name.length > 0) return described.name;
  if (described?.path !== undefined && described.path.length > 0) {
    const trimmed = described.path.replace(/\/+$/, "");
    const slash = trimmed.lastIndexOf("/");
    if (slash >= 0 && slash + 1 < trimmed.length) return trimmed.slice(slash + 1);
    if (trimmed.length > 0) return trimmed;
  }
  const owner = groupOwner(node);
  if (owner !== null) return "‹files›";
  return `Item ${node}`;
}

export function LargestItemsList({
  items,
  descriptions,
  selected,
  palette,
  focusedNode,
  onFocusNode,
  onSelect,
  onZoomIn,
  onZoomOut,
  formatBytes = formatSi,
  presentation = "auto",
  className,
}: LargestItemsListProps) {
  const listRef = useRef<HTMLUListElement | null>(null);

  // Resolve the visible rows only. The list is capped, so this is a bounded
  // number of `path_of` calls per navigation, not one per node.
  useEffect(() => {
    for (const item of items) descriptions.request(item.node);
  }, [items, descriptions]);

  const focusedIndex = Math.max(
    0,
    items.findIndex((item) => item.node === focusedNode),
  );

  const focusRow = (index: number): void => {
    const clamped = Math.max(0, Math.min(index, items.length - 1));
    const target = items[clamped];
    if (target === undefined) return;
    onFocusNode(target.node);
    listRef.current?.querySelectorAll<HTMLLIElement>("li[role='option']").item(clamped)?.focus();
  };

  return (
    <div
      className={cn(
        presentation === "auto" && "sr-only focus-within:not-sr-only focus-within:absolute focus-within:inset-x-0 focus-within:bottom-0 focus-within:z-40",
        "bg-popover text-popover-foreground",
        className,
      )}
    >
      <h3 id="canvas-largest-heading" className="px-2 pt-2 text-xs font-medium text-muted-foreground">
        Largest items in view
      </h3>
      <ul
        ref={listRef}
        role="listbox"
        aria-labelledby="canvas-largest-heading"
        aria-multiselectable
        className="max-h-64 overflow-y-auto p-1"
      >
        {items.length === 0 ? (
          <li role="option" aria-selected={false} aria-disabled className="px-2 py-1.5 text-sm text-muted-foreground">
            Nothing is drawn.
          </li>
        ) : null}
        {items.map((item, index) => {
          const described = descriptions.get(item.node);
          const name = itemName(item.node, described);
          const isSelected = selected.has(item.node);
          const category = categoryLabel(item.category);
          const share = formatShare(item.share);
          const sizes =
            described?.logical === undefined
              ? null
              : `${formatBytes(described.logical)} logical, ${
                  described.allocated === undefined ? "allocated unknown" : `${formatBytes(described.allocated)} allocated`
                }`;
          const label = [
            described?.path ?? name,
            sizes ?? "size not loaded",
            `${share} of view`,
            category,
            `depth ${item.depth}`,
            isVirtualGroup(item.node) ? "file group, not a path" : null,
            isSelected ? "selected" : null,
          ]
            .filter((part): part is string => part !== null)
            .join(", ");

          return (
            <li
              key={`${item.node}-${item.index}`}
              role="option"
              aria-selected={isSelected}
              aria-label={label}
              tabIndex={index === focusedIndex ? 0 : -1}
              onFocus={() => onFocusNode(item.node)}
              onClick={(event) => onSelect(item.node, event.metaKey || event.shiftKey)}
              onDoubleClick={() => onZoomIn(item.node)}
              onKeyDown={(event) => {
                switch (event.key) {
                  case "ArrowDown":
                    event.preventDefault();
                    focusRow(index + 1);
                    break;
                  case "ArrowUp":
                    event.preventDefault();
                    focusRow(index - 1);
                    break;
                  case "Home":
                    event.preventDefault();
                    focusRow(0);
                    break;
                  case "End":
                    event.preventDefault();
                    focusRow(items.length - 1);
                    break;
                  case "Enter":
                    event.preventDefault();
                    onSelect(item.node, event.metaKey || event.shiftKey);
                    break;
                  case " ":
                    event.preventDefault();
                    onSelect(item.node, true);
                    break;
                  case "ArrowRight":
                    event.preventDefault();
                    onZoomIn(item.node);
                    break;
                  case "ArrowLeft":
                    event.preventDefault();
                    onZoomOut();
                    break;
                  default:
                    break;
                }
              }}
              className={cn(
                "flex cursor-default items-center gap-2 rounded-sm px-2 py-1 text-sm outline-none",
                "focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
                isSelected && "bg-accent text-accent-foreground",
              )}
            >
              <span
                aria-hidden
                className="size-2.5 shrink-0 rounded-[2px]"
                style={{ backgroundColor: palette.fills[item.category] }}
              />
              <span className="min-w-0 flex-1 truncate">{name}</span>
              <span className="shrink-0 tabular-nums text-xs text-muted-foreground">{share}</span>
              {described?.allocated === undefined ? null : (
                <span className="shrink-0 tabular-nums text-xs">{formatBytes(described.allocated)}</span>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
