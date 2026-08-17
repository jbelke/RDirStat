/* =============================================================================
 * Node id -> human text, cached per generation.
 *
 * The pinned `layout` schema carries `node, depth, x, y, w, h, category` and no
 * name, path or byte count — by design, because a name column would put the
 * whole arena's names on the wire. So the tooltip and the accessible list ask
 * the backend, through an injected describer (`path_of` / `node_details`), and
 * the answers are memoised for the life of the generation.
 *
 * Without a describer the surfaces still work: they fall back to the node id and
 * the share-of-view derived from the geometry. That is a real, if thinner,
 * rendering — not a placeholder.
 * ========================================================================== */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { NodeDescriber, NodeDescription } from "./types.ts";

export interface NodeDescriptionStore {
  /** Cached description, or `undefined` if not resolved yet. */
  get(node: number): NodeDescription | undefined;
  /** Request a description. Idempotent; safe to call from a render effect. */
  request(node: number): void;
  /** Bumps whenever a new description lands, so consumers can re-render. */
  readonly revision: number;
}

const EMPTY: NodeDescription = {};

export function useNodeDescriptions(describer: NodeDescriber | undefined, generation: string): NodeDescriptionStore {
  const cache = useRef(new Map<number, NodeDescription>());
  const inflight = useRef(new Set<number>());
  const controller = useRef<AbortController | null>(null);
  const [revision, setRevision] = useState(0);

  // A generation change invalidates every cached path and size.
  useEffect(() => {
    cache.current.clear();
    inflight.current.clear();
    controller.current?.abort();
    const next = new AbortController();
    controller.current = next;
    setRevision((value) => value + 1);
    return () => {
      next.abort();
      if (controller.current === next) controller.current = null;
    };
  }, [generation, describer]);

  const request = useCallback(
    (node: number): void => {
      if (describer === undefined) return;
      if (cache.current.has(node) || inflight.current.has(node)) return;
      const active = controller.current;
      if (active === null) return;
      inflight.current.add(node);
      void describer(node, active.signal)
        .then((description) => {
          if (active.signal.aborted) return;
          cache.current.set(node, description ?? EMPTY);
          setRevision((value) => value + 1);
        })
        .catch(() => {
          if (active.signal.aborted) return;
          // A failed lookup is cached as "nothing known" so a broken node does
          // not re-issue an IPC call on every mousemove.
          cache.current.set(node, EMPTY);
          setRevision((value) => value + 1);
        })
        .finally(() => {
          inflight.current.delete(node);
        });
    },
    [describer],
  );

  const get = useCallback((node: number): NodeDescription | undefined => cache.current.get(node), []);

  return useMemo(() => ({ get, request, revision }), [get, request, revision]);
}
