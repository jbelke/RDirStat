/**
 * Local UI state. **Nothing that came from a command lives here.**
 *
 * docs/01-ARCHITECTURE.md, "Frontend boundary":
 *
 *   "Zustand holds only local UI state (selection, navigation stack, layout
 *    toggle); everything that came from a command is TanStack Query cache,
 *    keyed by `TreeGeneration` so a new scan invalidates rather than mixes."
 *
 * So this store may hold node *ids* but never node *rows*, a navigation stack
 * but never the paths it resolves to, and a size-metric preference but never a
 * size. Duplicating server data into Zustand is how the two caches drift and
 * how a stale row survives a rescan.
 *
 * The one piece of coupling that has to exist: a selection is only meaningful
 * inside the generation it was made in. `syncGeneration` is called by the shell
 * whenever the live generation changes, and it drops selection, focus, and the
 * navigation stack rather than letting them point into a freed arena. That is
 * the "a generation change invalidates both cached pages and stale selections"
 * rule from docs/05-UI.md, and it belongs on the state that would otherwise
 * survive the invalidation.
 */

import { create } from "zustand";

import type { LayoutKind } from "@/lib/bindings";
import { GENERATION_NONE, NODE_ID_ROOT } from "@/lib/wire";

/** Left-rail routes. Only the two that exist in this build are reachable. */
export type Route =
  | "volumes"
  | "overview"
  | "tree"
  | "sizes"
  | "types"
  | "ages"
  | "diff"
  | "dupes"
  | "sync"
  | "transfers"
  | "storage";

/**
 * Which byte count the size columns and the canvas show.
 *
 * docs/05-UI.md makes this an explicit user choice rather than a silent
 * default, "so a screenshot is never ambiguous about which number it is
 * showing". Logical and allocated are never summed or reconciled.
 */
export type SizeMetric = "logical" | "allocated";

export interface UiState {
  route: Route;

  /** The generation the ids below were resolved in. `0` == nothing loaded. */
  generation: number;

  /**
   * Drill-down stack, root-first. The last element is the current subtree and
   * drives both the breadcrumb and the canvas root. Never empty once a scan is
   * loaded.
   */
  navStack: readonly number[];

  /** Multi-selection, for the details panel and the trash drop target. */
  selection: ReadonlySet<number>;

  /**
   * Keyboard focus. Independent of selection on purpose — docs/05-UI.md
   * requires focus, selection, and hover to be three distinct states, because
   * merging them is what makes arrow-key navigation destructive.
   */
  focused: number | null;

  /** Hover, for tree <-> canvas highlight sync. Not persisted, not selection. */
  hovered: number | null;

  layoutKind: LayoutKind;
  sizeMetric: SizeMetric;

  /**
   * Whether the details panel is on screen. Meaningful only while unpinned —
   * a pinned panel is always present, which is what pinning means.
   */
  detailsOpen: boolean;

  /**
   * Docked, rather than sliding in over the content.
   *
   * Unpinned is the default because the panel is selection-driven: for most of
   * a session there is no selection, and an empty 320-point column that says
   * "select an item" is spending permanent width on a transient answer. Pinning
   * is for the workflow where you *are* reading details continuously and want
   * the tree to reflow around them once rather than jump on every selection.
   *
   * Persisted, unlike everything else here, because it is a statement about how
   * someone works rather than about the scan in front of them. `deletionArmed`
   * two fields down is the opposite case and is deliberately never persisted.
   */
  detailsPinned: boolean;

  /**
   * Whether the destructive actions are live.
   *
   * **Off by default, and it is never persisted.** Moving items to the Trash is
   * the one thing this app does that the user cannot undo from inside it, so it
   * costs a deliberate gesture to switch on and it goes back off by itself:
   * `syncGeneration` disarms on every new tree, because the selection that was
   * armed against the old generation no longer means anything. A preference
   * that survived a relaunch would turn "I armed this once" into "this app
   * deletes on drop, forever".
   */
  deletionArmed: boolean;

  setRoute: (route: Route) => void;
  /** Drop everything id-shaped when the live tree is replaced. */
  syncGeneration: (generation: number, root?: number) => void;
  navigateTo: (node: number) => void;
  navigateUpTo: (depth: number) => void;
  /** Replace the navigation stack with an exact root-to-node path. */
  setNavPath: (nodes: readonly number[]) => void;
  select: (node: number, mode?: "replace" | "toggle" | "add") => void;
  /**
   * Select many nodes at once — a shift-click range, or select-all.
   *
   * Separate from `select` rather than a loop over it because a range is one
   * user gesture and should be one state transition: looping would fire a
   * render per row and, on a 500-row page, make a single shift-click feel like
   * a stutter.
   */
  selectMany: (nodes: readonly number[], mode?: "replace" | "add") => void;
  clearSelection: () => void;
  setFocused: (node: number | null) => void;
  setHovered: (node: number | null) => void;
  setLayoutKind: (kind: LayoutKind) => void;
  setSizeMetric: (metric: SizeMetric) => void;
  toggleDetails: () => void;
  setDetailsOpen: (open: boolean) => void;
  setDetailsPinned: (pinned: boolean) => void;
  /** Arms or disarms Trash. Only ever called from an explicit user gesture. */
  setDeletionArmed: (armed: boolean) => void;
}

