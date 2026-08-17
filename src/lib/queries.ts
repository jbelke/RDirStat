/**
 * TanStack Query hooks. Every cache entry that describes tree contents is
 * keyed by `TreeGeneration`, which is what makes a rescan an invalidation
 * rather than a merge — the old generation's entries simply stop being
 * addressable and age out.
 *
 * `invoke` is not HTTP, so a few defaults are wrong for us and are overridden
 * in `createQueryClient`:
 *
 * - **`retry: false` for tree queries.** A `QueryError` is a typed, terminal
 *   answer ("that generation is gone", "that node is a virtual group"); asking
 *   three more times cannot change it and only delays the correct UI.
 * - **`staleTime: Infinity` for anything generation-keyed.** A frozen arena is
 *   immutable by construction. Refetching page 3 of `/Users` after a window
 *   focus is pure waste.
 * - **`gcTime` short-ish**, because the thing being cached is up to 500 rows
 *   per expanded directory and a deep session can accumulate a lot of them.
 */

import {
  QueryClient,
  useInfiniteQuery,
  useQuery,
  type UseInfiniteQueryResult,
  type UseQueryResult,
} from "@tanstack/react-query";

import {
  ageBucketEntries,
  ageBuckets,
  ancestors,
  categoryEntries,
  categoryTotals,
  children,
  duplicateCandidates,
  nodeDetails,
  scanDiff,
  scanErrors,
  scanStatus,
  sizeBandEntries,
  sizeBands,
  snapshotOffers,
  volumes,
  type AncestorRow,
  type ChildPageView,
  type DetailsView,
  type ScanErrorsView,
  type AgeBucketEntryView,
  type AgeBucketView,
  type CategoryEntryView,
  type CategoryRowView,
  type DiffMetricKind,
  type DiffReportView,
  type DupesReportView,
  type ScanStatusView,
  type SizeBandEntryView,
  type SizeBandView,
  type SnapshotOfferView,
  type Sort,
  type VolumeRow,
} from "@/lib/ipc";
import { MAX_REPORT_ENTRIES as BINDINGS_MAX_REPORT_ENTRIES } from "@/lib/bindings";
import { GENERATION_NONE, isRealNode, isVirtualGroup, MAX_CHILD_PAGE } from "@/lib/wire";

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
        staleTime: Number.POSITIVE_INFINITY,
        gcTime: 5 * 60 * 1000,
      },
    },
  });
}

/**
 * Query keys, in one place so an invalidation cannot miss a shape.
 *
 * `generation` is always the **second** element, so
 * `queryClient.removeQueries({ predicate })` can drop an entire generation
 * without enumerating node ids.
 */
export const queryKeys = {
  scanStatus: () => ["scanStatus"] as const,
  scanErrors: () => ["scanErrors"] as const,
  volumes: () => ["volumes"] as const,
  snapshotOffers: () => ["snapshotOffers"] as const,
  children: (generation: number, parent: number, sort: Sort) =>
    ["children", generation, parent, sort.key, sort.direction] as const,
  details: (generation: number, node: number) => ["details", generation, node] as const,
  ancestors: (generation: number, node: number) => ["ancestors", generation, node] as const,
  sizeBands: (generation: number, node: number) => ["sizeBands", generation, node] as const,
  sizeBandEntries: (generation: number, node: number, band: number) =>
    ["sizeBandEntries", generation, node, band] as const,
  categoryTotals: (generation: number, node: number) => ["categoryTotals", generation, node] as const,
  categoryEntries: (generation: number, node: number, category: number) =>
    ["categoryEntries", generation, node, category] as const,
  ageBuckets: (generation: number, node: number, now: number) =>
    ["ageBuckets", generation, node, now] as const,
  ageBucketEntries: (generation: number, node: number, now: number, bucket: number) =>
    ["ageBucketEntries", generation, node, now, bucket] as const,
  dupes: (generation: number, node: number) => ["dupes", generation, node] as const,
  scanDiff: (generation: number, metric: DiffMetricKind) => ["scanDiff", generation, metric] as const,
} as const;

