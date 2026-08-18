/**
 * Flattening for the hierarchy table.
 *
 * docs/05-UI.md: "The hierarchy table flattens only expanded, fetched pages."
 * That sentence rules out the two obvious implementations — it is not a
 * client-side tree with lazy leaves, and it is not `useInfiniteQuery` per row
 * (React hooks cannot be called per element of a set that changes at runtime).
 *
 * So expansion is explicit state and paging is imperative, through
 * `QueryClient.fetchQuery`. Every page still lands in the TanStack Query cache
 * under a key whose second element is the `TreeGeneration`, which is what makes
 * `dropStaleGenerations` able to evict a whole superseded tree without
 * enumerating node ids. The cache is doing its job — it is just not the thing
 * driving the render.
 *
 * Invariants this hook maintains:
 *
 * - A cursor is only ever handed back to the same (generation, parent, sort)
 *   it came from. The backend rejects a mismatched one, but re-deriving the key
 *   here means we never send one.
 * - Changing the sort or the generation clears everything. A merged half-sorted
 *   directory is worse than a refetch.
 * - The synthetic `<Files>` group IS expandable, and is a real level of the
 *   hierarchy: a directory yields its subdirectories plus the group, and the
 *   group yields that directory's own files. Each byte therefore appears at
 *   exactly one level. It previously yielded both the files and a group
 *   summarising them, which double-counted, and `children` on the group was an
 *   error — so the largest row in a directory could not be opened.
 * - Depth is capped. `MAX_TREE_DEPTH` is 4096 in the arena; the UI stops
 *   indenting long before that, and a cycle (which `Tree::validate` should have
 *   rejected) cannot lock the browser here.
 */

import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { children, type ChildPageView, type Sort, type TreeRow } from "@/lib/ipc";
import { queryKeys } from "@/lib/queries";
import { GENERATION_NONE, isRealNode } from "@/lib/wire";

/** Indentation stops being useful long before the arena's 4096-level ceiling. */
export const MAX_VISUAL_DEPTH = 64;

const PAGE_SIZE = 200;

export interface FlatRow {
  readonly row: TreeRow;
  readonly depth: number;
  readonly parent: number;
  /** Denominator for the `%Bar`: the parent's total in the same metric. */
  readonly parentLogical: number;
  readonly parentAllocated: number;
  readonly isExpanded: boolean;
  readonly isLoading: boolean;
}

interface NodePages {
  rows: TreeRow[];
  next: string | null;
  total: number;
  loading: boolean;
  error: Error | null;
}

export interface TreeRowsState {
  rows: readonly FlatRow[];
  expanded: ReadonlySet<number>;
  toggle: (node: number) => void;
  expand: (node: number) => void;
  collapse: (node: number) => void;
  /** Fetch the next cursor page for one expanded directory. */
  loadMore: (node: number) => void;
  hasMore: (node: number) => boolean;
  isLoading: boolean;
  error: Error | null;
  /** Server-side child count of the navigation root, for `aria-rowcount`. */
  rootTotal: number;
}

export interface UseTreeRowsOptions {
  generation: number;
  /** The subtree currently in view — the last element of the navigation stack. */
  root: number;
  sort: Sort;
  /** The root's own totals, used as the top-level `%Bar` denominator. */
  rootLogical: number;
  rootAllocated: number;
}

