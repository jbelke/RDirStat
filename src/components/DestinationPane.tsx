/**
 * The right half of Split View: where things are going.
 *
 * **A destination is a path, not a scan.** `relocate_plan` takes a directory
 * string and checks that directory's own properties; it never asks how large
 * the subtree under it is. So this pane browses the live filesystem rather than
 * a scanned tree, which is what lets it point at a disk that has never been
 * scanned — including one on the other side of the machine — without spending
 * minutes measuring a disk nobody asked about.
 *
 * That is also why it is not a second copy of the tree table. The left pane
 * answers "what is big"; this one answers "where should it go", and those need
 * different things on screen: sizes and categories on one side, folder names
 * and a path you can type on the other.
 *
 * **It proposes; it does not act.** Choosing a destination here and pressing
 * the button opens the existing move dialog with this path filled in. The plan
 * → token → confirm sequence in that dialog is the whole safety story for a
 * destructive operation — three separate data-loss defects were found and fixed
 * inside it — and a second path into the same operation that skipped those
 * steps would be a second place for those rules to drift out of agreement.
 */

import { ArrowUp, CornerDownRight, FolderOpen, Loader, TriangleAlert } from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import { PathField } from "@/components/PathField";
import { Button } from "@/components/ui/button";
import { browseDirectories, type BrowseListingView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

export interface DestinationPaneProps {
  /** The directory currently being pointed at. */
  path: string;
  onPathChange: (path: string) => void;
  /** How many nodes are selected in the source pane. */
  selectionCount: number;
  /** Opens the move dialog with this destination pre-filled. */
  onMoveHere: (destination: string) => void;
  className?: string;
}

export function DestinationPane({
  path,
  onPathChange,
  selectionCount,
  onMoveHere,
  className,
}: DestinationPaneProps) {
  const [listing, setListing] = useState<BrowseListingView | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (path.trim().length === 0) {
      setListing(null);
      return undefined;
    }
    // Same stale-response guard as the scan bar: browsing an external volume
    // can outlive a later, faster request, and the old answer would overwrite
    // the directory the user has already moved on to.
    let current = true;
    setLoading(true);
    browseDirectories(path)
      .then((next) => {
        if (current) setListing(next);
      })
      .catch(() => {
        if (current) setListing(null);
      })
      .finally(() => {
        if (current) setLoading(false);
      });
    return () => {
      current = false;
    };
  }, [path]);

  const descend = useCallback((child: string) => onPathChange(child), [onPathChange]);

  const canMove = selectionCount > 0 && listing !== null && listing.unreadable === null;

  return (
    <section
      aria-label="Destination"
      className={cn("flex min-h-0 min-w-0 flex-col gap-2 border-l border-border/60 p-3", className)}
    >
      <header className="flex shrink-0 items-center gap-2">
        <CornerDownRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />
        <h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Destination
        </h2>
        {loading && <Loader aria-hidden className="size-3 animate-spin text-muted-foreground" />}
      </header>

      <PathField
        label="Folder"
        layout="stacked"
        value={path}
        onChange={onPathChange}
        placeholder="/Volumes/…"
      />

      <div className="flex shrink-0 items-center gap-2">
        <Button
          size="sm"
          variant="ghost"
          disabled={listing?.parent === null || listing?.parent === undefined}
          onClick={() => listing?.parent !== null && listing?.parent !== undefined && onPathChange(listing.parent)}
          title="Up one level"
        >
          <ArrowUp aria-hidden />
          Up
        </Button>
        <span className="min-w-0 flex-1 truncate text-[10px] text-muted-foreground" title={listing?.path}>
          {listing?.path ?? ""}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto rounded-md border border-border/60">
        {/* "Could not read" and "nothing in here" are different answers and are
          * drawn differently on purpose. Rendering a permission failure as an
          * empty folder invites someone to pick it as a destination and only
          * find out at the plan step. */}
        {listing?.unreadable !== null && listing?.unreadable !== undefined ? (
          <p className="flex items-start gap-2 p-3 text-xs text-pressure-warn">
            <TriangleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
            <span>This folder could not be read. {listing.unreadable}</span>
          </p>
        ) : listing === null ? (
          <p className="p-3 text-xs text-muted-foreground">Type or choose a folder to move into.</p>
        ) : listing.directories.length === 0 ? (
          <p className="p-3 text-xs text-muted-foreground">
            No sub-folders here. This folder can still be the destination.
          </p>
        ) : (
          <ul className="p-1">
            {listing.directories.map((entry) => (
              <li key={entry.path}>
                <button
                  type="button"
                  onClick={() => descend(entry.path)}
                  className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  <FolderOpen aria-hidden className="size-3.5 shrink-0" />
                  <span className="truncate">{entry.name}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
        {listing?.truncated === true && (
          <p className="border-t border-border/60 px-3 py-1.5 text-[10px] text-muted-foreground">
            Showing the first 500 folders. Type a path to reach the rest.
          </p>
        )}
      </div>

      <Button
        size="sm"
        className="shrink-0"
        disabled={!canMove}
        onClick={() => listing !== null && onMoveHere(listing.path)}
      >
        {selectionCount === 0
          ? "Select something on the left"
          : `Move ${selectionCount} selected here…`}
      </Button>
      {/* The ellipsis is load-bearing: this opens the move dialog, it does not
        * move anything. */}
    </section>
  );
}
