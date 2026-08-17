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
  children,
  nodeDetails,
  scanStatus,
  volumes,
  type ChildPageView,
  type DetailsView,
  type ScanStatusView,
  type Sort,
  type VolumeRow,
} from "@/lib/ipc";
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
  volumes: () => ["volumes"] as const,
  children: (generation: number, parent: number, sort: Sort) =>
    ["children", generation, parent, sort.key, sort.direction] as const,
  details: (generation: number, node: number) => ["details", generation, node] as const,
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
      if (scope !== "children" && scope !== "details") return false;
      return typeof generation === "number" && generation !== live;
    },
  });
}

/**
 * Scan lifecycle. Polled only while a scan is in flight — the 10 Hz progress
 * event drives the numbers, and this exists for the *transition* (the contract
 * is explicit that completion is a supervisor state change, never inferred
 * from a gap in progress events).
 */
export function useScanStatus(): UseQueryResult<ScanStatusView, Error> {
  return useQuery({
    queryKey: queryKeys.scanStatus(),
    queryFn: scanStatus,
    staleTime: 0,
    refetchInterval: (query) => {
      const state = query.state.data?.state;
      return state === "scanning" || state === "cancelling" || state === "finalizing" ? 500 : false;
    },
  });
}

/**
 * Mounted volumes. `statfs` is cheap but not free and the numbers move, so this
 * one is allowed to go stale and refetch on demand.
 */
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
