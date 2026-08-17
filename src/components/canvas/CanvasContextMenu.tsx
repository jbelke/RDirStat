/* =============================================================================
 * Right-click menu over the canvas: Zoom / Reveal / Copy Path / Trash.
 *
 * Hand-rolled for the same reason as the segmented control — no dependency on
 * generated shadcn files. Keyboard semantics follow the WAI menu pattern:
 * arrows wrap, Home/End jump, Escape closes and returns focus, and disabled
 * items stay focusable so a screen reader can hear WHY they are disabled.
 *
 * docs/05-UI.md: destructive actions live in the details panel and the context
 * menu only, and the virtual `<Files>` group "cannot be revealed or trashed as
 * a path" — enforced by `disabled` here, not by the caller remembering.
 * ========================================================================== */

import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { cn } from "@/lib/utils";

import type { CanvasContextAction } from "./types.ts";

export interface ContextMenuItem {
  readonly action: CanvasContextAction;
  readonly label: string;
  readonly disabled: boolean;
  readonly disabledReason?: string;
  readonly destructive?: boolean;
}

export interface CanvasContextMenuProps {
  /** Viewport coordinates of the click. */
  readonly x: number;
  readonly y: number;
  readonly items: readonly ContextMenuItem[];
  readonly onAction: (action: CanvasContextAction) => void;
  readonly onClose: () => void;
}

export function CanvasContextMenu({ x, y, items, onAction, onClose }: CanvasContextMenuProps) {
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [position, setPosition] = useState({ left: x, top: y });
  const [active, setActive] = useState(() => items.findIndex((item) => !item.disabled));

  // Flip the menu back inside the window rather than letting it clip.
  useLayoutEffect(() => {
    const element = menuRef.current;
    if (element === null) return;
    const rect = element.getBoundingClientRect();
    const maxLeft = window.innerWidth - rect.width - 8;
    const maxTop = window.innerHeight - rect.height - 8;
    setPosition({ left: Math.max(8, Math.min(x, maxLeft)), top: Math.max(8, Math.min(y, maxTop)) });
    element.focus({ preventScroll: true });
  }, [x, y]);

  useEffect(() => {
    const onPointerDown = (event: PointerEvent): void => {
      if (menuRef.current?.contains(event.target as Node) === true) return;
      onClose();
    };
    const onWindowBlur = (): void => onClose();
    window.addEventListener("pointerdown", onPointerDown, true);
    window.addEventListener("blur", onWindowBlur);
    window.addEventListener("resize", onWindowBlur);
    return () => {
      window.removeEventListener("pointerdown", onPointerDown, true);
      window.removeEventListener("blur", onWindowBlur);
      window.removeEventListener("resize", onWindowBlur);
    };
  }, [onClose]);

  const step = (delta: number): void => {
    if (items.length === 0) return;
    let index = active;
    for (let attempt = 0; attempt < items.length; attempt += 1) {
      index = (index + delta + items.length) % items.length;
      const candidate = items[index];
      if (candidate !== undefined && !candidate.disabled) {
        setActive(index);
        return;
      }
    }
  };

  return (
    <div
      ref={menuRef}
      role="menu"
      tabIndex={-1}
      aria-label="Hierarchy item actions"
      style={{ left: position.left, top: position.top }}
      className={cn(
        "fixed z-50 min-w-44 rounded-md border border-border bg-popover p-1 text-popover-foreground shadow-lg",
        "focus:outline-none",
      )}
      onKeyDown={(event) => {
        switch (event.key) {
          case "Escape":
            event.preventDefault();
            onClose();
            break;
          case "ArrowDown":
            event.preventDefault();
            step(1);
            break;
          case "ArrowUp":
            event.preventDefault();
            step(-1);
            break;
          case "Home":
            event.preventDefault();
            setActive(items.findIndex((item) => !item.disabled));
            break;
          case "End":
            event.preventDefault();
            setActive(items.length - 1);
            step(0);
            break;
          case "Enter":
          case " ": {
            event.preventDefault();
            const item = items[active];
            if (item !== undefined && !item.disabled) onAction(item.action);
            break;
          }
          default:
            break;
        }
      }}
    >
      {items.map((item, index) => (
        <button
          key={item.action}
          type="button"
          role="menuitem"
          aria-disabled={item.disabled}
          aria-describedby={item.disabled && item.disabledReason !== undefined ? `canvas-menu-reason-${item.action}` : undefined}
          tabIndex={-1}
          onMouseEnter={() => {
            if (!item.disabled) setActive(index);
          }}
          onClick={() => {
            if (item.disabled) return;
            onAction(item.action);
          }}
          className={cn(
            "flex w-full items-center justify-between rounded-sm px-2 py-1.5 text-left text-sm",
            item.disabled && "cursor-not-allowed opacity-50",
            !item.disabled && index === active && "bg-accent text-accent-foreground",
            item.destructive === true && !item.disabled && "text-destructive",
          )}
        >
          <span>{item.label}</span>
          {item.disabled && item.disabledReason !== undefined ? (
            <span id={`canvas-menu-reason-${item.action}`} className="ml-3 text-[10px] text-muted-foreground">
              {item.disabledReason}
            </span>
          ) : null}
        </button>
      ))}
    </div>
  );
}