const EMPTY_SELECTION: ReadonlySet<number> = new Set<number>();

/** Where the pin preference lives between launches. */
const DETAILS_PINNED_KEY = "rdirstat.detailsPinned";

/*
 * Both wrapped in try/catch rather than assumed.
 *
 * `localStorage` throws — not returns null — when storage is disabled or the
 * quota is exhausted, and a layout preference is not worth taking the whole app
 * down over. Failing to read means "unpinned", which is the default anyway;
 * failing to write means the pin lasts this session only, which is a smaller
 * loss than a blank window.
 */
function readPinned(): boolean {
  try {
    return window.localStorage.getItem(DETAILS_PINNED_KEY) === "true";
  } catch {
    return false;
  }
}

function writePinned(pinned: boolean): void {
  try {
    window.localStorage.setItem(DETAILS_PINNED_KEY, String(pinned));
  } catch {
    // Preference not retained. The panel still works.
  }
}

export const useUiStore = create<UiState>((set) => ({
  route: "volumes",
  generation: GENERATION_NONE,
  navStack: [],
  selection: EMPTY_SELECTION,
  focused: null,
  hovered: null,
  layoutKind: "treemap",
  sizeMetric: "allocated",
  detailsOpen: false,
  detailsPinned: readPinned(),
  deletionArmed: false,

  setRoute: (route) => set({ route }),

  syncGeneration: (generation, root = NODE_ID_ROOT) =>
    set((state) => {
      if (state.generation === generation) return state;
      const loaded = generation !== GENERATION_NONE;
      return {
        generation,
        navStack: loaded ? [root] : [],
        selection: EMPTY_SELECTION,
        focused: null,
        hovered: null,
        route: loaded ? "tree" : "volumes",
        // A new tree is a new set of ids. Whatever the user armed deletion for
        // is gone, so the arming goes with it.
        deletionArmed: false,
      };
    }),

  navigateTo: (node) =>
    set((state) => {
      const existing = state.navStack.indexOf(node);
      // Navigating to something already on the stack is a *pop*, not a push:
      // otherwise clicking the breadcrumb grows the stack it is displaying.
      if (existing >= 0) return { navStack: state.navStack.slice(0, existing + 1) };
      return { navStack: [...state.navStack, node] };
    }),

  navigateUpTo: (depth) =>
    set((state) => {
      if (depth < 0 || depth >= state.navStack.length) return state;
      return { navStack: state.navStack.slice(0, depth + 1) };
    }),

  // Replaces the stack outright with a known-good path.
  //
  // The breadcrumb is built from the tree's ancestor chain, not from where the
  // user has clicked, so it can offer a jump to an ancestor that was never on
  // the stack — zoom straight from the root to a file eight levels down and
  // every directory in between is reachable but unvisited. Pushing one of
  // those with `navigateTo` would leave the stack holding a path that is not a
  // path. Setting it from the chain keeps `navStack` exactly equal to the
  // route from the root to the current node, which is what `useCurrentRoot`
  // and every up-navigation assume it is.
  setNavPath: (nodes) => set(() => ({ navStack: [...nodes] })),

  select: (node, mode = "replace") =>
    set((state) => {
      if (mode === "replace") {
        return { selection: new Set([node]), focused: node };
      }
      const next = new Set(state.selection);
      if (mode === "toggle" && next.has(node)) {
        next.delete(node);
      } else {
        next.add(node);
      }
      return { selection: next, focused: node };
    }),

  selectMany: (nodes, mode = "replace") =>
    set((state) => {
      const next = mode === "add" ? new Set(state.selection) : new Set<number>();
      for (const node of nodes) next.add(node);
      // Focus follows the end of the range, which is where the user's cursor
      // actually is — anchoring it at the start would make a subsequent
      // shift-click extend from the wrong end.
      return { selection: next, focused: nodes.at(-1) ?? state.focused };
    }),

  clearSelection: () => set({ selection: EMPTY_SELECTION }),
  setFocused: (focused) => set({ focused }),
  setHovered: (hovered) => set({ hovered }),
  setLayoutKind: (layoutKind) => set({ layoutKind }),
  setSizeMetric: (sizeMetric) => set({ sizeMetric }),
  toggleDetails: () => set((state) => ({ detailsOpen: !state.detailsOpen })),

  setDetailsOpen: (detailsOpen) => set({ detailsOpen }),

  setDetailsPinned: (detailsPinned) => {
    writePinned(detailsPinned);
    // Pinning implies showing it: pinning a hidden panel and getting nothing
    // reads as a broken switch. Unpinning leaves it up so the content does not
    // vanish from under the click that unpinned it — it slides away on the
    // next dismissal instead.
    set({ detailsPinned, detailsOpen: true });
  },
  setDeletionArmed: (deletionArmed) => set({ deletionArmed }),
}));

/** The subtree currently being viewed, or `null` before a scan is loaded. */
export function useCurrentRoot(): number | null {
  return useUiStore((state) => state.navStack.at(-1) ?? null);
}

/** The single selected node, or `null` for an empty or multiple selection. */
export function useSoleSelection(): number | null {
  return useUiStore((state) => (state.selection.size === 1 ? [...state.selection][0] : null));
}
