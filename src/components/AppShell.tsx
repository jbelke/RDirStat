/**
 * Application composition.
 *
 * This is the only component that knows about all the others, and the only one
 * that owns cross-cutting lifecycle:
 *
 * - **Generation change is the app's one real invalidation event.** When
 *   `scan_status` reports a new `TreeGeneration`, every cached page and every
 *   selected NodeId from the previous generation becomes meaningless — the
 *   arena they indexed is gone. `dropStaleGenerations` evicts the Query cache
 *   and `syncGeneration` drops selection, focus, and the navigation stack. Both
 *   have to happen together, which is why neither lives in a component that
 *   could unmount.
 * - **Completion is a state transition, never an inference.** The strip
 *   animates from the 10 Hz event, but "the scan is done" comes from
 *   `scan_status`, exactly as the contract requires.
 *
 * The left rail lists the routes docs/05 specifies. The four that still need a
 * completed catalog scan are visibly present and disabled with the reason,
 * rather than hidden: a missing feature the user can see and understand beats a
 * navigation model that changes shape.
 *
 * **Sizes is no longer one of them.** docs/05 grouped it with the catalog
 * reports, but a size histogram is a single `O(subtree)` pass over an arena that
 * is already in memory — it needs no Parquet partition, so it runs on the live
 * tree and the route is enabled as soon as a scan exists.
 */

import { useQueryClient } from "@tanstack/react-query";
import type { SortingState } from "@tanstack/react-table";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { DetailsPanel } from "@/components/DetailsPanel";
import { RelocateDialog } from "@/components/RelocateDialog";
import { CategoryLegend } from "@/components/canvas/CategoryLegend";
import {
  HierarchyCanvas,
  type ContextActionRequest,
  type NodeDescription,
  type SelectionChange,
} from "@/components/canvas";
import type { ColorBy } from "@/components/canvas/palette";
import { ScanAlerts } from "@/components/ScanAlerts";
import { SizeBands } from "@/components/SizeBands";
import { ScanProgressStrip, useScanProgress } from "@/components/ScanProgressStrip";
import { SegmentedControl } from "@/components/SegmentedControl";
import { Titlebar, type Crumb } from "@/components/Titlebar";
import { TreeTable } from "@/components/TreeTable";
import { VolumePicker } from "@/components/VolumePicker";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import { categoryOf } from "@/lib/categories";
import {
  nodeDetails,
  revealInFinder,
  scanCancel,
  scanStart,
  type LayoutKind,
  type RelocateReportView,
} from "@/lib/ipc";
import {
  dropStaleGenerations,
  queryKeys,
  useAncestors,
  useNodeDetails,
  useScanStatus,
  useSizeBands,
} from "@/lib/queries";
import { cn } from "@/lib/utils";
import { GENERATION_NONE, isRealNode } from "@/lib/wire";
import { useCurrentRoot, useSoleSelection, useUiStore, type Route } from "@/state/store";
import { CircleAlert, X } from "lucide-react";

const CATALOG_ROUTES: readonly { id: string; label: string }[] = [
  { id: "types", label: "Types" },
  { id: "ages", label: "Ages" },
  { id: "diff", label: "Diff" },
  { id: "dupes", label: "Dupes" },
];

const LAYOUT_OPTIONS = [
  { value: "treemap" as LayoutKind, label: "Treemap", title: "Squarified — biggest single thing" },
  { value: "icicle" as LayoutKind, label: "Icicle", title: "Depth on the y-axis — which deep path is heavy" },
  { value: "sunburst" as LayoutKind, label: "Sunburst", title: "Best overview, worst drill-down" },
];

const METRIC_OPTIONS = [
  { value: "logical" as const, label: "Logical", title: "File size as reported by the filesystem" },
  { value: "allocated" as const, label: "Allocated", title: "Blocks actually occupied (st_blocks × 512)" },
];

