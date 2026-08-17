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

import { ChevronRight, Search, Settings } from "lucide-react";
import { Fragment } from "react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface Crumb {
  /** `NodeId`, or `null` for a non-navigable label such as the app name. */
  readonly node: number | null;
  readonly label: string;
}

export interface TitlebarProps {
  crumbs: readonly Crumb[];
  /** Index into `crumbs`; the shell pops the navigation stack to that depth. */
  onNavigate?: (index: number) => void;
  onOpenCommandPalette?: () => void;
  onOpenSettings?: () => void;
  /** Rendered between the breadcrumb and the trailing actions (e.g. a Scan button). */
  children?: React.ReactNode;
}

export function Titlebar({
  crumbs,
  onNavigate,
  onOpenCommandPalette,
  onOpenSettings,
  children,
}: TitlebarProps) {
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
      <nav
        aria-label="Breadcrumb"
        // The nav itself stays draggable so the empty space beside a short
        // breadcrumb still moves the window; only the crumb buttons opt out.
        data-tauri-drag-region
        className="flex min-w-0 flex-1 items-center gap-0.5 overflow-hidden text-sm"
      >
        {crumbs.map((crumb, index) => {
          const isLast = index === crumbs.length - 1;
          return (
            <Fragment key={`${crumb.node ?? "root"}-${index}`}>
              {index > 0 && (
                <ChevronRight
                  aria-hidden
                  className="size-3.5 shrink-0 text-muted-foreground/60"
                />
              )}
              {crumb.node === null || isLast || onNavigate === undefined ? (
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
                  onClick={() => onNavigate(index)}
                  className="truncate rounded px-1.5 py-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  {crumb.label}
                </button>
              )}
            </Fragment>
          );
        })}
      </nav>

      <div className="flex shrink-0 items-center gap-1">
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
