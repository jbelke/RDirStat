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

import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import { useQueryClient } from "@tanstack/react-query";
import type { SortingState } from "@tanstack/react-table";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { DestinationPane } from "@/components/DestinationPane";
import { DetailsPanel } from "@/components/DetailsPanel";
import { DriveSwitcher } from "@/components/DriveSwitcher";
import { RelocateDialog } from "@/components/RelocateDialog";
import { SelectionActions } from "@/components/SelectionActions";
import { AUTHOR_URL, StoragePanel } from "@/components/StoragePanel";
import { SyncRoute } from "@/components/SyncRoute";
import { TransfersRoute } from "@/components/TransfersRoute";
import { CategoryLegend } from "@/components/canvas/CategoryLegend";
import {
  HierarchyCanvas,
  type ContextActionRequest,
  type NodeDescription,
  type SelectionChange,
} from "@/components/canvas";
import type { ColorBy } from "@/components/canvas/palette";
import { ScanAlerts } from "@/components/ScanAlerts";
import { AgesRoute, pinnedNowUnixSeconds } from "@/components/AgesRoute";
import { DiffRoute } from "@/components/DiffRoute";
import { DupesRoute } from "@/components/DupesRoute";
import { SizeBands } from "@/components/SizeBands";
import { TypesRoute } from "@/components/TypesRoute";
import { ScanBar } from "@/components/ScanBar";
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
  exportSnapshot,
  revealInFinder,
  scanCancel,
  restoreSnapshot,
  scanStart,
  scanStatus,
  OPEN_SETTINGS_EVENT,
  type DiffMetricKind,
  type LayoutKind,
  type RelocateReportView,
} from "@/lib/ipc";
import {
  dropStaleGenerations,
  MAX_REPORT_ENTRIES,
  queryKeys,
  useAgeBucketEntries,
  useAgeBuckets,
  useAncestors,
  useCategoryEntries,
  useCategoryTotals,
  useDuplicateCandidates,
  useNodeDetails,
  useScanDiff,
  useScanStatus,
  useSnapshotOffers,
  useSetSnapshotDir,
  useStorageReport,
  useVolumes,
  useSizeBands,
  useTransferProgress,
} from "@/lib/queries";
import { cn } from "@/lib/utils";
import { GENERATION_NONE, isRealNode } from "@/lib/wire";
import { useCurrentRoot, useSoleSelection, useUiStore, type Route } from "@/state/store";
import { CircleAlert, Columns2, PanelRightOpen, X } from "lucide-react";

/**
 * The report routes, all of them live.
 *
 * These were disabled buttons captioned "requires a completed catalog scan".
 * That turned out to be an assumption rather than a constraint: every one of
 * them is a single `O(subtree)` pass over an arena that is already in memory.
 * Types and Ages count files, Dupes groups them by size, and Diff compares the
 * published tree against the previous *.rdstat snapshot — no Parquet partition
 * is involved in any of it.
 *
 * The catalog still has a job (many scans, cross-scan queries, retention), but
 * it was never what these five needed.
 */
/**
 * The left rail, grouped by what kind of tool each route is.
 *
 * This app started as a disk *analyser* and is becoming a general disk
 * utility, so the rail is organised by the job rather than as one flat list.
 * The grouping is data, not markup: adding a tool — cloning, verifying,
 * archiving — is a row in this table and nothing else, which is the point when
 * the set is expected to grow.
 *
 * `needsScan` is the load-bearing field. Everything under Analysis reads the
 * tree in memory and is meaningless without one, so it is visibly disabled
 * with the reason rather than hidden — a missing feature you can see and
 * understand beats a navigation that changes shape underneath you. Tools that
 * operate on paths rather than on a scan are always available, which is why
 * Sync is not gated: syncing two folders has nothing to do with having scanned
 * a volume.
 */
interface RailItem {
  readonly id: Route;
  readonly label: string;
  /** Disabled until a tree is loaded. */
  readonly needsScan: boolean;
}

interface RailGroup {
  readonly id: string;
  readonly label: string;
  readonly items: readonly RailItem[];
}

