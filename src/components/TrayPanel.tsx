/**
 * The menu-bar mini panel.
 *
 * docs/05-UI.md, "Menu bar": "the question this app answers is a *standing* one
 * — a disk at 97% is a condition, not an event — so watching it should not cost
 * a window."
 *
 * This is the whole of that surface, and its constraints are what make it small
 * rather than a second copy of the app:
 *
 * - **It is a viewer, never an actor.** No Trash, no arming switch, no
 *   relocation, nothing that changes a byte on disk. The only thing it can
 *   start is a rescan of a root the user already chose, and even that is a
 *   button that names it. A surface that appears under the cursor at the top of
 *   the screen is where a mis-click costs the most.
 * - **It does not scan to fill itself.** Volume capacity comes from `statfs`
 *   through the ordinary `volumes` command, and the scan figures come from the
 *   published summary. Nothing here walks a filesystem.
 * - **It polls slowly.** Sixty seconds, because free space is a condition and
 *   not a stream. The 10 Hz progress event is deliberately not subscribed here:
 *   a menu-bar popover that re-renders ten times a second while hidden is a
 *   battery bug.
 * - **It says nothing about the build.** An earlier version carried a "Catalog"
 *   section explaining that Parquet/DuckDB reporting was a later phase. A
 *   menu-bar popover is the smallest surface in the app and it was spending a
 *   third of it on a feature that does not exist. Roadmap belongs in the repo,
 *   not in a panel someone opens to check free space.
 */

import { HardDrive, Loader, PanelTopOpen, RefreshCw, Settings } from "lucide-react";
import { useEffect, useMemo } from "react";

import { CapacityBar, capacitySegments } from "@/components/CapacityBar";
import { Button } from "@/components/ui/button";
import { groupVolumes } from "@/lib/disks";
import type { ContainerGroup } from "@/lib/disks";
import { formatCount, formatPercent, formatSI } from "@/lib/format";
import { useScanStatus, useVolumes } from "@/lib/queries";
import { OPEN_SETTINGS_EVENT } from "@/lib/ipc";
import { cn } from "@/lib/utils";

/** How often the panel re-reads volumes, in milliseconds. */
const REFRESH_MS = 60_000;