/**
 * Drop every cache entry belonging to a superseded generation.
 *
 * Cheaper and more honest than `invalidateQueries`: those entries can never be
 * valid again, so there is nothing to refetch. Called by the shell on a
 * generation change, alongside `useUiStore.syncGeneration`.
 */
export function dropStaleGenerations(client: QueryClient, live: number): void {
  client.removeQueries({
    predicate: (query) => {
      const [scope, generation] = query.queryKey as readonly unknown[];
      // Every generation-keyed scope must be listed here. A scope that is
      // omitted keeps answering from a tree that no longer exists.
      if (
        scope !== "children" &&
        scope !== "details" &&
        scope !== "ancestors" &&
        scope !== "sizeBands" &&
        scope !== "sizeBandEntries" &&
        scope !== "categoryTotals" &&
        scope !== "categoryEntries" &&
        scope !== "ageBuckets" &&
        scope !== "ageBucketEntries" &&
        scope !== "dupes" &&
        scope !== "scanDiff"
      ) {
        return false;
      }
      return typeof generation === "number" && generation !== live;
    },
  });
}

/**
 * Scan lifecycle. Polled only while a scan is in flight — the 10 Hz progress
 * event drives the numbers, and this exists for the *transition* (the contract
 * is explicit that completion is a supervisor state change, never inferred
 * from a gap in progress events).
 *
 * It also polls, far more slowly, while the app is `idle`. At launch the shell
 * restores the previous scan from a `*.rdstat` snapshot on a background thread;
 * that read finishes some seconds after the window appears and turns `idle`
 * into `ready` with a generation. Without a poll in `idle` the restored tree
 * would sit in memory unnoticed until the user did something.
 *
 * `idle` means *nothing has ever been published*, so this poll stops for good
 * the moment a tree exists — either the restore landed or a scan produced one.
 * It is an `O(1)` status read, not a tree query.
 */
const IDLE_POLL_MS = 1000;
const ACTIVE_POLL_MS = 500;

export function useScanStatus(): UseQueryResult<ScanStatusView, Error> {
  return useQuery({
    queryKey: queryKeys.scanStatus(),
    queryFn: scanStatus,
    staleTime: 0,
    refetchInterval: (query) => {
      const state = query.state.data?.state;
      if (state === "scanning" || state === "cancelling" || state === "finalizing") {
        return ACTIVE_POLL_MS;
      }
      return state === "idle" ? IDLE_POLL_MS : false;
    },
  });
}

/**
 * What the recorded scan failures were.
 *
 * **Fetched only while something is showing them.** `enabled` is the disclosure
 * state of the caller, because this is the one query in the app whose answer
 * nothing renders until asked: the strip shows a count from the progress event
 * and only reaches for the detail when the user opens it.
 *
 * While a scan is live the answer keeps changing, so it polls at 1 Hz — an
 * order of magnitude slower than the progress event, because a list of paths is
 * read, not watched. Once the scan is over the answer is final and the poll
 * stops.
 */
export function useScanErrors(enabled: boolean): UseQueryResult<ScanErrorsView, Error> {
  return useQuery({
    queryKey: queryKeys.scanErrors(),
    queryFn: () => scanErrors(),
    enabled,
    staleTime: 0,
    refetchInterval: (query) => (enabled && query.state.data?.live === true ? 1_000 : false),
  });
}

/**
 * Mounted volumes. `statfs` is cheap but not free and the numbers move, so this
 * one is allowed to go stale and refetch on demand.
 */
/**
 * Which drives could be restored from a snapshot rather than rescanned.
 *
 * Cheap by construction — the backend reads each snapshot's header and stops —
 * so this refetches on the same footing as the volume list. `staleTime` is
 * short rather than infinite because a scan that finishes writes a new
 * snapshot, which changes the answer.
 */
