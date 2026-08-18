/**
 * The macOS titlebar overlay.
 *
 * `tauri.conf.json` sets `"titleBarStyle": "Overlay"` with `"hiddenTitle": true`.
 * That is a **public** NSWindow style — no `macos-private-api` — so the native
 * traffic lights, window dragging, double-click-to-zoom, full-screen, and the
 * accessibility tree all keep working, and only the title *text* is hidden.
 * Web content then extends under the strip.
 *
 * Three consequences the markup below has to honour, all of which are easy to
 * get wrong and produce a window that cannot be moved:
 *
 * 1. The strip must carry `data-tauri-drag-region`, or the top of the window
 *    stops being draggable — the native title area is gone.
 * 2. Anything interactive inside it must opt **out** of the drag region, or it
 *    swallows its own clicks. `src/index.css` does that automatically for
 *    `button`/`a`/`input`/`[role=button]` descendants; anything else needs
 *    `data-tauri-drag-region="false"` explicitly.
 * 3. Content must start to the right of the traffic lights. The inset is a
 *    token (`--traffic-light-inset`) rather than a magic number because the
 *    lights move with the system's window-control size.
 *
 * The breadcrumb lives here rather than in a second toolbar row, per
 * docs/05-UI.md: "The breadcrumb is navigation, so there is no second row of
 * unlabeled toolbar icons."
 */