export function TrayPanel() {
  const { data: volumes, refetch, isFetching } = useVolumes();
  const status = useScanStatus();

  useEffect(() => {
    const timer = window.setInterval(() => void refetch(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refetch]);

  const disks = useMemo(() => groupVolumes(volumes ?? []), [volumes]);
  const summary = status.data?.summary ?? null;
  const scanState = status.data?.state ?? "idle";

  return (
    <div className="flex h-full flex-col bg-background text-foreground">
      <header
        // The panel has no title bar, so this row is also the drag handle.
        data-tauri-drag-region
        className="flex shrink-0 items-center gap-2 border-b border-border/60 px-3 py-2"
      >
        <span className="flex-1 text-xs font-medium">Stellar RDIRSTAT</span>
        <button
          type="button"
          onClick={() => void refetch()}
          title="Re-read volumes now"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <RefreshCw aria-hidden className={cn("size-3.5", isFetching && "animate-spin")} />
        </button>
        <button
          type="button"
          onClick={() => void openSettings()}
          title="Settings"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <Settings aria-hidden className="size-3.5" />
          <span className="sr-only">Settings</span>
        </button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        <h2 className="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Disks</h2>
        {volumes === undefined && (
          <p className="flex items-center gap-2 py-3 text-xs text-muted-foreground">
            <Loader aria-hidden className="size-3.5 animate-spin" />
            Reading volumes…
          </p>
        )}
        <ul className="flex flex-col gap-2.5">
          {disks.map((disk) => (
            // One border per physical disk, volumes inside it. The panel used
            // to draw one flat row per container, so a multi-container disk
            // appeared as several disks that happened to share a name.
            <li key={disk.id} className="overflow-hidden rounded-lg border border-border/60">
              <div className="flex items-baseline gap-1.5 bg-muted/40 px-2.5 py-1.5">
                <HardDrive
                  aria-hidden
                  className="size-3 shrink-0 translate-y-px text-muted-foreground"
                />
                <span className="min-w-0 flex-1 truncate text-xs font-medium">{disk.label}</span>
                {disk.sizeBytes !== null && (
                  <span className="rds-numeric shrink-0 text-[10px] text-muted-foreground">
                    {formatSI(disk.sizeBytes)}
                  </span>
                )}
              </div>
              <div className="flex flex-col divide-y divide-border/40 border-t border-border/60">
                {disk.containers.map((container) => {
                  const segments = capacitySegments(
                    container.totalBytes,
                    container.availableBytes,
                    container.importantAvailableBytes,
                  );
                  return (
                    <div key={container.id} className="flex flex-col gap-1 px-2.5 py-2">
                      <div className="flex items-baseline gap-2 text-xs">
                        <span className="min-w-0 flex-1 truncate">{containerTitle(container)}</span>
                        <span className="rds-numeric shrink-0 tabular-nums">
                          {formatPercent(segments.used, segments.total)}
                        </span>
                        <span className="rds-numeric shrink-0 text-muted-foreground">
                          {formatSI(segments.available)} free
                        </span>
                      </div>
                      <CapacityBar
                        total={container.totalBytes}
                        available={container.availableBytes}
                        importantAvailable={container.importantAvailableBytes}
                      />
                    </div>
                  );
                })}
              </div>
            </li>
          ))}
        </ul>

        <h2 className="mb-1 mt-4 text-[10px] uppercase tracking-wide text-muted-foreground">
          Last scan
        </h2>
        {summary === null ? (
          <p className="text-xs text-muted-foreground">
            Nothing scanned yet in this session.
          </p>
        ) : (
          <dl className="flex flex-col gap-1 text-xs">
            <Row label="Root">
              <span className="break-all font-mono text-[11px]">{summary.rootPath}</span>
            </Row>
            <Row label="Logical">{formatSI(summary.totals.logical)}</Row>
            <Row label="Allocated">{formatSI(summary.totals.allocated)}</Row>
            <Row label="Entries">{formatCount(summary.counts.observedEntries)}</Row>
            {summary.counts.unreadableDirs > 0 && (
              <Row label="Unreadable">
                <span className="text-pressure-warn">
                  {formatCount(summary.counts.unreadableDirs)} folders
                </span>
              </Row>
            )}
            {summary.partial && (
              <p className="text-[10px] text-muted-foreground">
                Something made these totals a floor rather than a measurement — the main window says
                what.
              </p>
            )}
          </dl>
        )}

        {scanState === "scanning" && (
          <p className="mt-2 flex items-center gap-2 text-xs text-brand">
            <Loader aria-hidden className="size-3.5 animate-spin" />
            A scan is running. Open the window to watch it.
          </p>
        )}

      </div>

      <footer className="flex shrink-0 items-center gap-2 border-t border-border/60 px-3 py-2">
        <Button size="sm" variant="outline" className="flex-1" onClick={() => void openMainWindow()}>
          <PanelTopOpen aria-hidden />
          Open Stellar RDIRSTAT
        </Button>
      </footer>
    </div>
  );
}

/**
 * Brings the main window forward from the panel.
 *
 * Deliberately not a command: showing a window is a window-manager operation
 * that the webview API already owns, and adding an IPC command for it would put
 * a second, weaker copy of `tray::show_main_window` in the contract.
 */
async function openMainWindow(): Promise<void> {
  const { WebviewWindow } = await import("@tauri-apps/api/webviewWindow");
  const main = await WebviewWindow.getByLabel("main");
  if (main === null) return;
  await main.show();
  await main.unminimize();
  await main.setFocus();
}

/**
 * Shows the main window on its settings route.
 *
 * Two steps rather than one because they answer different questions: the window
 * has to exist and be frontmost before a route change is worth anything, and the
 * route itself belongs to the shell. Emitting after showing also means a missed
 * event degrades to "the window opened where you left it" rather than to a
 * window that never appeared.
 */
async function openSettings(): Promise<void> {
  await openMainWindow();
  const { emitTo } = await import("@tauri-apps/api/event");
  await emitTo("main", OPEN_SETTINGS_EVENT);
}

/**
 * What one capacity bar covers, named by its volumes.
 *
 * Capacity is a container property (see `@/lib/disks`), so the bar is per
 * container and this line says whose data shares it. User volumes are worth
 * their names; the macOS-owned ones (`Preboot`, `VM`, `Update`, …) are not —
 * in a four-inch panel they are counted, not listed.
 */
function containerTitle(container: ContainerGroup): string {
  const user = container.volumes.filter((volume) => !volume.isSystem).map((volume) => volume.name);
  const system = container.volumes.length - user.length;
  if (system === 0) return user.join(" · ");
  return [...user, `${system} system volume${system === 1 ? "" : "s"}`].join(" · ");
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="shrink-0 text-muted-foreground">{label}</dt>
      <dd className="min-w-0 truncate text-right">{children}</dd>
    </div>
  );
}
