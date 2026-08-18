/**
 * The path entry that starts a scan, and the app's primary way to name a root.
 *
 * Lives in the titlebar rather than on the volume picker because scanning an
 * arbitrary folder is not a property of the launch screen — it is the thing the
 * app does, and it should be reachable from wherever the user already is. The
 * volume picker still owns picking a *whole volume*; this owns everything else.
 *
 * Two constraints the markup below has to honour:
 *
 * 1. **The dropdown must stay inside the titlebar's drag-region opt-out.**
 *    `Titlebar` wraps its trailing actions in `data-tauri-drag-region="false"`,
 *    and that is load-bearing. The per-element opt-out in `index.css` covers a
 *    plain `onClick` (which fires on mouseup) but *not* anything that opens on
 *    pointerdown — the drag region consumes pointerdown first. Rendering these
 *    options in a portal would put them outside that container and the failure
 *    is silent and partial: hover works, focus works, and the list never opens.
 *
 * 2. **Completion is advisory, never authority.** The string in the input is
 *    what the user typed; `scan_start` canonicalises it in Rust and that result
 *    is the only root any later action trusts. Offering only real directories
 *    is a courtesy that keeps `~/x` and `~/x/` from becoming two scan histories,
 *    not a validation step this component is entitled to perform.
 */

