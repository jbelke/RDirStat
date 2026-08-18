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

import { CATEGORIES, FAMILIES, categoryOf, familyKey } from "../../lib/categories.ts";

/**
 * What a tile's colour encodes.
 *
 * Twenty-five categories is past what a treemap can express. Once tiles are a
 * few pixels wide, adjacent hues stop being tellable apart and the colour
 * carries no information — it is just texture. `family` collapses the taxonomy
 * onto docs/04's five headings, which stay distinguishable at tile size and
 * survive a colour-vision deficiency; `category` keeps the full palette, which
 * is the useful one once a drill-down has cut the categories on screen down to
 * a handful.
 */
export type ColorBy = "category" | "family";

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
  // macOS additions, ids 19..24. Present in the compiled Rust table from the
  // start; absent here until now, which is why a `node_modules` tile painted
  // the golden-angle "I have never heard of this index" colour.
  "#8e6fd0", // package — violet, the bundle family
  "#2e8fa8", // media-library — deep cyan, media reads cool
  "#c85a5a", // build-junk — dull red, build output is waste
  "#d6714e", // cache — burnt orange, the most reclaimable thing on the disk
  "#8e7cc3", // font — muted violet
  "#7ca23c", // database — olive
];

/** Maximum `CategoryId` count (`rdirstat_core::MAX_CATEGORIES`). */
const MAX_CATEGORIES = 256;

const GOLDEN_ANGLE_DEGREES = 137.508;

/** Fallback family fills, used when the theme defines no `--fam-<key>`. */
const FALLBACK_FAMILY_COLORS: Readonly<Record<string, string>> = {
  system: "#6b7280",
  archives: "#d0a03e",
  media: "#3e8ad1",
  "documents-and-code": "#59bf7a",
  "large-runtime-data": "#ce5b57",
};

