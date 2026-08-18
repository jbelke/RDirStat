/**
 * A folder path, with a button that opens the one macOS actually trusts.
 *
 * The field started as a bare text input in three places — sync source and
 * destination, the snapshot store location, the move dialog's destination —
 * and a bare text input asks the user to have memorised a path, which nobody
 * has. Worse, it is not equivalent to picking one: docs/03-MACOS.md names the
 * native open panel as the explicit-consent path for folders, so a directory
 * chosen through Browse is one this process has been authorised to read while
 * the same characters typed by hand carry no such grant. The button is
 * therefore the primary control and the text is the fallback, not the reverse.
 *
 * Typing is still allowed, deliberately. Pasting a path from a terminal is a
 * real workflow, and a picker-only field cannot express "the folder I am about
 * to create" or a path on a volume the panel is slow to enumerate.
 *
 * The panel opens at whatever is currently in the field, so Browse refines an
 * existing answer rather than restarting from the home folder. A cancelled
 * panel changes nothing — it does not clear the field, because "I looked and
 * changed my mind" is not "I meant to erase this".
 */

import { Folder } from "lucide-react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { pickFolder } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface PathFieldProps {
  /** Shown beside the input, and used as the chooser's window title. */
  label: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  /** Disables both the input and the button. */
  disabled?: boolean;
  /**
   * `"inline"` puts the label to the left on one row — the sync panel's shape.
   * `"stacked"` puts it above, for narrower columns.
   */
  layout?: "inline" | "stacked";
  /** Extra guidance under the field. */
  hint?: React.ReactNode;
  /** Rendered in place of {@link hint} when present. */
  error?: string | null;
  className?: string;
  inputId?: string;
  /** Extra controls after the Browse button — "Use this", "Default", and such. */
  children?: React.ReactNode;
}

export function PathField({
  label,
  value,
  onChange,
  placeholder,
  disabled = false,
  layout = "inline",
  hint,
  error = null,
  className,
  inputId,
  children,
}: PathFieldProps) {
  // Guards against a second panel while one is open. The plugin tolerates it,
  // but two stacked choosers over one field is not a thing to hand a user.
  const [choosing, setChoosing] = useState(false);

  async function browse() {
    if (disabled || choosing) return;
    setChoosing(true);
    try {
      const chosen = await pickFolder(`Choose the ${label.toLowerCase()} folder`, value);
      if (chosen !== null) onChange(chosen);
    } finally {
      setChoosing(false);
    }
  }

  const input = (
    <input
      id={inputId}
      type="text"
      value={value}
      placeholder={placeholder}
      spellCheck={false}
      disabled={disabled}
      onChange={(event) => onChange(event.target.value)}
      className="min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 font-mono text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-60"
    />
  );

  const controls = (
    <div className="flex min-w-0 flex-1 items-center gap-2">
      {input}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="shrink-0"
        disabled={disabled || choosing}
        onClick={() => void browse()}
      >
        <Folder aria-hidden />
        Browse…
      </Button>
      {children}
    </div>
  );

  return (
    <div className={cn("flex flex-col gap-1", className)}>
      {layout === "inline" ? (
        <div className="flex items-center gap-2">
          <label className="w-20 shrink-0 text-xs text-muted-foreground" htmlFor={inputId}>
            {label}
          </label>
          {controls}
        </div>
      ) : (
        <>
          <label className="text-xs text-muted-foreground" htmlFor={inputId}>
            {label}
          </label>
          {controls}
        </>
      )}
      {error !== null ? (
        <p className="text-xs text-destructive">{error}</p>
      ) : hint !== undefined ? (
        <p className="text-xs text-muted-foreground">{hint}</p>
      ) : null}
    </div>
  );
}