export function AppShell() {
  const client = useQueryClient();
  const status = useScanStatus();
  const progress = useScanProgress();

  const route = useUiStore((state) => state.route);
  const setRoute = useUiStore((state) => state.setRoute);
  const generation = useUiStore((state) => state.generation);
  const navStack = useUiStore((state) => state.navStack);
  const selection = useUiStore((state) => state.selection);
  const focused = useUiStore((state) => state.focused);
  const layoutKind = useUiStore((state) => state.layoutKind);
  const sizeMetric = useUiStore((state) => state.sizeMetric);
  const syncGeneration = useUiStore((state) => state.syncGeneration);
  const navigateTo = useUiStore((state) => state.navigateTo);
  const navigateUpTo = useUiStore((state) => state.navigateUpTo);
  const setNavPath = useUiStore((state) => state.setNavPath);
  const select = useUiStore((state) => state.select);
  const setHovered = useUiStore((state) => state.setHovered);
  const setLayoutKind = useUiStore((state) => state.setLayoutKind);
  const setSizeMetric = useUiStore((state) => state.setSizeMetric);
  const deletionArmed = useUiStore((state) => state.deletionArmed);
  const setDeletionArmed = useUiStore((state) => state.setDeletionArmed);

  const currentRoot = useCurrentRoot();
  const soleSelection = useSoleSelection();

  const [sorting, setSorting] = useState<SortingState>([{ id: "logical", desc: true }]);
  const [starting, setStarting] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  /** The root of the scan in flight, for the progress panel's denominator. */
  const [scanRoot, setScanRoot] = useState<string | null>(null);
  const [relocating, setRelocating] = useState<number | null>(null);
  // Family first: a whole-volume scan puts far more than a dozen categories on
  // screen at once, and past that the per-category hues stop being tellable
  // apart at tile size. Drilling in is when switching to `category` pays.
  const [colorBy, setColorBy] = useState<ColorBy>("family");

  const liveGeneration = status.data?.generation ?? GENERATION_NONE;
  const scanState = status.data?.state ?? "idle";
  const summary = status.data?.summary ?? null;
  const activeScan = status.data?.activeScan ?? null;

  // The one invalidation. Both halves, or neither.
  const lastGeneration = useRef(GENERATION_NONE);
  useEffect(() => {
    if (lastGeneration.current === liveGeneration) return;
    lastGeneration.current = liveGeneration;
    dropStaleGenerations(client, liveGeneration);
    syncGeneration(liveGeneration, status.data?.summary?.root ?? 0);
    // A new tree makes every previous complaint obsolete. Without this, a
    // correct refusal — "already scanning", say — stays on screen through the
    // successful scan that followed it, so the user reads a red banner
    // describing something that is no longer true and has no way to dismiss
    // it. An error message outliving the state it described is worse than no
    // message.
    setActionError(null);
  }, [liveGeneration, client, syncGeneration, status.data?.summary?.root]);

  // A cancel is only "done" when the supervisor says so.
  useEffect(() => {
    if (scanState !== "scanning" && scanState !== "cancelling") setCancelling(false);
  }, [scanState]);

  const handleScan = useCallback(
    async (root: string) => {
      setActionError(null);
      setStarting(true);
      // Remembered for the progress panel's coverage denominator. It cannot come
      // from `scan_status`: `summary` is null until a scan *completes*, so while
      // the scan that needs the root is running, the caller that started it is
      // the only thing that knows what it is.
      setScanRoot(root);
      try {
        await scanStart(root);
        await client.invalidateQueries({ queryKey: queryKeys.scanStatus() });
      } catch (cause) {
        setActionError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setStarting(false);
      }
    },
    [client],
  );

  const handleCancel = useCallback(async () => {
    if (activeScan === null) return;
    setCancelling(true);
    try {
      await scanCancel(activeScan);
      await client.invalidateQueries({ queryKey: queryKeys.scanStatus() });
    } catch (cause) {
      setCancelling(false);
      setActionError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [activeScan, client]);

  const handleReveal = useCallback(
    (node: number) => {
      void revealInFinder(generation, node).catch((cause: unknown) => {
        setActionError(cause instanceof Error ? cause.message : String(cause));
      });
    },
    [generation],
  );

  // Trash needs `trash_preview` -> confirmation UI -> `move_to_trash`. The
  // preview command exists in the contract; the confirmation sheet does not
  // exist in this build, and issuing `move_to_trash` without showing the user
  // what a generation-bound token covers would be worse than not offering it.
  //
  // The arming check is here as well as on every control that can reach it.
  // The controls disable themselves so the state is visible; this one is the
  // one that would still refuse if a control forgot to.
  const handleTrash = useCallback(
    (nodes: number[]) => {
      if (!deletionArmed) {
        setActionError(
          "Deletion is off. Switch it on in the details panel if you mean to move items to the Trash — it turns itself off again on the next scan.",
        );
        return;
      }
      setActionError(
        `Move to Trash is not wired up in this build (${nodes.length} item${nodes.length === 1 ? "" : "s"} selected). ` +
          "The confirmation sheet that binds a token to this generation has not been built, and issuing the action without it is not acceptable.",
      );
    },
    [deletionArmed],
  );

  // ---------------------------------------------------------------------
  // Canvas wiring.
  //
  // The canvas is the *other* view of the same selection the table shows, so
  // it is driven controlled: `selectedNodes` is the tree -> canvas half and
  // `onSelectionChange` is the canvas -> tree half. `source` is carried on the
  // change so an echo can be suppressed later if one ever appears; today the
  // store is the single owner, so setting it from either side converges.
  // ---------------------------------------------------------------------

  // A new array identity every render would re-run the canvas's selection
  // effect on every keystroke elsewhere in the shell.
  const selectedNodes = useMemo(() => [...selection], [selection]);

  const handleCanvasSelection = useCallback(
    (change: SelectionChange) => {
      if (change.primary === null) return;
      select(change.primary, change.additive ? "add" : "replace");
    },
    [select],
  );

  // The layout batch carries only ids and geometry — the pinned 7-column schema
  // has no name — so tooltips and the accessible list get their text from
  // `node_details`, through the same Query cache the table uses. A virtual
  // `<Files>` group is skipped: `node_details` answers `VirtualGroup` for one
  // by design, and a rejected promise here would surface as a tooltip error.
  const describeNode = useCallback(
    async (node: number): Promise<NodeDescription | undefined> => {
      if (generation === GENERATION_NONE || !isRealNode(node)) return undefined;
      const details = await client.fetchQuery({
        queryKey: queryKeys.details(generation, node),
        queryFn: () => nodeDetails(generation, node),
        staleTime: Number.POSITIVE_INFINITY,
      });
      return {
        path: details.path,
        name: details.name,
        logical: details.logical,
        allocated: details.allocated,
        categoryLabel: categoryOf(details.category).label,
      };
    },
    [client, generation],
  );

  const handleCanvasAction = useCallback(
    (request: ContextActionRequest) => {
      switch (request.action) {
        case "reveal":
          handleReveal(request.primary);
          break;
        case "trash":
          handleTrash([...request.nodes]);
          break;
        case "copy-path":
          void describeNode(request.primary)
            .then((description) => {
              if (description?.path === undefined) return;
              return navigator.clipboard.writeText(description.path);
            })
            .catch((cause: unknown) => {
              setActionError(cause instanceof Error ? cause.message : String(cause));
            });
          break;
        case "zoom":
          // The canvas already pushed its own stack; mirror it onto the store
          // so the breadcrumb and the table follow the canvas.
          navigateTo(request.primary);
          break;
      }
    },
    [describeNode, handleReveal, handleTrash, navigateTo],
  );

  const crumbs = useCrumbs(generation, currentRoot, summary?.rootPath ?? null);
  const rootDetails = useNodeDetails(generation, currentRoot);
  // Only while the route is showing: this is an O(subtree) walk on the backend.
  const sizeBands = useSizeBands(generation, currentRoot, route === "sizes");
  const relocatingDetails = useNodeDetails(generation, relocating);

  // A completed relocation makes the tree on screen wrong: the subtree that
  // moved is now a symlink and the sizes above it are stale. Rather than
  // silently showing numbers that no longer describe the disk, drop the caches
  // and say so — the honest fix is a re-scan, which the user starts.
  const handleRelocated = useCallback(
    (report: RelocateReportView) => {
      void client.invalidateQueries({ queryKey: queryKeys.scanStatus() });
      setActionError(
        `Moved to ${report.destination}. The sizes on screen still describe the volume as it was scanned — re-scan to see the space return.`,
      );
    },
    [client],
  );

  // A crumb click sets the navigation stack to the whole path down to that
  // crumb, not just the one node. Crumb 0 is the app name and is not
  // navigable, so the stack is `crumbs[1..=index]`.
  const handleCrumbNavigate = useCallback(
    (_node: number, index: number) => {
      const path = crumbs
        .slice(1, index + 1)
        .map((crumb) => crumb.node)
        .filter((node): node is number => node !== null);
      if (path.length > 0) setNavPath(path);
    },
    [crumbs, setNavPath],
  );

  // ⌘↑ is the Finder shortcut for "enclosing folder", so it is the one a macOS
  // user will already try.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.metaKey || event.key !== "ArrowUp" || navStack.length < 2) return;
      event.preventDefault();
      navigateUpTo(navStack.length - 2);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navStack.length, navigateUpTo]);

  return (
    <div className="flex h-full flex-col">
      <Titlebar crumbs={crumbs} onNavigate={handleCrumbNavigate}>
        {generation !== GENERATION_NONE && (
          <Button variant="ghost" size="sm" onClick={() => setRoute("volumes")}>
            Scan…
          </Button>
        )}
      </Titlebar>

      <div className="flex min-h-0 flex-1">
        <nav aria-label="Views" className="flex w-32 shrink-0 flex-col gap-0.5 border-r border-border/60 p-2">
          <RailButton id="volumes" label="Volumes" route={route} onSelect={setRoute} />
          <RailButton
            id="tree"
            label="Tree"
            route={route}
            onSelect={setRoute}
            disabled={generation === GENERATION_NONE}
          />
          <RailButton
            id="sizes"
            label="Sizes"
            route={route}
            onSelect={setRoute}
            disabled={generation === GENERATION_NONE}
          />
          {CATALOG_ROUTES.map((entry) => (
            <button
              key={entry.id}
              type="button"
              disabled
              title="Requires a completed catalog scan. The catalog (DuckDB/Parquet) is not part of this build."
              className="rounded px-2 py-1 text-left text-sm text-muted-foreground/40"
            >
              {entry.label}
            </button>
          ))}
        </nav>

        <main className="flex min-h-0 min-w-0 flex-1 flex-col">
          {actionError !== null && (
            <Alert variant="destructive" className="m-3 pr-10">
              <CircleAlert aria-hidden />
              <AlertTitle>Action failed</AlertTitle>
              <AlertDescription>{actionError}</AlertDescription>
              {/* Dismissible on purpose. It also clears itself on the next
                * generation, but a banner the user cannot get rid of is its
                * own defect — especially for a message that has already been
                * read and acted on. */}
              <button
                type="button"
                onClick={() => setActionError(null)}
                title="Dismiss"
                className="absolute right-2 top-2 rounded p-1 opacity-70 hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <X aria-hidden className="size-4" />
                <span className="sr-only">Dismiss this message</span>
              </button>
            </Alert>
          )}

          {route === "volumes" && <VolumePicker onScan={(root) => void handleScan(root)} busy={starting} />}

          {route === "sizes" && currentRoot !== null && (
            <SizeBands
              rows={sizeBands.data}
              isLoading={sizeBands.isLoading}
              error={sizeBands.error}
              subtreeAllocated={rootDetails.data?.subtree?.allocated ?? null}
              className="min-h-0 flex-1"
            />
          )}

          {route === "tree" && currentRoot !== null && (
            <div className="flex min-h-0 flex-1 flex-col">
              <div className="flex items-center gap-3 border-b border-border/60 px-3 py-2">
                <SegmentedControl
                  label="Hierarchy layout"
                  options={LAYOUT_OPTIONS}
                  value={layoutKind}
                  onChange={setLayoutKind}
                />
                <SegmentedControl
                  label="Size metric"
                  options={METRIC_OPTIONS}
                  value={sizeMetric}
                  onChange={setSizeMetric}
                />
                <span className="ml-auto text-xs text-muted-foreground">
                  {rootDetails.data !== undefined && (
                    <>
                      {formatSI(
                        sizeMetric === "logical"
                          ? rootDetails.data.logical
                          : rootDetails.data.allocated,
                      )}{" "}
                      in this subtree
                    </>
                  )}
                </span>
              </div>

              {/* The canvas owns its own `layout` fetch, decode, hit-test and
                * paint. `showToggle={false}` because the toolbar above already
                * owns `layoutKind`; two controls for one piece of state is how
                * they drift apart. */}
              <HierarchyCanvas
                className="h-72 shrink-0 border-b border-border/60"
                generation={generation}
                root={currentRoot}
                kind={layoutKind}
                showToggle={false}
                selectedNodes={selectedNodes}
                onSelectionChange={handleCanvasSelection}
                onNavigate={navigateTo}
                onContextAction={handleCanvasAction}
                trashEnabled={deletionArmed}
                describeNode={describeNode}
                formatBytes={formatSI}
                colorBy={colorBy}
              />

              {/* The key to the colours. Without it the encoding is decoration:
                * you can see two tiles differ but not what the difference
                * means, which throws away the entire point of classifying by
                * content type. `present={null}` lists the whole taxonomy;
                * once the canvas reports which categories it actually drew,
                * pass that set instead so the legend shrinks on drill-down. */}
              <CategoryLegend colorBy={colorBy} present={null} onColorByChange={setColorBy} />

              <ScanAlerts summary={summary} className="p-3" />

              <TreeTable
                generation={generation}
                root={currentRoot}
                rootLogical={rootDetails.data?.logical ?? summary?.totals.logical ?? 0}
                rootAllocated={rootDetails.data?.allocated ?? summary?.totals.allocated ?? 0}
                sizeMetric={sizeMetric}
                sorting={sorting}
                onSortingChange={setSorting}
                selection={selection}
                focused={focused}
                onSelect={select}
                onDrillDown={navigateTo}
                onHover={setHovered}
                onReveal={handleReveal}
                onTrash={(node) => handleTrash([node])}
                trashEnabled={deletionArmed}
              />
            </div>
          )}
        </main>

        <DetailsPanel
          generation={generation}
          node={soleSelection}
          selectionCount={selection.size}
          onReveal={handleReveal}
          onTrash={handleTrash}
          onRelocate={setRelocating}
          onTrashDropped={handleTrash}
          deletionArmed={deletionArmed}
          onArmDeletion={setDeletionArmed}
        />
      </div>

      <RelocateDialog
        generation={generation}
        node={relocating}
        sourcePath={relocatingDetails.data?.path ?? null}
        scanRootPath={summary?.rootPath ?? null}
        deletionArmed={deletionArmed}
        onClose={() => setRelocating(null)}
        onRelocated={handleRelocated}
      />

      <ScanProgressStrip
        state={scanState}
        progress={progress}
        scanRoot={scanRoot}
        cancelling={cancelling}
        onCancel={() => void handleCancel()}
      />
    </div>
  );
}

