/* =============================================================================
 * Synthetic hierarchy + reference layouts, for the dev preview and for tests.
 *
 * `rdirstat-treemap` owns the production layouts; this is the TypeScript
 * reference that lets the renderer be exercised without a backend, and it
 * doubles as an executable description of what the Rust side must emit into the
 * pinned `node, depth, x, y, w, h, category` schema:
 *
 *   treemap  -> (x, y, w, h) in CSS pixels, children nested inside their parent
 *   icicle   -> x/w proportional to size, y = depth * row height, h = row height
 *   sunburst -> (start_angle, inner_radius, sweep, thickness), angles in radians
 *
 * Nothing in the shipped app imports this outside the preview page, so it is
 * tree-shaken out of the production bundle.
 * ========================================================================== */

import type { LayoutRow } from "./fixtures.ts";

export interface FakeNode {
  readonly node: number;
  readonly size: number;
  readonly category: number;
  readonly children: FakeNode[];
}

/** Deterministic 32-bit LCG. A fixture that changes every reload is not a fixture. */
function lcg(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
    return state / 0x1_0000_0000;
  };
}

/**
 * A tree whose sizes are heavy-tailed, like a real disk: a few enormous
 * directories and a long tail of small ones.
 */
export function buildFakeTree(seed = 12_345, breadth = 12, depth = 3): FakeNode {
  const random = lcg(seed);
  let nextId = 0;

  const build = (level: number): FakeNode => {
    const node = nextId++;
    const category = Math.floor(random() * 19);
    if (level >= depth) {
      // Heavy tail: most leaves are small, a few are gigantic.
      const magnitude = random() ** 4;
      return { node, size: Math.max(1, Math.round(magnitude * 1_000_000)), category, children: [] };
    }
    const count = 2 + Math.floor(random() * breadth);
    const children: FakeNode[] = [];
    for (let i = 0; i < count; i += 1) children.push(build(level + 1));
    const size = children.reduce((sum, child) => sum + child.size, 0);
    return { node, size, category, children };
  };

  return build(0);
}

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

function worstAspect(row: readonly number[], length: number, scale: number): number {
  if (row.length === 0 || length <= 0) return Number.POSITIVE_INFINITY;
  let sum = 0;
  let min = Number.POSITIVE_INFINITY;
  let max = 0;
  for (const value of row) {
    const area = value * scale;
    sum += area;
    if (area < min) min = area;
    if (area > max) max = area;
  }
  if (sum <= 0) return Number.POSITIVE_INFINITY;
  const side = length * length;
  return Math.max((side * max) / (sum * sum), (sum * sum) / (side * min));
}

/**
 * Squarified treemap (Bruls, Huizing & van Wijk 2000), the algorithm docs/05
 * pins for the treemap view. Children are laid out inside `rect`; the caller
 * recurses.
 */
function squarify(sizes: readonly number[], rect: Rect): Rect[] {
  const out: Rect[] = [];
  const total = sizes.reduce((sum, value) => sum + value, 0);
  if (total <= 0 || rect.w <= 0 || rect.h <= 0) return sizes.map(() => ({ x: rect.x, y: rect.y, w: 0, h: 0 }));

  let area = rect.w * rect.h;
  let scale = area / total;
  let remaining = { ...rect };
  let index = 0;

  while (index < sizes.length) {
    const shortSide = Math.min(remaining.w, remaining.h);
    const row: number[] = [];
    let bestWorst = Number.POSITIVE_INFINITY;

    while (index + row.length < sizes.length) {
      const candidate = [...row, sizes[index + row.length] ?? 0];
      const worst = worstAspect(candidate, shortSide, scale);
      if (row.length > 0 && worst > bestWorst) break;
      bestWorst = worst;
      row.push(sizes[index + row.length] ?? 0);
    }

    const rowSum = row.reduce((sum, value) => sum + value, 0);
    const rowArea = rowSum * scale;
    if (remaining.w >= remaining.h) {
      const columnWidth = shortSide > 0 ? rowArea / remaining.h : 0;
      let offset = remaining.y;
      for (const value of row) {
        const height = rowSum > 0 ? (value / rowSum) * remaining.h : 0;
        out.push({ x: remaining.x, y: offset, w: columnWidth, h: height });
        offset += height;
      }
      remaining = { x: remaining.x + columnWidth, y: remaining.y, w: remaining.w - columnWidth, h: remaining.h };
    } else {
      const rowHeight = shortSide > 0 ? rowArea / remaining.w : 0;
      let offset = remaining.x;
      for (const value of row) {
        const width = rowSum > 0 ? (value / rowSum) * remaining.w : 0;
        out.push({ x: offset, y: remaining.y, w: width, h: rowHeight });
        offset += width;
      }
      remaining = { x: remaining.x, y: remaining.y + rowHeight, w: remaining.w, h: remaining.h - rowHeight };
    }

    index += row.length;
    area = remaining.w * remaining.h;
    const rest = sizes.slice(index).reduce((sum, value) => sum + value, 0);
    scale = rest > 0 ? area / rest : 0;
  }

  return out;
}

