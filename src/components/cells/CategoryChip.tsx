/**
 * The category chip: swatch + name.
 *
 * docs/05-UI.md: "the color swatch from the categorizer, with the name, so the
 * treemap's colors are learnable from the table."
 *
 * The name is not optional decoration — it is the accessibility contract.
 * docs/05 requires "no-colour-only meaning", so a swatch on its own is not
 * allowed to be the sole carrier of the category. `compact` shrinks the text,
 * it never removes it; the swatch-only variant would be a defect.
 */

import { categoryColorVar, categoryOf } from "@/lib/categories";
import { cn } from "@/lib/utils";

export interface CategoryChipProps {
  /** `CategoryId` as sent by Rust. Unknown indices render as `Category N`. */
  category: number;
  compact?: boolean;
  className?: string;
}

export function CategoryChip({ category, compact = false, className }: CategoryChipProps) {
  const entry = categoryOf(category);
  return (
    <span
      className={cn(
        "inline-flex min-w-0 items-center gap-1.5 rounded-full border border-border/60",
        compact ? "px-1.5 py-0 text-[11px]" : "px-2 py-0.5 text-xs",
        className,
      )}
      title={`${entry.label} · ${entry.family}`}
    >
      <span
        aria-hidden
        className="size-2 shrink-0 rounded-[2px]"
        style={{ backgroundColor: categoryColorVar(category) }}
      />
      <span className="truncate">{entry.label}</span>
    </span>
  );
}

/** The swatch alone, for places that already state the category in text. */
export function CategorySwatch({ category, className }: { category: number; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn("inline-block size-2.5 shrink-0 rounded-[2px]", className)}
      style={{ backgroundColor: categoryColorVar(category) }}
    />
  );
}
