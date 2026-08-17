/* =============================================================================
 * Category index -> fill colour.
 *
 * docs/05-UI.md#color-and-formatting: "have Rust send *category indices* while
 * the frontend resolves colours. That keeps theming in CSS and out of the Rust
 * palette table." So the resolution order is:
 *
 *   1. `--cat-<key>` read off the document element, if the theme defines it;
 *   2. the built-in fallback table below;
 *   3. a deterministic golden-angle hue for an index the taxonomy has not
 *      claimed yet — a category the frontend has never heard of must still be
 *      visible and stable across repaints, never invisible and never random.
 *
 * The built-in table is a PLACEHOLDER pending the phase-3 palette work
 * (automated contrast test + colour-vision snapshots). It follows docs/04's
 * semantic grouping: waste and build output read warm, media reads cool, and
 * Uncompressed Image is as red as junk.
 *
 * Resolution happens once per theme change and produces a 256-entry string
 * table, so the paint loop only ever does an array index.
 * ========================================================================== */

import { categoryOf } from "../../lib/categories.ts";

/*
 * The index -> key -> label table is `src/lib/categories.ts`, which docs/05
 * makes the single place in the app that knows what a `CategoryId` means. It is
 * imported relatively rather than through `@/` so this module stays loadable by
 * `node --test`, which has no path mapping.
 */

/**
 * Fallback hexes, indexed by `CategoryId`, used only when the theme does not
 * define `--cat-<key>` (a bare test harness, or CSS that failed to load). The
 * shipped colours are `src/styles/categories.css`'s.
 */
const FALLBACK_COLORS: readonly string[] = [
  "#6b7280", // uncategorized — neutral gray
  "#94a3b8", // symlink — light slate, deliberately low chroma
  "#a78bfa", // executable — violet, the app accent family
  "#64748b", // apple-metadata — slate
  "#f59e0b", // compressed-archive — amber
  "#ea580c", // uncompressed-archive — orange, warmer because it is waste
  "#fbbf24", // compressed-stream — light amber
  "#fb923c", // disk-image / installer — orange
  "#22d3ee", // image — cyan
  "#06b6d4", // raw-photo — deeper cyan
  "#ef4444", // uncompressed-image — red, "as red as Junk" per docs/05
  "#3b82f6", // video — blue
  "#14b8a6", // audio — teal
  "#cbd5e1", // document — pale slate
  "#34d399", // source — emerald
  "#d97706", // object-generated — dark amber, build output is waste
  "#10b981", // library — emerald
  "#f472b6", // vm-disk — pink
  "#e879f9", // container-disk — fuchsia
];

/** Maximum `CategoryId` count (`rdirstat_core::MAX_CATEGORIES`). */
const MAX_CATEGORIES = 256;

const GOLDEN_ANGLE_DEGREES = 137.508;

export interface Palette {
  /** 256 CSS colour strings, indexed by `CategoryId`. */
  readonly fills: readonly string[];
  /** Hairline between sibling tiles. */
  readonly border: string;
  /** Outline of a selected tile. */
  readonly selection: string;
  /** Outline of the hovered tile. */
  readonly hover: string;
  /** Canvas clear colour. */
  readonly background: string;
  /** Token generation, so a repaint can be keyed on it. */
  readonly revision: number;
}

export function categoryLabel(index: number): string {
  return categoryOf(index).label;
}

export function categoryKey(index: number): string {
  return categoryOf(index).key;
}

function generatedColor(index: number): string {
  const hue = (index * GOLDEN_ANGLE_DEGREES) % 360;
  return `hsl(${hue.toFixed(1)} 55% 58%)`;
}

/**
 * A colour string that `CanvasRenderingContext2D.fillStyle` will actually
 * accept. Theme tokens are authored in `oklch()`; WebKit parses that in canvas,
 * but a token could hold anything, and an unparseable value silently leaves the
 * PREVIOUS fill in place — which paints a tile the wrong category's colour. The
 * probe below turns that into a fallback instead.
 */
function makeColorValidator(): (candidate: string) => boolean {
  if (typeof document === "undefined") return () => true;
  const probe = document.createElement("canvas").getContext("2d");
  if (probe === null) return () => true;
  return (candidate: string): boolean => {
    // Two sentinels: if the candidate is unparseable, fillStyle keeps the
    // sentinel, and no single sentinel can be equal to a valid candidate twice.
    probe.fillStyle = "#000000";
    probe.fillStyle = candidate;
    const first = probe.fillStyle;
    probe.fillStyle = "#ffffff";
    probe.fillStyle = candidate;
    return probe.fillStyle === first;
  };
}

function readVar(styles: CSSStyleDeclaration | null, name: string): string | null {
  if (styles === null) return null;
  const value = styles.getPropertyValue(name).trim();
  return value.length > 0 ? value : null;
}

let revisionCounter = 0;

/**
 * Build the palette for the current theme. Call on mount and whenever the theme
 * changes — not per frame.
 */
export function resolvePalette(element?: Element | null): Palette {
  const host = element ?? (typeof document === "undefined" ? null : document.documentElement);
  const styles = host !== null && typeof getComputedStyle === "function" ? getComputedStyle(host) : null;
  const isValid = makeColorValidator();

  const fills: string[] = new Array<string>(MAX_CATEGORIES);
  for (let index = 0; index < MAX_CATEGORIES; index += 1) {
    // `categoryOf` maps an index the taxonomy has not claimed to `unknown`,
    // which the theme colours distinctly, so a version skew is visible rather
    // than silently painting VM disks in the audio colour.
    const themed = readVar(styles, `--cat-${categoryOf(index).key}`);
    const fallback = FALLBACK_COLORS.at(index) ?? generatedColor(index);
    fills[index] = themed !== null && isValid(themed) ? themed : fallback;
  }

  const border = readVar(styles, "--cat-border") ?? "rgba(0, 0, 0, 0.35)";
  const selection = readVar(styles, "--cat-selection") ?? readVar(styles, "--ring") ?? "#ffffff";
  const hover = readVar(styles, "--cat-hover") ?? "rgba(255, 255, 255, 0.85)";
  const background = readVar(styles, "--background") ?? "#0a0a0a";

  revisionCounter += 1;
  return {
    fills,
    border: isValid(border) ? border : "rgba(0, 0, 0, 0.35)",
    selection: isValid(selection) ? selection : "#ffffff",
    hover: isValid(hover) ? hover : "rgba(255, 255, 255, 0.85)",
    background: isValid(background) ? background : "#0a0a0a",
    revision: revisionCounter,
  };
}