import { FolderSearch, Loader, X } from "lucide-react";
import { useCallback, useEffect, useId, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { completePath } from "@/lib/ipc";
import { cn } from "@/lib/utils";

/**
 * Idle time before asking the filesystem for completions.
 *
 * Every keystroke is a `read_dir` on a directory that may hold forty thousand
 * entries. Typing a path is bursty — people type a whole segment, then pause to
 * look — so waiting out the burst costs nothing perceptible and removes most of
 * the calls.
 */
const DEBOUNCE_MS = 120;

export interface ScanBarProps {
  /** Called with the raw typed path. Canonicalisation happens in Rust. */
  onScan: (root: string) => void;
  /** A scan is already starting or running; the control refuses to start another. */
  busy?: boolean;
  /**
   * The drive on screen, as a path. When it changes, the field resets to it.
   *
   * A path typed against the old drive is not merely unhelpful after a switch,
   * it is wrong in a way that looks right: `/Volumes/NATO/TBD` left sitting in
   * the field while `tuf8tb` is on screen reads as "this is where you are" and
   * is not. Resetting to the new root replaces a stale answer with a true one,
   * and leaves the field somewhere useful to type onward from.
   */
  scanRoot?: string | null;
}

export function ScanBar({ onScan, busy = false, scanRoot = null }: ScanBarProps) {
  const [value, setValue] = useState("");
  const [options, setOptions] = useState<readonly string[]>([]);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [loading, setLoading] = useState(false);

  const inputRef = useRef<HTMLInputElement>(null);

  /*
   * Follow the drive.
   *
   * Keyed on `scanRoot` alone, so this fires when the drive on screen changes
   * and not on every keystroke — the user's typing is never overwritten while
   * they are in the middle of it.
   */
  useEffect(() => {
    setValue(scanRoot ?? "");
    setOpen(false);
    setActive(-1);
  }, [scanRoot]);

  const listId = useId();

  // The completion the user has actually singled out, as opposed to the first
  // one in the list. Enter on nothing highlighted submits what was typed —
  // silently substituting the top suggestion is how people scan the wrong
  // directory.
  const highlighted = active >= 0 && active < options.length ? options[active] : null;

  useEffect(() => {
    const trimmed = value.trim();
    if (trimmed.length === 0) {
      setOptions([]);
      setLoading(false);
      return undefined;
    }

    // Guards the response, not the request: `read_dir` on a slow external
    // volume can return after a later, narrower query has already resolved,
    // and the stale answer would overwrite the fresh one.
    let current = true;
    setLoading(true);
    const timer = window.setTimeout(() => {
      completePath(trimmed)
        .then((found) => {
          if (!current) return;
          setOptions(found);
          // Reset rather than preserve: index 3 of the old list is a different
          // directory in the new one, and keeping it moves the highlight to
          // something the user never looked at.
          setActive(-1);
        })
        .catch(() => {
          if (current) setOptions([]);
        })
        .finally(() => {
          if (current) setLoading(false);
        });
    }, DEBOUNCE_MS);

    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [value]);

  const submit = useCallback(
    (root: string) => {
      const trimmed = root.trim();
      if (trimmed.length === 0 || busy) return;
      setOpen(false);
      setActive(-1);
      onScan(trimmed);
    },
    [busy, onScan],
  );

  /** Takes a suggestion into the field without scanning, so the user can keep typing. */
  const accept = useCallback((path: string) => {
    setValue(path);
    setActive(-1);
    setOpen(true);
    inputRef.current?.focus();
  }, []);

  const onKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLInputElement>) => {
      if (event.key === "Escape") {
        setOpen(false);
        setActive(-1);
        return;
      }
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        if (options.length === 0) return;
        event.preventDefault();
        setOpen(true);
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActive((index) => {
          const next = index + step;
          if (next < 0) return options.length - 1;
          if (next >= options.length) return 0;
          return next;
        });
        return;
      }
      // Tab completes without leaving the field — the shell behaviour anyone
      // typing a path already has in their fingers.
      if (event.key === "Tab" && highlighted !== null) {
        event.preventDefault();
        accept(highlighted);
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        if (highlighted !== null) accept(highlighted);
        else submit(value);
      }
    },
    [accept, highlighted, options.length, submit, value],
  );

  return (
    <div className="relative flex items-center gap-1.5">
      <div className="relative flex items-center">
        <FolderSearch
          aria-hidden
          className="pointer-events-none absolute left-2 size-3.5 text-muted-foreground"
        />
        <input
          ref={inputRef}
          value={value}
          onChange={(event) => {
            setValue(event.currentTarget.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          // A click inside the list would otherwise blur the field and unmount
          // the option before its own click landed.
          onBlur={() => window.setTimeout(() => setOpen(false), 120)}
          onKeyDown={onKeyDown}
          placeholder="Scan a folder…"
          aria-label="Folder to scan"
          role="combobox"
          aria-expanded={open && options.length > 0}
          aria-controls={listId}
          aria-autocomplete="list"
          aria-activedescendant={highlighted === null ? undefined : `${listId}-${active}`}
          spellCheck={false}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          className={cn(
            "h-7 w-64 rounded-md border border-input bg-transparent pl-7 pr-7",
            "font-mono text-xs outline-none",
            "placeholder:font-sans placeholder:text-muted-foreground",
            "focus-visible:ring-2 focus-visible:ring-ring",
          )}
        />
        {loading ? (
          <Loader aria-hidden className="pointer-events-none absolute right-2 size-3 animate-spin text-muted-foreground" />
        ) : (
          value.length > 0 && (
            <button
              type="button"
              // Clears without scanning and without closing the bar, so the
              // fastest way back to "somewhere else entirely" is one click
              // rather than a select-all and a delete.
              onClick={() => {
                setValue("");
                setOptions([]);
                setActive(-1);
                setOpen(false);
                inputRef.current?.focus();
              }}
              title="Clear"
              className="absolute right-1.5 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <X aria-hidden className="size-3" />
              <span className="sr-only">Clear the path</span>
            </button>
          )
        )}
      </div>

      <Button size="sm" onClick={() => submit(value)} disabled={busy || value.trim().length === 0}>
        Scan
      </Button>

      {open && options.length > 0 && (
        <ul
          id={listId}
          role="listbox"
          aria-label="Matching folders"
          className={cn(
            "absolute left-0 top-full z-50 mt-1 max-h-72 w-[28rem] overflow-y-auto",
            "rounded-md border border-border bg-popover p-1 shadow-lg",
          )}
        >
          {options.map((path, index) => (
            <li key={path}>
              <button
                type="button"
                id={`${listId}-${index}`}
                role="option"
                aria-selected={index === active}
                // pointerdown, not click: the input's blur would close this
                // list first and the click would land on nothing.
                onPointerDown={(event) => {
                  event.preventDefault();
                  accept(path);
                }}
                onMouseEnter={() => setActive(index)}
                className={cn(
                  "block w-full truncate rounded px-2 py-1 text-left font-mono text-xs",
                  index === active ? "bg-accent text-accent-foreground" : "text-muted-foreground",
                )}
              >
                {path}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