export function useTreeRows({
  generation,
  root,
  sort,
  rootLogical,
  rootAllocated,
}: UseTreeRowsOptions): TreeRowsState {
  const client = useQueryClient();

  const [expanded, setExpanded] = useState<ReadonlySet<number>>(() => new Set([root]));
  const [pages, setPages] = useState<ReadonlyMap<number, NodePages>>(() => new Map());
  const [error, setError] = useState<Error | null>(null);

  // Fetches in flight, so an effect re-run cannot double-request a page.
  const inFlight = useRef(new Set<string>());

  const sortKey = `${sort.key}:${sort.direction}`;

  // Generation, root, or sort changed: nothing cached is still addressable.
  useEffect(() => {
    inFlight.current.clear();
    setPages(new Map());
    setExpanded(new Set([root]));
    setError(null);
  }, [generation, root, sortKey]);

  const fetchPage = useCallback(
    async (parent: number, cursor: string | null) => {
      const guard = `${generation}:${parent}:${sortKey}:${cursor ?? ""}`;
      if (inFlight.current.has(guard)) return;
      inFlight.current.add(guard);

      setPages((current) => {
        const next = new Map(current);
        const existing = next.get(parent);
        next.set(parent, {
          rows: existing?.rows ?? [],
          next: existing?.next ?? null,
          total: existing?.total ?? 0,
          loading: true,
          error: null,
        });
        return next;
      });

      try {
        const page: ChildPageView = await client.fetchQuery({
          // The cursor is part of the key: two pages of the same directory are
          // two cache entries, and neither can be served for the other.
          queryKey: [...queryKeys.children(generation, parent, sort), cursor ?? "first"],
          queryFn: () => children(generation, parent, sort, cursor, PAGE_SIZE),
          staleTime: Number.POSITIVE_INFINITY,
        });

        setPages((current) => {
          const next = new Map(current);
          const existing = next.get(parent);
          // A first page replaces; a cursor page appends. Appending a first
          // page would duplicate every row on a refetch.
          const rows = cursor === null ? page.rows.slice() : [...(existing?.rows ?? []), ...page.rows];
          next.set(parent, {
            rows,
            next: page.next,
            total: page.totalChildren,
            loading: false,
            error: null,
          });
          return next;
        });
      } catch (cause) {
        const failure = cause instanceof Error ? cause : new Error(String(cause));
        setError(failure);
        setPages((current) => {
          const next = new Map(current);
          const existing = next.get(parent);
          next.set(parent, {
            rows: existing?.rows ?? [],
            next: existing?.next ?? null,
            total: existing?.total ?? 0,
            loading: false,
            error: failure,
          });
          return next;
        });
      } finally {
        inFlight.current.delete(guard);
      }
    },
    [client, generation, sort, sortKey],
  );

  // Request the first page of anything expanded that has none yet.
  useEffect(() => {
    if (generation === GENERATION_NONE || !isRealNode(root)) return;
    for (const node of expanded) {
      if (!pages.has(node)) void fetchPage(node, null);
    }
  }, [expanded, pages, fetchPage, generation, root]);

  const rows = useMemo(() => {
    const flat: FlatRow[] = [];
    if (generation === GENERATION_NONE) return flat;

    const walk = (parent: number, depth: number, parentLogical: number, parentAllocated: number) => {
      if (depth > MAX_VISUAL_DEPTH) return;
      const page = pages.get(parent);
      if (page === undefined) return;
      for (const row of page.rows) {
        const isExpanded = expanded.has(row.node);
        flat.push({
          row,
          depth,
          parent,
          parentLogical,
          parentAllocated,
          isExpanded,
          isLoading: isExpanded && (pages.get(row.node)?.loading ?? !pages.has(row.node)),
        });
        if (isExpanded) walk(row.node, depth + 1, row.logical, row.allocated);
      }
    };

    walk(root, 0, rootLogical, rootAllocated);
    return flat;
  }, [pages, expanded, root, rootLogical, rootAllocated, generation]);

  const toggle = useCallback((node: number) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(node)) next.delete(node);
      else next.add(node);
      return next;
    });
  }, []);

  const expand = useCallback((node: number) => {
    setExpanded((current) => (current.has(node) ? current : new Set(current).add(node)));
  }, []);

  const collapse = useCallback((node: number) => {
    setExpanded((current) => {
      if (!current.has(node)) return current;
      const next = new Set(current);
      next.delete(node);
      return next;
    });
  }, []);

  const loadMore = useCallback(
    (node: number) => {
      const page = pages.get(node);
      if (page === undefined || page.next === null || page.loading) return;
      void fetchPage(node, page.next);
    },
    [pages, fetchPage],
  );

  const hasMore = useCallback((node: number) => pages.get(node)?.next != null, [pages]);

  const rootPage = pages.get(root);

  return {
    rows,
    expanded,
    toggle,
    expand,
    collapse,
    loadMore,
    hasMore,
    isLoading: rootPage?.loading ?? (generation !== GENERATION_NONE && rootPage === undefined),
    error: rootPage?.error ?? error,
    rootTotal: rootPage?.total ?? 0,
  };
}