const RAIL_GROUPS: readonly RailGroup[] = [
  {
    id: "analysis",
    label: "Analyse",
    items: [
      { id: "volumes", label: "Volumes", needsScan: false },
      { id: "tree", label: "Tree", needsScan: true },
      { id: "sizes", label: "Sizes", needsScan: true },
      { id: "types", label: "Types", needsScan: true },
      { id: "ages", label: "Ages", needsScan: true },
      { id: "diff", label: "Diff", needsScan: true },
      { id: "dupes", label: "Dupes", needsScan: true },
    ],
  },
  {
    id: "transfer",
    label: "Transfer",
    items: [
      { id: "sync", label: "Sync folders", needsScan: false },
      // Remote is a sibling of the local sync rather than a mode inside it:
      // the two share a promise but not a destination picker, a credential
      // model, or a lifetime — a local copy returns, a remote one is queued.
      { id: "transfers", label: "Remote transfers", needsScan: false },
    ],
  },
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
  // Subscribed here rather than in TransfersRoute, so a running upload keeps
  // updating the queue while the user is looking at the treemap — and so a
  // transfer that finishes while this panel is closed is already correct when
  // they open it.
  useTransferProgress();

  const route = useUiStore((state) => state.route);
  const detailsOpen = useUiStore((state) => state.detailsOpen);
  const detailsPinned = useUiStore((state) => state.detailsPinned);
  const setDetailsOpen = useUiStore((state) => state.setDetailsOpen);
  const setDetailsPinned = useUiStore((state) => state.setDetailsPinned);
  const splitView = useUiStore((state) => state.splitView);
  const setSplitView = useUiStore((state) => state.setSplitView);
  const destinationPath = useUiStore((state) => state.destinationPath);
  const setDestinationPath = useUiStore((state) => state.setDestinationPath);
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
  const selectMany = useUiStore((state) => state.selectMany);
  const clearSelection = useUiStore((state) => state.clearSelection);
  const setHovered = useUiStore((state) => state.setHovered);
  const setFocused = useUiStore((state) => state.setFocused);
  const setLayoutKind = useUiStore((state) => state.setLayoutKind);
  const setSizeMetric = useUiStore((state) => state.setSizeMetric);
  const deletionArmed = useUiStore((state) => state.deletionArmed);
  const setDeletionArmed = useUiStore((state) => state.setDeletionArmed);

  const currentRoot = useCurrentRoot();
  const soleSelection = useSoleSelection();
  // Also feeds the volume picker route; the query is shared and cached, so the
  // drive switcher costs no extra IPC.
  const volumes = useVolumes();
  // Which drives can be restored rather than rescanned. Header-only on the
  // backend, so this is cheap enough to keep fresh alongside the volume list.
  const offers = useSnapshotOffers();
  // Only while the panel is open: reading this walks the store directory and
  // peeks every file, which is cheap but not free.
  const storage = useStorageReport(route === "storage");
  const setSnapshotDir = useSetSnapshotDir();

  const [sorting, setSorting] = useState<SortingState>([{ id: "logical", desc: true }]);
  const [starting, setStarting] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  /** The root of the scan in flight, for the progress panel's denominator. */
  const [scanRoot, setScanRoot] = useState<string | null>(null);
  /** Which report row is expanded. One per route; they are independent views. */
  const [expandedCategory, setExpandedCategory] = useState<number | null>(null);
  const [expandedAgeBucket, setExpandedAgeBucket] = useState<number | null>(null);
  const [diffMetric, setDiffMetric] = useState<DiffMetricKind>("allocated");
  /**
   * "Now" for the Ages report, pinned once per session.
   *
   * It is part of the query key, and `Date.now()` changes every second — which
   * would be one full O(subtree) walk per second over a twelve-million-node
   * tree. The buckets are day-scale, so hour granularity is invisible in the
   * answer and the difference in cost is total.
   */
  const [nowUnixSeconds] = useState(pinnedNowUnixSeconds);
  /**
   * The nodes the move dialog is open on. Empty means closed.
   *
   * An array rather than a single id because migrating is inherently plural —
   * the user picks out the twelve things they want off this disk, not one.
   */
  const [relocating, setRelocating] = useState<readonly number[]>([]);
  /** Combined size of the selection, reported up by the table that holds the rows. */
  const [selectionBytes, setSelectionBytes] = useState(0);
  // Family first: a whole-volume scan puts far more than a dozen categories on
  // screen at once, and past that the per-category hues stop being tellable
  // apart at tile size. Drilling in is when switching to `category` pays.
  const [colorBy, setColorBy] = useState<ColorBy>("family");
  /**
   * The legend's filter, as `CategoryId`s. `null` is "show everything".
   *
   * Always category ids even while the legend shows families: the family
   * grouping is a presentation choice and the backend has no concept of it, so
   * `CategoryLegend` expands a family to its members before it gets here.
   */
  const [categoryFilter, setCategoryFilter] = useState<readonly number[] | null>(null);

  // Switching between family and category rebuilds what a row *means*, so a
  // filter carried across the switch would be a selection the user cannot see
  // the shape of — half a family checked, with no row showing it. Clearing is
  // the honest reset.
  useEffect(() => setCategoryFilter(null), [colorBy]);

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

  // The menu-bar panel is a separate webview and cannot set this window's route
  // directly. It shows the window and then asks, which keeps navigation owned by
  // the shell that renders it rather than split across two webviews.
  useEffect(() => {
    let stop: (() => void) | undefined;
    let live = true;
    void import("@tauri-apps/api/event")
      .then(({ listen }) => listen(OPEN_SETTINGS_EVENT, () => setRoute("storage")))
      .then((unlisten) => {
        if (live) stop = unlisten;
        else unlisten();
      })
      .catch(() => {
        // No event bridge outside the Tauri host (a browser dev server). The
        // titlebar's own settings control still works; only the tray's does not.
      });
    return () => {
      live = false;
      stop?.();
    };
  }, []);

  /*
   * A selection opens the panel. This is what makes "slide-in" the default
   * rather than a control the user has to find: the panel appears when there is
   * something for it to say, which is the same moment it used to stop saying
   * "select an item".
   *
   * Only when unpinned — a pinned panel is already open, and setting `open`
   * again on every selection would fight the user's own dismissals.
   */
  useEffect(() => {
    if (!detailsPinned && soleSelection !== null) setDetailsOpen(true);
  }, [detailsPinned, soleSelection, setDetailsOpen]);

  /*
   * Escape dismisses the sliding panel.
   *
   * Bound at the document rather than on the panel because the panel does not
   * hold focus — it slides in beside whatever you were doing, and requiring a
   * click into it before Escape works would mean the key does nothing at the
   * moment you most want it. Ignored while pinned, where dismissal is not the
   * gesture on offer.
   */
  useEffect(() => {
    if (detailsPinned || !detailsOpen) return undefined;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setDetailsOpen(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [detailsOpen, detailsPinned, setDetailsOpen]);

  /*
   * The destination the move dialog should open on, when the split view
   * proposed one. Cleared on close so the next move from the tree's own
   * action does not inherit a folder chosen minutes ago for something else.
   */
  const [relocateDestination, setRelocateDestination] = useState<string | null>(null);

  /*
   * A drive offered to the scan bar by the switcher, for a drive with nothing
   * stored to restore. Cleared once a scan actually lands, so the field goes
   * back to following whatever is on screen rather than pinning an offer the
   * user has since acted on — or abandoned.
   */
  const [scanBarOffer, setScanBarOffer] = useState<string | null>(null);
  useEffect(() => setScanBarOffer(null), [generation]);

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
        `Move to Trash is not available yet for ${nodes.length} item${nodes.length === 1 ? "" : "s"}. ` +
          "It needs a confirmation step that shows exactly what will move, and moving things to the Trash without showing you that first is not something this will do.",
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
  // Each report is an O(subtree) walk, so each is gated on its own route.
  const categoryTotals = useCategoryTotals(generation, currentRoot, route === "types");
  const categoryEntries = useCategoryEntries(generation, currentRoot, expandedCategory);
  const ageBuckets = useAgeBuckets(generation, currentRoot, nowUnixSeconds, route === "ages");
  const ageEntries = useAgeBucketEntries(generation, currentRoot, nowUnixSeconds, expandedAgeBucket);
  const dupes = useDuplicateCandidates(generation, currentRoot, route === "dupes");
  const scanDiff = useScanDiff(generation, diffMetric, route === "diff");
  const relocatingDetails = useNodeDetails(generation, relocating.length === 1 ? relocating[0] : null);

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

  // Escape clears a multi-selection. The bar has a Clear button, but a bulk
  // selection is exactly the state a user most wants to abandon quickly, and
  // reaching for the mouse to do it is the wrong shape.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      // Not while the move dialog is open — Escape belongs to the modal there.
      if (event.key !== "Escape" || relocating.length > 0) return;
      clearSelection();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [clearSelection, relocating.length]);

  // ⌘↑ is the Finder shortcut for "enclosing folder", so it is the one a macOS
  // user will already try; ⇧⌘↑ goes all the way back to the scan root.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.metaKey || event.key !== "ArrowUp" || navStack.length < 2) return;
      event.preventDefault();
      navigateUpTo(event.shiftKey ? 0 : navStack.length - 2);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navStack.length, navigateUpTo]);

  /*
   * Switching drives is a *new scan*, not a view change.
   *
   * It discards the tree on screen along with the selection and the navigation
   * stack, and on a large volume it costs minutes. `handleScan` already refuses
   * while one is running (the app runs exactly one), so the guard here is about
   * not asking for work that would be thrown away rather than about safety.
   */
  const scanning = scanState === "scanning" || scanState === "cancelling" || scanState === "finalizing";
  /*
   * Choosing a drive supersedes whatever scan is running.
   *
   * The app runs exactly one scan, and the backend refuses to publish a
   * restored tree while one is in flight — correctly, because the scan is
   * about to publish over it. The switcher used to answer that by disabling
   * itself, which made "I want to look at the other drive instead" impossible
   * to express while the scan you no longer care about kept running: the
   * control showed a spinner, took no clicks, and the only way through was to
   * find Cancel in the status strip first.
   *
   * So a switch cancels first and then waits for the slot to actually free.
   * Cancellation is a request, not an instant: `CancelState::Acknowledged`
   * means the supervisor heard it, and the walk stops between directories. The
   * poll is on `scan_status` — the supervisor's own answer — rather than on a
   * timer, because "the scan has stopped" is a state transition and everything
   * else is a guess.
   */
  const switchingRef = useRef(false);
  const supersedeRunningScan = useCallback(async (): Promise<void> => {
    if (activeScan === null) return;
    setCancelling(true);
    await scanCancel(activeScan);
    // Bounded: an uninterruptible syscall on a network mount can hold a worker
    // for a while, and hanging the switcher forever is worse than saying so.
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = await scanStatus();
      if (status.state !== "scanning" && status.state !== "cancelling" && status.state !== "finalizing") {
        await client.invalidateQueries({ queryKey: queryKeys.scanStatus() });
        return;
      }
      await new Promise((resolve) => window.setTimeout(resolve, 100));
    }
    throw new Error(
      "The running scan has not stopped yet — it may be waiting on a slow or unresponsive disk. Try again in a moment.",
    );
  }, [activeScan, client]);

  /**
   * Runs `next` with the scan slot free, serialised against other switches.
   *
   * The guard is a ref rather than state because it must not re-render the
   * shell: two clicks in the menu inside the same tick would otherwise both
   * pass a state check and race, and the second `scan_start` would come back
   * `AlreadyScanning` — a confusing error for a button that looks idle.
   */
  /**
   * Plain English for the one refusal a drive switch can still hit.
   *
   * `restore_snapshot` answers `internal: a scan is running; cancel it first`,
   * which is the right refusal and the wrong sentence to put in front of
   * someone: it names a command they did not type and a state they thought
   * they had already dealt with. `switchDrive` stops the scan first precisely
   * so this should not happen — but a scan started from another window, or one
   * that has not finished stopping, can still produce it, and the message
   * should say what to do rather than what the backend called it.
   */
  const describeSwitchFailure = (cause: unknown): string => {
    const message = cause instanceof Error ? cause.message : String(cause);
    if (message.includes("a scan is running")) {
      return "That drive could not be put on screen because a scan is still stopping. Try again in a moment.";
    }
    return message;
  };

  const switchDrive = useCallback(
    (next: () => Promise<void>) => {
      if (switchingRef.current) return;
      switchingRef.current = true;
      setActionError(null);
      void (async () => {
        try {
          await supersedeRunningScan();
          await next();
        } catch (cause) {
          setActionError(describeSwitchFailure(cause));
        } finally {
          switchingRef.current = false;
          setCancelling(false);
        }
      })();
    },
    [supersedeRunningScan],
  );

  const handleSwitchDrive = useCallback(
    (mountPoint: string) => {
      switchDrive(() => handleScan(mountPoint));
    },
    [handleScan, switchDrive],
  );

  /*
   * Restoring is the cheap path: the tree comes off disk rather than off the
   * filesystem, so a drive scanned before switches in about a second.
   *
   * The backend refuses while a scan is running rather than replacing a tree
   * that scan is about to publish over, so this goes through `switchDrive`,
   * which stops the running scan and waits for the slot before asking. The
   * refusal is still surfaced if it happens anyway — a switch that silently
   * did nothing would be worse than one that explains itself.
   */
  const handleRestoreDrive = useCallback(
    (mountPoint: string, device: number) => {
      switchDrive(async () => {
        await restoreSnapshot(mountPoint, device);
        await client.invalidateQueries({ queryKey: queryKeys.scanStatus() });
      });
    },
    [client, switchDrive],
  );

  return (
    <div className="flex h-full flex-col">
      <Titlebar
        crumbs={crumbs}
        onNavigate={handleCrumbNavigate}
        onOpenSettings={() => setRoute("storage")}
        driveSelector={
          generation !== GENERATION_NONE && (
            <DriveSwitcher
              volumes={volumes.data ?? []}
              offers={offers.data ?? []}
              scanRootPath={summary?.rootPath ?? null}
              busy={scanning || starting}
              onScan={handleSwitchDrive}
              onRestore={handleRestoreDrive}
              onOfferToScanBar={setScanBarOffer}
            />
          )
        }
      >
        <ScanBar
          onScan={(root) => void handleScan(root)}
          busy={scanning || starting}
          scanRoot={scanBarOffer ?? summary?.rootPath ?? null}
        />
      </Titlebar>

      <div className="relative flex min-h-0 flex-1">
        <nav aria-label="Tools" className="flex w-36 shrink-0 flex-col gap-3 border-r border-border/60 p-2">
          {RAIL_GROUPS.map((group) => (
            <div key={group.id} className="flex flex-col gap-0.5">
              {/* A real heading, not a styled div: the rail is the app's
                * primary navigation and a screen-reader user should be able to
                * jump between tool families rather than hearing ten peer
                * buttons with no structure. */}
              <h2 className="px-2 pb-0.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
                {group.label}
              </h2>
              {group.items.map((item) => (
                <RailButton
                  key={item.id}
                  id={item.id}
                  label={item.label}
                  route={route}
                  onSelect={setRoute}
                  disabled={item.needsScan && generation === GENERATION_NONE}
                />
              ))}
            </div>
          ))}

          {/* Pinned to the bottom and outside the groups: this is about the
            * app's own data rather than any disk you asked it to look at.
            * Never disabled — "what is this app storing" is a fair question
            * before you have scanned anything, and the honest answer is
            * "nothing yet". */}
          <div className="mt-auto flex flex-col gap-0.5">
            <h2 className="px-2 pb-0.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
              App
            </h2>
            <RailButton id="storage" label="Stored data" route={route} onSelect={setRoute} />
          </div>
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

          {route === "sync" && <SyncRoute />}
          {route === "transfers" && <TransfersRoute />}

          {route === "storage" && (
            <StoragePanel
              report={storage.data ?? null}
              loading={storage.isLoading}
              onRevealDirectory={() => {
                const directory = storage.data?.directory;
                // Reveal rather than open: the store holds the app's own data
                // and the user wants to SEE it, not have something try to
                // interpret a 960 MB binary.
                if (directory !== undefined) void revealItemInDir(directory);
              }}
              onChangeDirectory={async (directory) => {
                // Deliberately not caught here: the panel renders the
                // backend's reason inline, next to the field the user just
                // edited, rather than in the shell-wide error strip where it
                // would be detached from the thing that caused it.
                await setSnapshotDir.mutateAsync(directory);
              }}
              // Opened here rather than as an `href` in the panel: following a
              // link inside the webview would replace the whole app with a web
              // page and leave no way back.
              onOpenAuthor={() => void openUrl(AUTHOR_URL)}
              onExport={(snapshot) => {
                setActionError(null);
                exportSnapshot(snapshot.path)
                  .then((written) => setActionError(`Exported to ${written}`))
                  .catch((cause: unknown) => {
                    setActionError(cause instanceof Error ? cause.message : String(cause));
                  });
              }}
            />
          )}

          {route === "volumes" && <VolumePicker onScan={(root) => void handleScan(root)} busy={starting} />}

          {route === "types" && currentRoot !== null && (
            <TypesRoute
              rows={categoryTotals.data}
              isLoading={categoryTotals.isLoading}
              error={categoryTotals.error}
              subtreeAllocated={rootDetails.data?.subtree?.allocated ?? null}
              expanded={expandedCategory}
              onExpandedChange={setExpandedCategory}
              entries={categoryEntries.data}
              entriesLoading={categoryEntries.isLoading}
              entriesError={categoryEntries.error}
              entryLimit={MAX_REPORT_ENTRIES}
              onReveal={handleReveal}
              onTrash={(node) => handleTrash([node])}
              trashEnabled={deletionArmed}
              className="min-h-0 flex-1"
            />
          )}

          {route === "ages" && currentRoot !== null && (
            <AgesRoute
              rows={ageBuckets.data}
              isLoading={ageBuckets.isLoading}
              error={ageBuckets.error}
              subtreeAllocated={rootDetails.data?.subtree?.allocated ?? null}
              nowUnixSeconds={nowUnixSeconds}
              expandedBucket={expandedAgeBucket}
              onExpandedBucketChange={setExpandedAgeBucket}
              entries={ageEntries.data}
              entriesLoading={ageEntries.isLoading}
              entriesError={ageEntries.error}
              entryLimit={MAX_REPORT_ENTRIES}
              onReveal={handleReveal}
              onTrash={(node) => handleTrash([node])}
              trashEnabled={deletionArmed}
              className="min-h-0 flex-1"
            />
          )}

          {route === "dupes" && currentRoot !== null && (
            <DupesRoute
              generation={generation}
              report={dupes.data}
              isLoading={dupes.isLoading}
              error={dupes.error}
              onReveal={handleReveal}
              onTrash={(node) => handleTrash([node])}
              trashEnabled={deletionArmed}
              className="min-h-0 flex-1"
            />
          )}

          {route === "diff" && (
            <DiffRoute
              report={scanDiff.data}
              isLoading={scanDiff.isLoading}
              // "There has only ever been one scan of this volume" is not an
              // error, it is the honest answer to "what changed since last
              // time". The backend says so in the message; surfacing it as a
              // red failure would read as a bug in the app.
              error={scanDiff.error?.message.includes("only one saved scan") === true ? null : scanDiff.error}
              unavailableReason={
                scanDiff.error?.message.includes("only one saved scan") === true
                  ? "This volume has only been scanned once, so there is nothing to compare against yet. Scan it again and the previous scan becomes the baseline."
                  : (scanDiff.error?.message ?? null)
              }
              onMetricChange={setDiffMetric}
              onReveal={handleReveal}
              onTrash={(node) => handleTrash([node])}
              trashEnabled={deletionArmed}
              className="min-h-0 flex-1"
            />
          )}

          {route === "sizes" && currentRoot !== null && (
            <SizeBands
              rows={sizeBands.data}
              isLoading={sizeBands.isLoading}
              error={sizeBands.error}
              subtreeAllocated={rootDetails.data?.subtree?.allocated ?? null}
              generation={generation}
              root={currentRoot}
              onReveal={handleReveal}
              onTrash={(node) => handleTrash([node])}
              trashEnabled={deletionArmed}
              selection={selection}
              onSelect={select}
              onFocusNode={setFocused}
              onHover={setHovered}
              className="min-h-0 flex-1"
            />
          )}

          {route === "tree" && currentRoot !== null && (
            <div className="flex min-h-0 flex-1">
              <div className="flex min-h-0 min-w-0 flex-1 flex-col">
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
                {/* Opening the split view seeds the destination with the
                  * volume list rather than with nothing: an empty path field is
                  * a question, and "/Volumes" is the answer for every move that
                  * goes to another disk, which is what this is for. */}
                <Button
                  size="sm"
                  variant={splitView ? "default" : "ghost"}
                  aria-pressed={splitView}
                  title={splitView ? "Close the destination pane" : "Split: choose where things go"}
                  onClick={() => {
                    if (!splitView && destinationPath.trim().length === 0) {
                      setDestinationPath("/Volumes");
                    }
                    setSplitView(!splitView);
                  }}
                >
                  <Columns2 aria-hidden />
                  Split
                </Button>
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

                {/* Top-right, on the toolbar line, and dismissible. These are
                  * permanent facts about a boot-volume scan rather than news,
                  * so as stacked banners between the chart and the table they
                  * were a fixed tax on the main view for the whole session. */}
                <ScanAlerts summary={summary} />
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
                categoryFilter={categoryFilter}
                metric={sizeMetric}
              />

              {/* The key to the colours. Without it the encoding is decoration:
                * you can see two tiles differ but not what the difference
                * means, which throws away the entire point of classifying by
                * content type. `present={null}` lists the whole taxonomy;
                * once the canvas reports which categories it actually drew,
                * pass that set instead so the legend shrinks on drill-down. */}
              <CategoryLegend
                colorBy={colorBy}
                present={null}
                onColorByChange={setColorBy}
                selected={categoryFilter}
                onFilterChange={setCategoryFilter}
              />

              <SelectionActions
                count={selection.size}
                bytes={selectionBytes}
                armed={deletionArmed}
                onMove={() => setRelocating([...selection])}
                onReveal={() => {
                  // Finder shows one item at a time; revealing twelve would
                  // open twelve windows. The focused row is the one the user
                  // most recently touched, so it is the honest single answer.
                  const target = focused ?? [...selection][0];
                  if (target !== undefined) handleReveal(target);
                }}
                onTrash={() => handleTrash([...selection])}
                onClear={clearSelection}
              />

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
                onSelectMany={selectMany}
                onSelectionBytes={setSelectionBytes}
                onDrillDown={navigateTo}
                onHover={setHovered}
                onReveal={handleReveal}
                onTrash={(node) => handleTrash([node])}
                trashEnabled={deletionArmed}
              />
              </div>

              {/* The second pane is a sibling of the tree column, not an
                * overlay: both are being read at once, and a destination you
                * have to dismiss to see what you are moving is not a split
                * view. */}
              {splitView && (
                <DestinationPane
                  className="w-96 shrink-0"
                  path={destinationPath}
                  onPathChange={setDestinationPath}
                  selectionCount={selection.size}
                  onMoveHere={(destination) => {
                    setRelocateDestination(destination);
                    setRelocating([...selection]);
                  }}
                />
              )}
            </div>
          )}
        </main>

        {/* Pinned: an ordinary column, and the layout reflows around it once.
          * Unpinned: the same element positioned over the content, so showing
          * and hiding it never reflows the tree underneath — a treemap that
          * relaid itself on every selection would move the tile you were about
          * to click. */}
        <div
          className={cn(
            detailsPinned
              ? "flex min-h-0 shrink-0"
              : cn(
                  "absolute inset-y-0 right-0 z-30 transition-transform duration-200 ease-out",
                  "motion-reduce:transition-none",
                  detailsOpen ? "translate-x-0 shadow-2xl" : "pointer-events-none translate-x-full",
                ),
          )}
          // Hidden from the accessibility tree when slid away, so a screen
          // reader does not read out a panel that is not on screen. `inert`
          // would be better but is not on every WebKit this ships to.
          aria-hidden={!detailsPinned && !detailsOpen}
        >
          <DetailsPanel
            generation={generation}
            node={soleSelection}
            selectionCount={selection.size}
            onReveal={handleReveal}
            onTrash={handleTrash}
            onRelocate={(node) => setRelocating([node])}
            onTrashDropped={handleTrash}
            deletionArmed={deletionArmed}
            onArmDeletion={setDeletionArmed}
            pinned={detailsPinned}
            onTogglePin={() => setDetailsPinned(!detailsPinned)}
            onClose={() => setDetailsOpen(false)}
            className={detailsPinned ? undefined : "h-full bg-background"}
          />
        </div>

        {/* The way back in when nothing is selected. Without it the panel is
          * only reachable by selecting something, which makes the pin — and the
          * deletion switch that lives in the panel — unreachable from an empty
          * selection. */}
        {!detailsPinned && !detailsOpen && (
          <button
            type="button"
            onClick={() => setDetailsOpen(true)}
            title="Show details"
            className="absolute right-0 top-1/2 z-20 -translate-y-1/2 rounded-l-md border border-r-0 border-border/60 bg-background/90 px-1 py-3 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            <PanelRightOpen aria-hidden className="size-4" />
            <span className="sr-only">Show details</span>
          </button>
        )}
      </div>

      <RelocateDialog
        generation={generation}
        nodes={relocating}
        sourcePath={relocating.length === 1 ? (relocatingDetails.data?.path ?? null) : null}
        scanRootPath={summary?.rootPath ?? null}
        deletionArmed={deletionArmed}
        initialDestination={relocateDestination}
        onClose={() => {
          setRelocating([]);
          setRelocateDestination(null);
        }}
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
