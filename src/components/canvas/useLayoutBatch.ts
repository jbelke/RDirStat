/* =============================================================================
 * One `layout` request in, one validated batch out.
 *
 * Deliberately NOT TanStack Query. The canvas must work without a
 * `QueryClientProvider` above it (fixtures, Storybook-style harnesses, tests),
 * and the request is keyed by so much viewport state that the cache would
 * almost never hit. A shell that wants Query semantics passes a `fetchLayout`
 * backed by `queryClient.fetchQuery` — the seam is the fetcher, not the hook.
 *
 * The fail-closed rule lives here: on ANY error the previous batch is dropped.
 * A stale treemap under a fresh error banner is a wrong answer that looks right.
 * ========================================================================== */

import { useEffect, useMemo, useRef, useState } from "react";

import { decodeLayoutBatch } from "./arrow.ts";
import { LayoutError, describeTransportFailure } from "./errors.ts";
import { normalizeGeneration } from "./generation.ts";
import type { Generation, LayoutBatch, LayoutFetcher, LayoutKind, SizeMetric } from "./types.ts";

export interface UseLayoutBatchOptions {
  readonly generation: Generation;
  readonly root: number;
  readonly kind: LayoutKind;
  readonly width: number;
  readonly height: number;
  readonly devicePixelRatio: number;
  readonly minPx: number;
  readonly fetchLayout: LayoutFetcher;
  /** Category ids to keep, or `null` for everything. Part of the request key. */
  readonly categories?: readonly number[] | null;
  /**
   * Which byte count the areas encode. Part of the request key: changing it is
   * a different picture of the same tree, not a relabelling of this one.
   */
  readonly metric?: SizeMetric;
  /** Suppress the request (no scan loaded, zero-sized container). */
  readonly enabled: boolean;
}

export interface LayoutBatchState {
  readonly batch: LayoutBatch | null;
  readonly error: LayoutError | null;
  readonly loading: boolean;
  /** Milliseconds spent in `fetch + decode` for the batch on screen. */
  readonly loadMs: number | null;
  refetch(): void;
}

export function useLayoutBatch(options: UseLayoutBatchOptions): LayoutBatchState {
  const {
    generation,
    root,
    kind,
    width,
    height,
    devicePixelRatio,
    minPx,
    categories = null,
    metric,
    fetchLayout,
    enabled,
  } = options;

  const [batch, setBatch] = useState<LayoutBatch | null>(null);
  const [error, setError] = useState<LayoutError | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadMs, setLoadMs] = useState<number | null>(null);
  const [nonce, setNonce] = useState(0);
  // A stable key for the filter. The array identity changes on every render, so
  // depending on it directly would refetch the layout continuously; the joined
  // ids change only when the filter actually does.
  const categoriesKey = categories === null ? "" : [...categories].sort((a, b) => a - b).join(",");

  const fetcherRef = useRef(fetchLayout);
  fetcherRef.current = fetchLayout;

  // Normalising here means a `generation` prop that flips between 3, 3n and "3"
  // does not re-issue the request.
  const normalizedGeneration = useMemo(() => {
    try {
      return normalizeGeneration(generation);
    } catch {
      return null;
    }
  }, [generation]);

  useEffect(() => {
    if (!enabled || normalizedGeneration === null || width <= 0 || height <= 0) {
      setBatch(null);
      setError(
        normalizedGeneration === null
          ? new LayoutError("transport_failure", "The scan generation is not a valid number.", String(generation))
          : null,
      );
      setLoading(false);
      return;
    }

    const controller = new AbortController();
    let cancelled = false;
    setLoading(true);

    const started = typeof performance === "undefined" ? 0 : performance.now();

    void (async () => {
      try {
        const bytes = await fetcherRef.current(
          {
            generation: normalizedGeneration,
            root,
            kind,
            viewport: { width, height, devicePixelRatio },
            minPx,
            categories,
            metric,
          },
          controller.signal,
        );
        if (cancelled) return;
        const decoded = decodeLayoutBatch(bytes, { generation: normalizedGeneration, root, kind });
        if (cancelled) return;
        setBatch(decoded);
        setError(null);
        setLoadMs((typeof performance === "undefined" ? 0 : performance.now()) - started);
      } catch (cause) {
        if (cancelled) return;
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        // Fail closed: no tiles survive an error.
        setBatch(null);
        setLoadMs(null);
        setError(
          cause instanceof LayoutError
            ? cause
            : new LayoutError("transport_failure", "The layout request failed.", describeTransportFailure(cause), { cause }),
        );
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [
    categoriesKey,
    metric,
    enabled,
    normalizedGeneration,
    generation,
    root,
    kind,
    width,
    height,
    devicePixelRatio,
    minPx,
    nonce,
  ]);

  const refetch = useRef(() => setNonce((value) => value + 1)).current;

  return { batch, error, loading, loadMs, refetch };
}