/** Squarified treemap rows, parents first so the reverse hit-scan finds children. */
export function treemapRows(root: FakeNode, width: number, height: number, minPx: number): LayoutRow[] {
  const out: LayoutRow[] = [];

  const emit = (node: FakeNode, rect: Rect, depth: number): void => {
    if (rect.w < minPx || rect.h < minPx) return;
    out.push({ node: node.node, depth, x: rect.x, y: rect.y, w: rect.w, h: rect.h, category: node.category });
    if (node.children.length === 0) return;
    const inset = depth === 0 ? 0 : 1;
    const inner: Rect = {
      x: rect.x + inset,
      y: rect.y + inset,
      w: Math.max(0, rect.w - inset * 2),
      h: Math.max(0, rect.h - inset * 2),
    };
    const ordered = [...node.children].sort((a, b) => b.size - a.size);
    const rects = squarify(
      ordered.map((child) => child.size),
      inner,
    );
    ordered.forEach((child, position) => {
      const childRect = rects[position];
      if (childRect !== undefined) emit(child, childRect, depth + 1);
    });
  };

  emit(root, { x: 0, y: 0, w: width, h: height }, 0);
  return out;
}

/** Icicle rows: depth on the y-axis, size on the x-axis. */
export function icicleRows(root: FakeNode, width: number, height: number, maxDepth: number, minPx: number): LayoutRow[] {
  const out: LayoutRow[] = [];
  const rowHeight = height / Math.max(1, maxDepth + 1);

  const emit = (node: FakeNode, x: number, w: number, depth: number): void => {
    if (w < minPx) return;
    out.push({ node: node.node, depth, x, y: depth * rowHeight, w, h: rowHeight, category: node.category });
    let offset = x;
    for (const child of [...node.children].sort((a, b) => b.size - a.size)) {
      const childWidth = node.size > 0 ? (child.size / node.size) * w : 0;
      emit(child, offset, childWidth, depth + 1);
      offset += childWidth;
    }
  };

  emit(root, 0, width, 0);
  return out;
}

/** Sunburst rows: (start_angle, inner_radius, sweep, thickness), radians + units. */
export function sunburstRows(root: FakeNode, maxDepth: number, outerRadius: number, minSweep: number): LayoutRow[] {
  const out: LayoutRow[] = [];
  const ring = outerRadius / Math.max(1, maxDepth + 1);

  const emit = (node: FakeNode, start: number, sweep: number, depth: number): void => {
    if (sweep < minSweep) return;
    out.push({ node: node.node, depth, x: start, y: depth * ring, w: sweep, h: ring, category: node.category });
    let offset = start;
    for (const child of [...node.children].sort((a, b) => b.size - a.size)) {
      const childSweep = node.size > 0 ? (child.size / node.size) * sweep : 0;
      emit(child, offset, childSweep, depth + 1);
      offset += childSweep;
    }
  };

  emit(root, 0, Math.PI * 2, 0);
  return out;
}

export function treeDepth(node: FakeNode): number {
  if (node.children.length === 0) return 0;
  return 1 + Math.max(...node.children.map(treeDepth));
}

/** Flatten the tree so a preview can answer `node_details`-shaped questions. */
export function indexTree(root: FakeNode): Map<number, { node: FakeNode; path: string }> {
  const index = new Map<number, { node: FakeNode; path: string }>();
  const walk = (node: FakeNode, path: string): void => {
    index.set(node.node, { node, path });
    for (const child of node.children) walk(child, `${path}/item-${child.node}`);
  };
  walk(root, "/fixture");
  return index;
}
