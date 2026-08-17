/**
 * A radio group that looks like a macOS segmented control.
 *
 * Two places in docs/05-UI.md call for exactly this, and both are about
 * removing ambiguity rather than saving space:
 *
 * - The layout toggle: "three layouts off one Rust-computed buffer, toggled in
 *   a segmented control."
 * - Logical vs allocated: "`pdu` surfaces it as `--quantity`; we surface it as
 *   a segmented control in the toolbar that retitles the size columns, so a
 *   screenshot is never ambiguous about which number it is showing."
 *
 * Built on native radio inputs rather than buttons so arrow-key navigation,
 * grouping, and VoiceOver's "1 of 3" announcement come for free. A row of
 * `<button aria-pressed>` looks identical and behaves worse.
 */

import { useId } from "react";

import { cn } from "@/lib/utils";

export interface SegmentedOption<T extends string> {
  readonly value: T;
  readonly label: string;
  readonly title?: string;
}

export interface SegmentedControlProps<T extends string> {
  label: string;
  options: readonly SegmentedOption<T>[];
  value: T;
  onChange: (value: T) => void;
  className?: string;
}

export function SegmentedControl<T extends string>({
  label,
  options,
  value,
  onChange,
  className,
}: SegmentedControlProps<T>) {
  const name = useId();
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className={cn("inline-flex items-center rounded-md border border-border p-0.5", className)}
    >
      {options.map((option) => {
        const id = `${name}-${option.value}`;
        const selected = option.value === value;
        return (
          <label
            key={option.value}
            htmlFor={id}
            title={option.title}
            className={cn(
              "cursor-default rounded-[5px] px-2.5 py-1 text-xs transition-colors",
              "focus-within:ring-2 focus-within:ring-ring",
              selected
                ? "bg-brand text-brand-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            <input
              id={id}
              type="radio"
              name={name}
              value={option.value}
              checked={selected}
              onChange={() => onChange(option.value)}
              className="sr-only"
            />
            {option.label}
          </label>
        );
      })}
    </div>
  );
}