function RailButton({
  id,
  label,
  route,
  onSelect,
  disabled = false,
}: {
  id: Route;
  label: string;
  route: Route;
  onSelect: (route: Route) => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      aria-current={route === id ? "page" : undefined}
      onClick={() => onSelect(id)}
      className={cn(
        "rounded px-2 py-1 text-left text-sm transition-colors",
        route === id ? "bg-accent font-medium text-accent-foreground" : "text-muted-foreground",
        !disabled && "hover:bg-accent/60 hover:text-foreground",
        disabled && "text-muted-foreground/40",
      )}
    >
      {label}
    </button>
  );
}

/**
 * Breadcrumb labels.
 *
 * Each ancestor's name comes from the same `node_details` cache entry the
 * details panel uses, so navigating back up a path the user already visited
 * costs nothing. The first crumb is the app name and is not navigable; the
 * second is the scan root, labelled with its path rather than its basename so
 * `/Volumes/tuf8tb` does not display as `tuf8tb › tuf8tb`.
 */
function useCrumbs(generation: number, current: number | null, rootPath: string | null): Crumb[] {
  // The `ancestors` command answers this in O(depth) with no `stat`, from the
  // frozen tree, so it is both cheaper and more correct than the alternative.
  const chain = useAncestors(generation, current);

  const crumbs: Crumb[] = [{ node: null, label: "RDirStat" }];
  if (chain.data === undefined) {
    // Before the chain arrives, show the root rather than nothing: the strip
    // must not change height or jump on every navigation.
    if (current !== null && rootPath !== null) crumbs.push({ node: null, label: rootPath });
    return crumbs;
  }
  for (const ancestor of chain.data) {
    crumbs.push({ node: ancestor.node, label: ancestor.name });
  }
  return crumbs;
}
