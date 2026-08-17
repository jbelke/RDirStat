/* =============================================================================
 * Segmented control: treemap | icicle | sunburst.
 *
 * Hand-rolled rather than pulled from `src/components/ui`, because this file
 * must compile whether or not the shadcn `ToggleGroup` has been generated yet.
 * Semantics follow the WAI radio-group pattern: one tab stop for the group,
 * arrow keys move AND select, which is what macOS segmented controls do.
 * ========================================================================== */

import { useRef } from "react";

import { cn } from "@/lib/utils";

import { layoutKindLabel } from "./geometry.ts";
import { LAYOUT_KINDS, type LayoutKind } from "./types.ts";

export interface LayoutKindToggleProps {
  readonly value: LayoutKind;
  readonly onChange: (kind: LayoutKind) => void;
  readonly disabled?: boolean;
  readonly className?: string;
}

export function LayoutKindToggle({ value, onChange, disabled = false, className }: LayoutKindToggleProps) {
  const groupRef = useRef<HTMLDivElement | null>(null);

  const move = (delta: number): void => {
    const current = LAYOUT_KINDS.indexOf(value);
    const next = LAYOUT_KINDS[(current + delta + LAYOUT_KINDS.length) % LAYOUT_KINDS.length];
    if (next === undefined) return;
    onChange(next);
    const buttons = groupRef.current?.querySelectorAll<HTMLButtonElement>("button[role='radio']");
    buttons?.item(LAYOUT_KINDS.indexOf(next))?.focus();
  };

  return (
    <div
      ref={groupRef}
      role="radiogroup"
      aria-label="Hierarchy layout"
      className={cn("inline-flex items-center gap-0.5 rounded-md border border-border bg-muted/40 p-0.5", className)}
      onKeyDown={(event) => {
        if (disabled) return;
        if (event.key === "ArrowRight" || event.key === "ArrowDown") {
          event.preventDefault();
          move(1);
        } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
          event.preventDefault();
          move(-1);
        }
      }}
    >
      {LAYOUT_KINDS.map((kind) => {
        const selected = kind === value;
        return (
          <button
            key={kind}
            type="button"
            role="radio"
            aria-checked={selected}
            disabled={disabled}
            tabIndex={selected ? 0 : -1}
            onClick={() => onChange(kind)}
            className={cn(
              "rounded-sm px-2.5 py-1 text-xs font-medium transition-colors",
              "focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring",
              "motion-reduce:transition-none",
              selected ? "bg-background text-foreground shadow-sm" : "text-muted-foreground hover:text-foreground",
              disabled && "cursor-not-allowed opacity-50",
            )}
          >
            {layoutKindLabel(kind)}
          </button>
        );
      })}
    </div>
  );
}