export interface Palette {
  /**
   * 256 CSS colour strings, indexed by `CategoryId`.
   *
   * Which encoding this holds depends on the `colorBy` the palette was
   * resolved with: under `"family"` every category in a family maps to the
   * same string, so the paint loop stays a single array index either way and
   * needs to know nothing about the mode.
   */
  readonly fills: readonly string[];
  /** What `fills` encodes, so the legend can label itself honestly. */
  readonly colorBy: ColorBy;
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

export interface LegendEntry {
  /** A `CategoryId` under `"category"`; a family name under `"family"`. */
  readonly id: string;
  readonly label: string;
  /** The CSS `var()` reference, so the swatch follows the theme with no JS. */
  readonly colorVar: string;
  /**
   * Every `CategoryId` this row stands for — one under `"category"`, a whole
   * family's worth under `"family"`.
   *
   * This is what makes a legend row usable as a *filter*. The backend has no
   * concept of a family and should not gain one: the grouping is a
   * presentation choice that can change without the arena changing. So the
   * expansion happens here, and everything downstream only ever sees category
   * ids.
   */
  readonly categoryIds: readonly number[];
}

/**
 * The distinct `CategoryId`s a layout actually drew.
 *
 * This is what [`legendEntries`] should be given as `present`: the legend
 * lists what is on screen rather than the whole taxonomy, and `batch.category`
 * is the only honest source for that — it is the array the renderer coloured
 * from, so the legend cannot claim a category the canvas did not paint.
 *
 * Takes the column and a count rather than the batch, because `count` is
 * authoritative and the typed array may be longer than the rows in use.
 *
 * A caller must not pass a *filtered* layout: it contains only the categories
 * already selected, so latching it would drop every other row from the legend
 * — including the ones needed to add a second category to the filter.
 */
export function drawnCategories(category: Uint8Array, count: number): ReadonlySet<number> {
  const drawn = new Set<number>();
  const limit = Math.min(count, category.length);
  for (let index = 0; index < limit; index += 1) {
    const id = category[index];
    if (id !== undefined) drawn.add(id);
  }
  return drawn;
}

/**
 * The rows a legend should show for a palette.
 *
 * Only categories actually present in the current view are worth listing — a
 * legend enumerating all twenty-five is a wall of names, most of which are not
 * on screen. `present` is the set of `CategoryId`s in the layout batch; pass
 * `null` to list everything (the settings/help case).
 */
export function legendEntries(colorBy: ColorBy, present: ReadonlySet<number> | null): LegendEntry[] {
  if (colorBy === "family") {
    const families = new Set(
      present === null
        ? FAMILIES
        : [...present].map((index) => categoryOf(index).family).filter((family) => family !== undefined),
    );
    return FAMILIES.filter((family) => families.has(family)).map((family) => ({
      id: family,
      label: family,
      colorVar: `var(--fam-${familyKey(family)})`,
      categoryIds: CATEGORIES.filter((category) => category.family === family).map(
        (category) => category.id,
      ),
    }));
  }

  return CATEGORIES.filter((category) => present === null || present.has(category.id)).map((category) => ({
    id: String(category.id),
    label: category.label,
    colorVar: `var(--cat-${category.key})`,
    categoryIds: [category.id],
  }));
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

/** How much of a filtered-out category's own colour survives. */
const DIM_PERCENT = 16;

/**
 * The colour a filtered-out category paints in.
 *
 * Mixed toward the canvas background rather than made translucent: tiles
 * overlap their parents, so alpha would compound with depth and the deepest
 * tiles would come out darkest for no reason the user could explain. A mix
 * against the background is flat regardless of nesting.
 *
 * Enough of the hue survives (16%) that the shape of the excluded data is
 * still legible — the point is to push it back, not to erase it, because the
 * excluded tiles are the context that makes the included ones mean anything.
 *
 * `color-mix` is validated like every other token: WebKit has supported it for
 * years, but an unparseable `fillStyle` silently leaves the PREVIOUS fill in
 * place, which would paint a tile a different category's colour. The fallback
 * is a flat muted grey, which is wrong but visibly wrong.
 */
function dim(color: string, background: string, isValid: (candidate: string) => boolean): string {
  const mixed = `color-mix(in oklab, ${color} ${DIM_PERCENT}%, ${background})`;
  return isValid(mixed) ? mixed : "#3a3a3a";
}

let revisionCounter = 0;

/**
 * Build the palette for the current theme. Call on mount and whenever the theme
 * changes — not per frame.
 */
export function resolvePalette(
  element?: Element | null,
  colorBy: ColorBy = "category",
  filter: ReadonlySet<number> | null = null,
): Palette {
  const host = element ?? (typeof document === "undefined" ? null : document.documentElement);
  const styles = host !== null && typeof getComputedStyle === "function" ? getComputedStyle(host) : null;
  const isValid = makeColorValidator();

  const background = readVar(styles, "--background") ?? "#0a0a0a";
  const backgroundOk = isValid(background) ? background : "#0a0a0a";

  const fills: string[] = new Array<string>(MAX_CATEGORIES);
  for (let index = 0; index < MAX_CATEGORIES; index += 1) {
    // `categoryOf` maps an index the taxonomy has not claimed to `unknown`,
    // which the theme colours distinctly, so a version skew is visible rather
    // than silently painting VM disks in the audio colour.
    const category = categoryOf(index);
    const token = colorBy === "family" ? `--fam-${familyKey(category.family)}` : `--cat-${category.key}`;
    const themed = readVar(styles, token);
    const fallback =
      colorBy === "family"
        ? (FALLBACK_FAMILY_COLORS[familyKey(category.family)] ?? FALLBACK_COLORS[0] ?? "#6b7280")
        : (FALLBACK_COLORS.at(index) ?? generatedColor(index));
    const full = themed !== null && isValid(themed) ? themed : fallback;

    // The filter is resolved INTO the fill table rather than handed to the
    // paint loop. Two reasons: the loop stays a single array index with no
    // branch per tile, and `render.ts` needs no knowledge of filtering at all.
    // Re-resolving 256 strings when the filter changes is free; a per-tile
    // membership test on a million tiles is not.
    fills[index] = filter === null || filter.has(index) ? full : dim(full, backgroundOk, isValid);
  }

  const border = readVar(styles, "--cat-border") ?? "rgba(0, 0, 0, 0.35)";
  const selection = readVar(styles, "--cat-selection") ?? readVar(styles, "--ring") ?? "#ffffff";
  const hover = readVar(styles, "--cat-hover") ?? "rgba(255, 255, 255, 0.85)";

  revisionCounter += 1;
  return {
    fills,
    colorBy,
    border: isValid(border) ? border : "rgba(0, 0, 0, 0.35)",
    selection: isValid(selection) ? selection : "#ffffff",
    hover: isValid(hover) ? hover : "rgba(255, 255, 255, 0.85)",
    background: backgroundOk,
    revision: revisionCounter,
  };
}
