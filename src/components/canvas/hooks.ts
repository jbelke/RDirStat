/* =============================================================================
 * Environment hooks: size, device pixel ratio, theme revision, reduced motion.
 *
 * Every one of these exists to keep a repaint tied to a DISCRETE event. The
 * canvas repaints on resize, navigation, selection and theme change — never on
 * a frame tick and never on mousemove.
 * ========================================================================== */

import { useEffect, useState, type RefObject } from "react";

import { resolvePalette, type ColorBy, type Palette } from "./palette.ts";

export interface Size {
  readonly width: number;
  readonly height: number;
}

const ZERO_SIZE: Size = { width: 0, height: 0 };

/**
 * Observe an element's content box. Updates are coalesced through a short timer
 * so a window drag produces a handful of `layout` requests, not one per frame.
 */
export function useElementSize<T extends HTMLElement>(ref: RefObject<T | null>, settleMs = 100): Size {
  const [size, setSize] = useState<Size>(ZERO_SIZE);

  useEffect(() => {
    const element = ref.current;
    if (element === null || typeof ResizeObserver === "undefined") return;

    let timer: ReturnType<typeof setTimeout> | null = null;
    let pending: Size = ZERO_SIZE;

    const flush = (): void => {
      timer = null;
      setSize((previous) => (previous.width === pending.width && previous.height === pending.height ? previous : pending));
    };

    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry === undefined) return;
      const box = entry.contentRect;
      pending = { width: Math.max(0, Math.floor(box.width)), height: Math.max(0, Math.floor(box.height)) };
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(flush, settleMs);
    });

    observer.observe(element);
    const initial = element.getBoundingClientRect();
    pending = { width: Math.max(0, Math.floor(initial.width)), height: Math.max(0, Math.floor(initial.height)) };
    flush();

    return () => {
      if (timer !== null) clearTimeout(timer);
      observer.disconnect();
    };
  }, [ref, settleMs]);

  return size;
}

/** Current backing-store scale, tracked across monitor moves. */
export function useDevicePixelRatio(): number {
  const [ratio, setRatio] = useState(() => (typeof window === "undefined" ? 1 : window.devicePixelRatio || 1));

  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    let query: MediaQueryList | null = null;
    let cancelled = false;

    const subscribe = (): void => {
      if (cancelled) return;
      const current = window.devicePixelRatio || 1;
      setRatio(current);
      query?.removeEventListener("change", subscribe);
      query = window.matchMedia(`(resolution: ${current}dppx)`);
      query.addEventListener("change", subscribe);
    };

    subscribe();
    return () => {
      cancelled = true;
      query?.removeEventListener("change", subscribe);
    };
  }, []);

  return ratio;
}

/** `prefers-reduced-motion: reduce`. */
export function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  });

  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const query = window.matchMedia("(prefers-reduced-motion: reduce)");
    const onChange = (event: MediaQueryListEvent): void => setReduced(event.matches);
    query.addEventListener("change", onChange);
    setReduced(query.matches);
    return () => query.removeEventListener("change", onChange);
  }, []);

  return reduced;
}

/**
 * Resolve the category palette, and re-resolve when the theme changes. The
 * theme moves either because macOS switched appearance or because the shell put
 * `.light` / `.dark` on `<html>`, so both are watched.
 */
export function usePalette(colorBy: ColorBy = "category"): Palette {
  const [palette, setPalette] = useState<Palette>(() => resolvePalette(null, colorBy));

  useEffect(() => {
    if (typeof document === "undefined") return;
    const refresh = (): void => setPalette(resolvePalette(null, colorBy));

    const observer = new MutationObserver(refresh);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class", "style", "data-theme"] });

    let query: MediaQueryList | null = null;
    if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
      query = window.matchMedia("(prefers-color-scheme: dark)");
      query.addEventListener("change", refresh);
    }

    refresh();
    return () => {
      observer.disconnect();
      query?.removeEventListener("change", refresh);
    };
    // `colorBy` belongs in the deps: switching encoding has to rebuild the
    // 256-entry fill table, exactly as a theme change does.
  }, [colorBy]);

  return palette;
}
