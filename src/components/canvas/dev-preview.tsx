/* =============================================================================
 * Dev-only preview page for the hierarchy canvas.
 *
 * Served by Vite at /src/components/canvas/dev-preview.html during `pnpm dev`.
 * It is NOT an entry in the production build (only index.html is), so it costs
 * the shipped bundle nothing.
 *
 * It exists so this component can be exercised end to end — Arrow encode ->
 * decode -> paint -> hover -> select -> zoom -> toggle — before `src-tauri` and
 * `rdirstat-treemap` are finished. When they are, the same page is the fastest
 * way to tell a renderer bug apart from a layout bug.
 * ========================================================================== */

import { StrictMode, useCallback, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";

import "@/index.css";

import { HierarchyCanvas } from "./HierarchyCanvas.tsx";
import { buildLayoutIpc } from "./fixtures.ts";
import { buildFakeTree, icicleRows, indexTree, sunburstRows, treeDepth, treemapRows } from "./devFixtures.ts";
import { formatSi } from "./format.ts";
import type { LayoutFetcher, NodeDescriber, PaintReport, SelectionChange } from "./index.ts";

const GENERATION = "1";

function Preview() {
  const tree = useMemo(() => buildFakeTree(20_260_817, 10, 3), []);
  const index = useMemo(() => indexTree(tree), [tree]);
  const depth = useMemo(() => treeDepth(tree), [tree]);

  const [selection, setSelection] = useState<readonly number[]>([]);
  const [paint, setPaint] = useState<PaintReport | null>(null);
  const [log, setLog] = useState<string[]>([]);

  const push = useCallback((line: string) => setLog((entries) => [line, ...entries].slice(0, 8)), []);

  const fetchLayout = useCallback<LayoutFetcher>(
    async (request) => {
      const { width, height } = request.viewport;
      const rows =
        request.kind === "treemap"
          ? treemapRows(tree, width, height, request.minPx)
          : request.kind === "icicle"
            ? icicleRows(tree, width, height, depth, request.minPx)
            : sunburstRows(tree, depth, Math.min(width, height) / 2, 0.004);
      return buildLayoutIpc(rows, { generation: GENERATION });
    },
    [depth, tree],
  );

  const describeNode = useCallback<NodeDescriber>(
    async (node) => {
      const entry = index.get(node);
      if (entry === undefined) return undefined;
      return {
        path: entry.path,
        name: `item-${node}`,
        logical: entry.node.size,
        allocated: Math.ceil(entry.node.size / 4096) * 4096,
      };
    },
    [index],
  );

  return (
    <div className="flex h-screen w-screen flex-col bg-background text-foreground">
      <header className="flex items-center justify-between border-b border-border px-3 py-2 text-xs">
        <strong>HierarchyCanvas preview</strong>
        <span className="tabular-nums text-muted-foreground">
          {paint === null
            ? "no paint yet"
            : `${paint.drawn} drawn / ${paint.culled} culled · paint ${paint.paintMs.toFixed(1)}ms · load ${
                paint.loadMs === null ? "?" : paint.loadMs.toFixed(1)
              }ms · ${paint.width}x${paint.height} @${paint.devicePixelRatio}x`}
        </span>
      </header>

      <div className="min-h-0 flex-1">
        <HierarchyCanvas
          generation={GENERATION}
          root={0}
          fetchLayout={fetchLayout}
          describeNode={describeNode}
          selectedNodes={selection}
          formatBytes={formatSi}
          onSelectionChange={(change: SelectionChange) => {
            setSelection(change.nodes);
            push(`select ${change.nodes.length} node(s) via ${change.source}`);
          }}
          onNavigate={(node, stack) => push(`navigate -> ${node} (depth ${stack.length})`)}
          onContextAction={(request) => push(`context ${request.action} on ${request.primary}`)}
          onPaint={setPaint}
        />
      </div>

      <footer className="max-h-28 overflow-y-auto border-t border-border px-3 py-1.5 font-mono text-[11px] text-muted-foreground">
        {log.length === 0 ? <div>hover, click, ⌘-click, double-click and right-click the canvas</div> : null}
        {log.map((line, position) => (
          <div key={`${position}-${line}`}>{line}</div>
        ))}
      </footer>
    </div>
  );
}

const host = document.getElementById("preview-root");
if (host === null) throw new Error("dev-preview.html is missing #preview-root");
createRoot(host).render(
  <StrictMode>
    <Preview />
  </StrictMode>,
);