export function useSnapshotOffers(): UseQueryResult<SnapshotOfferView[], Error> {
  return useQuery({
    queryKey: queryKeys.snapshotOffers(),
    queryFn: snapshotOffers,
    staleTime: 15_000,
  });
}

export function useVolumes(): UseQueryResult<VolumeRow[], Error> {
  return useQuery({
    queryKey: queryKeys.volumes(),
    queryFn: volumes,
    staleTime: 15_000,
  });
}

export interface ChildrenQueryOptions {
  generation: number;
  parent: number;
  sort: Sort;
  /** Rows per request. Clamped to `MAX_CHILD_PAGE` by both sides. */
  pageSize?: number;
  enabled?: boolean;
}

/**
 * One expanded directory's children, paged by the backend's opaque cursor.
 *
 * `manualPagination` in the table sense: the frontend never holds more than the
 * pages it asked for, the cursor binds generation + parent + sort + a stable
 * `NodeId` tie-breaker, and a stale cursor is rejected in Rust rather than
 * silently paging the wrong tree.
 */
export function useChildren({
  generation,
  parent,
  sort,
  pageSize = 200,
  enabled = true,
}: ChildrenQueryOptions): UseInfiniteQueryResult<{ pages: ChildPageView[] }, Error> {
  const limit = Math.min(pageSize, MAX_CHILD_PAGE);
  return useInfiniteQuery({
    queryKey: queryKeys.children(generation, parent, sort),
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => children(generation, parent, sort, pageParam, limit),
    getNextPageParam: (lastPage) => lastPage.next,
    enabled: enabled && generation !== GENERATION_NONE && isRealNode(parent),
  });
}

/**
 * Details for the selected node.
 *
 * Disabled for a virtual `<Files>` group: it has no directory entry, so
 * `node_details` on it is `QueryError::VirtualGroup` by design. The panel
 * renders the group's aggregate from the row it already has instead of asking
 * a question with a known answer.
 */
export function useNodeDetails(
  generation: number,
  node: number | null,
): UseQueryResult<DetailsView, Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.details(generation, target),
    queryFn: () => nodeDetails(generation, target),
    enabled: generation !== GENERATION_NONE && node !== null && isRealNode(target) && !isVirtualGroup(target),
  });
}

/**
 * The size-band histogram for a subtree.
 *
 * `staleTime: Infinity` for the same reason every other generation-keyed query
 * uses it: a published arena is frozen, so the answer for `(generation, node)`
 * cannot change. A new scan means a new generation and a new key.
 *
 * `O(subtree)` on the backend, so this is fetched only when the Sizes route is
 * actually showing rather than kept warm.
 */
export function useSizeBands(
  generation: number,
  node: number | null,
  enabled = true,
): UseQueryResult<SizeBandView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.sizeBands(generation, target),
    queryFn: () => sizeBands(generation, target),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: enabled && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

/**
 * How many files any report's breakdown lists.
 *
 * Imported from the generated bindings rather than restated, because this
 * number had begun to exist independently in the command layer, here, and in
 * each report — four copies, each free to drift.
 */
export const MAX_REPORT_ENTRIES = BINDINGS_MAX_REPORT_ENTRIES;
/** @deprecated Use {@link MAX_REPORT_ENTRIES}; kept so existing imports resolve. */
export const BAND_ENTRY_LIMIT = MAX_REPORT_ENTRIES;

/**
 * The heaviest files inside one band.
 *
 * Fetched only while that band is expanded — this is an `O(subtree)` walk, and
 * six of them running because six accordions exist would be six full passes
 * over a twelve-million-node tree for rows nobody is looking at.
 */
export function useSizeBandEntries(
  generation: number,
  node: number | null,
  band: number | null,
): UseQueryResult<SizeBandEntryView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.sizeBandEntries(generation, target, band ?? -1),
    queryFn: () => sizeBandEntries(generation, target, band ?? 0, BAND_ENTRY_LIMIT),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: band !== null && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