import { ChevronRight, ChevronUp, ChevronsUp, Search, Settings } from "lucide-react";
import { Fragment, useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface Crumb {
  /** `NodeId`, or `null` for a non-navigable label such as the app name. */
  readonly node: number | null;
  readonly label: string;
}

export interface TitlebarProps {
  crumbs: readonly Crumb[];
  /**
   * Called with the crumb's `NodeId` and its index. Never called for a crumb
   * whose `node` is `null`, nor for the last one.
   *
   * The index is passed alongside the node because the shell rebuilds the
   * navigation stack from the crumb *path* — everything up to and including
   * this crumb — rather than pushing a single node. A breadcrumb built from
   * the tree can offer an ancestor the user has never visited, and pushing one
   * of those onto a click-history stack leaves it holding something that is
   * not a path.
   */
  onNavigate?: (node: number, index: number) => void;
  onOpenCommandPalette?: () => void;
  onOpenSettings?: () => void;
  /**
   * Rendered **inside the breadcrumb**, between the app name and the scan
   * root: `RDirStat › [Archive ⌄] › /Volumes/Archive`.
   *
   * That position is the point of it. Which drive you are looking at is the
   * outermost fact about the tree on screen — one level above the scan root —
   * so the control that changes it belongs where the eye already goes to read
   * "where am I", not in the corner beside the window buttons. Kept as a slot
   * rather than a prop bundle so the titlebar does not have to know what a
   * volume is.
   */
  driveSelector?: React.ReactNode;
  /** Rendered between the breadcrumb and the trailing actions (e.g. a Scan button). */
  children?: React.ReactNode;
}

/**
 * Crumbs shown before the trail collapses.
 *
 * A real drill-down goes deep fast — `/Users/josh/Library/Containers/
 * com.docker.docker/Data/vms/0/data/Docker.raw` is nine crumbs — and nine
 * `truncate`d labels in a fixed-width strip degrades into nine unreadable
 * two-character stubs. Collapsing the middle keeps the two ends, which are the
 * two the user actually navigates to.
 */
const COLLAPSE_THRESHOLD = 6;
/** Kept at the end of a collapsed trail, nearest the current node. */
const TAIL_CRUMBS = 3;

export function Titlebar({
  crumbs,
  onNavigate,
  onOpenCommandPalette,
  onOpenSettings,
  driveSelector,
  children,
}: TitlebarProps) {
  const [expanded, setExpanded] = useState(false);

  // Navigating elsewhere re-collapses: the expansion is about reading *this*
  // trail, and leaving it open lets the strip stay overfull indefinitely.
  const trail = crumbs.map((crumb) => crumb.label).join("/");
  useEffect(() => setExpanded(false), [trail]);

  const collapsible = crumbs.length > COLLAPSE_THRESHOLD && !expanded;
  // Always keep crumb 0 (the app name) and crumb 1 (the scan root); the hidden
  // run is everything between that and the tail.
  const hiddenFrom = 2;
  const hiddenTo = crumbs.length - TAIL_CRUMBS;
  const hidden = collapsible ? crumbs.slice(hiddenFrom, hiddenTo) : [];

  // The last crumb is the current node and is never a link, so an "up" control
  // targets the one before it.
  const parent = crumbs.length >= 2 ? crumbs[crumbs.length - 2] : undefined;
  const upTarget = parent?.node ?? null;

  /*
   * Crumb 0 is the app name; crumb 1 is the scan root. So "go to the root of
   * the drive" is a jump to index 1, and it is only offered when we are not
   * already there.
   *
   * This exists because the trail was the *only* way back. That is fine at
   * depth — the root crumb is right there — but the moment a user drills in via
   * the canvas or a double-click, the way back is a small text link they have
   * to notice, and there is no fixed affordance that is always in the same
   * place. Drilling in is one gesture; getting out should not require reading.
   */
  const rootCrumb = crumbs.length >= 2 ? crumbs[1] : undefined;
  const atRoot = crumbs.length <= 2;
  const rootTarget = atRoot ? null : (rootCrumb?.node ?? null);

  return (
    <header
      data-tauri-drag-region
      className={cn(
        "flex h-[var(--titlebar-height)] shrink-0 items-center gap-2",
        "border-b border-border/60 bg-background/80 backdrop-blur-xl",
        "pr-3",
      )}
      style={{ paddingLeft: "var(--traffic-light-inset)" }}
    >
      {rootTarget !== null && onNavigate !== undefined && (
        <button
          type="button"
          onClick={() => onNavigate(rootTarget, 1)}
          aria-keyshortcuts="Meta+Shift+ArrowUp"
          title={`Back to ${rootCrumb?.label ?? "the scan root"} (⇧⌘↑)`}
          className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <ChevronsUp aria-hidden className="size-4" />
          <span className="sr-only">Back to the top of this drive</span>
        </button>
      )}

      {upTarget !== null && onNavigate !== undefined && (
        <button
          type="button"
          onClick={() => onNavigate(upTarget, crumbs.length - 2)}
          aria-keyshortcuts="Meta+ArrowUp"
          title={`Up to ${parent?.label ?? "the parent"} (⌘↑)`}
          className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <ChevronUp aria-hidden className="size-4" />
          <span className="sr-only">Up one level</span>
        </button>
      )}

      <nav
        aria-label="Breadcrumb"
        // The nav itself stays draggable so the empty space beside a short
        // breadcrumb still moves the window; only the crumb buttons opt out.
        data-tauri-drag-region
        className="flex min-w-0 flex-1 items-center gap-0.5 overflow-hidden text-sm"
      >
        {crumbs.map((crumb, index) => {
          const isLast = index === crumbs.length - 1;
          if (collapsible && index >= hiddenFrom && index < hiddenTo) {
            // Render the ellipsis once, in place of the first hidden crumb.
            if (index !== hiddenFrom) return null;
            return (
              <Fragment key="ellipsis">
                <ChevronRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground/60" />
                <button
                  type="button"
                  onClick={() => setExpanded(true)}
                  title={`Show ${hidden.length} hidden: ${hidden.map((entry) => entry.label).join(" / ")}`}
                  className="shrink-0 rounded px-1.5 py-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  …
                  <span className="sr-only">
                    Show {hidden.length} hidden breadcrumb levels
                  </span>
                </button>
              </Fragment>
            );
          }
          // Bound once so the button branch has a `number`, not `number | null`.
          const target = crumb.node;
          return (
            <Fragment key={`${crumb.node ?? "root"}-${index}`}>
              {index > 0 && (
                <ChevronRight
                  aria-hidden
                  className="size-3.5 shrink-0 text-muted-foreground/60"
                />
              )}
              {/* The drive selector sits between the app name and the scan
                * root, as its own crumb-shaped control.
                *
                * It is wrapped in an explicit `data-tauri-drag-region="false"`
                * even though the surrounding nav is inside the strip: the
                * per-element opt-out in index.css covers `button`, and that is
                * enough for a click handler that fires on mouseup, but a Radix
                * menu trigger opens on POINTERDOWN and the drag region eats
                * that before the trigger sees it. The menu then never opens
                * while hover and tooltips keep working perfectly — which reads
                * exactly like a dead handler. */}
              {index === 1 && driveSelector !== undefined && (
                <Fragment>
                  <span data-tauri-drag-region="false" className="flex shrink-0 items-center">
                    {driveSelector}
                  </span>
                  <ChevronRight
                    aria-hidden
                    className="size-3.5 shrink-0 text-muted-foreground/60"
                  />
                </Fragment>
              )}
              {target === null || isLast || onNavigate === undefined ? (
                <span
                  aria-current={isLast ? "page" : undefined}
                  className={cn(
                    "truncate rounded px-1.5 py-0.5",
                    isLast ? "font-medium text-foreground" : "text-muted-foreground",
                  )}
                >
                  {crumb.label}
                </span>
              ) : (
                <button
                  type="button"
                  onClick={() => onNavigate(target, index)}
                  title={`Go to ${crumb.label}`}
                  className="truncate rounded px-1.5 py-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  {crumb.label}
                </button>
              )}
            </Fragment>
          );
        })}
      </nav>

      {/* Explicitly OUT of the drag region, as a container rather than relying
        * on the per-element `button` opt-out in index.css.
        *
        * That opt-out is enough for a plain `onClick` button, which fires on
        * mouseup — the root and up-one-level buttons above work fine. It is
        * NOT enough for anything that opens on `pointerdown`: Radix's menu
        * triggers do, and the drag region consumes pointerdown before the
        * trigger sees it, so the menu silently never opens while `:hover` and
        * the tooltip still work perfectly. That combination reads exactly like
        * a dead handler and is why this cost an hour to find.
        *
        * Marking the whole actions container means the next control dropped in
        * here inherits the fix instead of rediscovering the bug. */}
      <div data-tauri-drag-region="false" className="flex shrink-0 items-center gap-1">
        {children}
        {onOpenCommandPalette !== undefined && (
          <Button
            variant="ghost"
            size="sm"
            onClick={onOpenCommandPalette}
            aria-keyshortcuts="Meta+K"
            title="Command palette (⌘K)"
          >
            <Search aria-hidden />
            <span className="text-xs text-muted-foreground">⌘K</span>
          </Button>
        )}
        {onOpenSettings !== undefined && (
          <Button variant="ghost" size="icon" onClick={onOpenSettings} title="Settings">
            <Settings aria-hidden />
            <span className="sr-only">Settings</span>
          </Button>
        )}
      </div>
    </header>
  );
}