/**
 * The report routes.
 *
 * Every one of these is an `O(subtree)` walk on the backend, so all of them are
 * gated on their own route being visible. Six of them running because six routes
 * exist would be six full passes over a twelve-million-node tree for answers
 * nobody is looking at.
 *
 * All are generation-keyed and `staleTime: Infinity` for the usual reason: a
 * published arena is frozen, so the answer for a given key cannot change.
 */
export function useCategoryTotals(
  generation: number,
  node: number | null,
  enabled = true,
): UseQueryResult<CategoryRowView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.categoryTotals(generation, target),
    queryFn: () => categoryTotals(generation, target),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: enabled && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

export function useCategoryEntries(
  generation: number,
  node: number | null,
  category: number | null,
): UseQueryResult<CategoryEntryView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.categoryEntries(generation, target, category ?? -1),
    queryFn: () => categoryEntries(generation, target, category ?? 0, MAX_REPORT_ENTRIES),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: category !== null && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

export function useAgeBuckets(
  generation: number,
  node: number | null,
  nowUnixSeconds: number,
  enabled = true,
): UseQueryResult<AgeBucketView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.ageBuckets(generation, target, nowUnixSeconds),
    queryFn: () => ageBuckets(generation, target, nowUnixSeconds),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: enabled && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

export function useAgeBucketEntries(
  generation: number,
  node: number | null,
  nowUnixSeconds: number,
  bucket: number | null,
): UseQueryResult<AgeBucketEntryView[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.ageBucketEntries(generation, target, nowUnixSeconds, bucket ?? -1),
    // The SAME `nowUnixSeconds` the bucket totals used. A mismatch is not
    // detectable by the backend and would silently produce a file list that
    // disagrees with the count above it.
    queryFn: () => ageBucketEntries(generation, target, nowUnixSeconds, bucket ?? 0, MAX_REPORT_ENTRIES),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: bucket !== null && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

export function useDuplicateCandidates(
  generation: number,
  node: number | null,
  enabled = true,
): UseQueryResult<DupesReportView, Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.dupes(generation, target),
    // Zero means "use the backend's own ceilings" rather than duplicating them here.
    queryFn: () => duplicateCandidates(generation, target, 0, 0),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: enabled && generation !== GENERATION_NONE && node !== null && isRealNode(target),
  });
}

/**
 * The Diff report.
 *
 * Not keyed on a node: a diff is always whole-scan against whole-scan. It is
 * also the one report that can legitimately be unavailable — there may only
 * ever have been one scan of this volume — so the caller distinguishes that
 * from an error rather than showing an empty diff.
 */
export function useScanDiff(
  generation: number,
  metric: DiffMetricKind,
  enabled = true,
): UseQueryResult<DiffReportView, Error> {
  return useQuery({
    queryKey: queryKeys.scanDiff(generation, metric),
    queryFn: () => scanDiff(generation, metric, MAX_REPORT_ENTRIES),
    staleTime: Number.POSITIVE_INFINITY,
    enabled: enabled && generation !== GENERATION_NONE,
  });
}

/**
 * The ancestor chain for the breadcrumb.
 *
 * `staleTime: Infinity` because a published tree is frozen: within one
 * generation a node's ancestors cannot change, so re-fetching could only ever
 * return the same answer. The generation is in the key, so a new scan gets a
 * new entry and `dropStaleGenerations` evicts the old one.
 */
export function useAncestors(
  generation: number,
  node: number | null,
): UseQueryResult<AncestorRow[], Error> {
  const target = node ?? -1;
  return useQuery({
    queryKey: queryKeys.ancestors(generation, target),
    queryFn: () => ancestors(generation, target),
    enabled: generation !== GENERATION_NONE && node !== null && isRealNode(target),
    staleTime: Number.POSITIVE_INFINITY,
  });
}
