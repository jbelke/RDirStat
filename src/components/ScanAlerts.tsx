/**
 * Completion alerts, as chips.
 *
 * docs/05-UI.md: "On completion, alerts surface unreadable directories,
 * exclusions, mutations, aggregation, and a material tree-versus-volume
 * discrepancy with the causes in `03`. **Permission guidance appears only when
 * the recorded error classes support it.**"
 *
 * That last sentence is why this reads `errorCounts` rather than
 * `counts.unreadable_dirs` alone. `ErrorClass::PermissionDenied` is the only
 * class that may trigger Full Disk Access guidance; a directory that failed with
 * `input_output` or `remote_unavailable` is a different problem, and telling the
 * user to open System Settings would be a wrong answer confidently delivered.
 *
 * ## Why chips rather than stacked banners
 *
 * These were full-width `Alert` blocks. Every one of them is *permanent* — a
 * scan of a boot volume always has unreadable directories and always has
 * exclusions — so the banners were not exceptional news, they were a fixed tax
 * of roughly a fifth of the window, sitting between the chart and the table for
 * the entire session. A caveat that is always on screen stops being read.
 *
 * So the headline (icon + count + noun) is always visible and costs one line,
 * and the explanation is one click away. Nothing is hidden that was not already
 * being skimmed past, and the qualification is still reachable at the moment the
 * user wonders about a number.
 *
 * These are still alerts, not dialogs: a per-path failure is recorded and the
 * scan continues — that is the scanner's documented policy, and a partial scan
 * is a success with a payload, never an error.
 */

import { CircleAlert, Info, TriangleAlert, X } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";

import { ScanErrorList } from "@/components/ScanErrorList";
import { formatCount } from "@/lib/format";
import type { ScanSummaryView } from "@/lib/ipc";
import { useScanErrors } from "@/lib/queries";
import { cn } from "@/lib/utils";

export interface ScanAlertsProps {
  summary: ScanSummaryView | null;
  className?: string;
}

interface Chip {
  id: string;
  tone: "warn" | "info";
  icon: ReactNode;
  /** The always-visible headline. Short enough to sit in a toolbar. */
  label: string;
  /** The explanation, revealed on click. */
  detail: ReactNode;
}

export function ScanAlerts({ summary, className }: ScanAlertsProps) {
  // Unconditional: this component returns null on several paths below, and the
  // hook order must not depend on which one.
  const [openId, setOpenId] = useState<string | null>(null);
  // Dismissed for THIS scan only, keyed by generation. A new tree is new news:
  // "370 unreadable" dismissed on Monday's scan must not silently suppress the
  // same warning about a different scan on Tuesday, because the caveat it
  // carries — every total above them is a floor — is about the numbers on
  // screen right now.
  const [dismissed, setDismissed] = useState<{ generation: number; ids: readonly string[] }>({
    generation: -1,
    ids: [],
  });
  // The failed-path list is only fetched once its own chip is open, which is the
  // same contract the old disclosure had.
  const errors = useScanErrors(openId === "unreadable" || openId === "errors");
  const container = useRef<HTMLDivElement | null>(null);

  // Escape closes, and so does a click anywhere else. Without both, a panel
  // pinned over the tree is something the user has to hunt for a way out of.
  useEffect(() => {
    if (openId === null) return undefined;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpenId(null);
    };
    const onPointer = (event: PointerEvent) => {
      if (!(event.target instanceof Node)) return;
      if (container.current?.contains(event.target) === true) return;
      setOpenId(null);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onPointer);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onPointer);
    };
  }, [openId]);

  if (summary === null) return null;

  const live = dismissed.generation === summary.generation ? dismissed.ids : [];
  const dismiss = (id: string) => {
    setDismissed({ generation: summary.generation, ids: [...live, id] });
    setOpenId((open) => (open === id ? null : open));
  };

  const permissionDenied = summary.errorCounts
    .filter((entry) => entry.errorClass === "permission_denied")
    .reduce((total, entry) => total + entry.count, 0);

  const otherErrors = summary.errorCounts
    .filter((entry) => entry.errorClass !== "permission_denied")
    .reduce((total, entry) => total + entry.count, 0);

  const chips: Chip[] = [];

  if (summary.counts.unreadableDirs > 0) {
    chips.push({
      id: "unreadable",
      tone: "warn",
      icon: <TriangleAlert aria-hidden className="size-3.5" />,
      label: `${formatCount(summary.counts.unreadableDirs)} unreadable`,
      detail: (
        <>
          <p>
            {formatCount(summary.counts.unreadableDirs)} director
            {summary.counts.unreadableDirs === 1 ? "y" : "ies"} could not be read. Their contents are
            not included, so every total above them is a floor rather than a measurement.
          </p>
          <p>
            {permissionDenied > 0 ? (
              <>
                {formatCount(permissionDenied)} of the recorded failures were permission denials.
                Granting Full Disk Access, or rescanning the specific folder through the folder
                picker (macOS&rsquo;s explicit-consent path), would let those be counted.
              </>
            ) : (
              <>
                None of the recorded failures were permission denials, so Full Disk Access would not
                change this result.
              </>
            )}
          </p>
          <ScanErrorList
            report={errors.data}
            isLoading={errors.isLoading}
            error={errors.error}
            className="rounded-md border border-border/60 p-2"
          />
        </>
      ),
    });
  }

  if (otherErrors > 0) {
    chips.push({
      id: "errors",
      tone: "warn",
      icon: <CircleAlert aria-hidden className="size-3.5" />,
      label: `${formatCount(otherErrors)} path errors`,
      detail: (
        <>
          <p>
            {summary.errorCounts
              .filter((entry) => entry.errorClass !== "permission_denied")
              .map((entry) => `${entry.errorClass.replace(/_/g, " ")} ×${formatCount(entry.count)}`)
              .join(", ")}
            . Each was recorded and the scan continued.
          </p>
          <ScanErrorList
            report={errors.data}
            isLoading={errors.isLoading}
            error={errors.error}
            className="rounded-md border border-border/60 p-2"
          />
        </>
      ),
    });
  }

  if (summary.mutations > 0) {
    chips.push({
      id: "mutations",
      tone: "info",
      icon: <Info aria-hidden className="size-3.5" />,
      label: `${formatCount(summary.mutations)} changed during scan`,
      detail: (
        <p>
          {formatCount(summary.mutations)} entries were created, removed, or resized while they were
          being walked. The tree is a consistent snapshot of what was observed, not of any single
          instant.
        </p>
      ),
    });
  }

  if (summary.excludedRoots.length > 0) {
    chips.push({
      id: "excluded",
      tone: "info",
      icon: <Info aria-hidden className="size-3.5" />,
      label: `${summary.excludedRoots.length} excluded`,
      detail: (
        <>
          <p>
            These paths matched an exclusion rule and were not opened. They are reported separately
            from unreadable paths because nothing failed — the scan was told to skip them.
          </p>
          <ul className="flex flex-col gap-0.5 break-all font-mono text-[11px] text-muted-foreground">
            {summary.excludedRoots.map((path) => (
              <li key={path}>{path}</li>
            ))}
          </ul>
        </>
      ),
    });
  }

  if (summary.aggregated) {
    chips.push({
      id: "aggregated",
      tone: "info",
      icon: <Info aria-hidden className="size-3.5" />,
      label: "aggregated",
      detail: (
        <p>
          {formatCount(summary.counts.aggregatedNodes)} entries below the aggregation threshold were
          folded into their parents to bound memory. Their bytes are counted; their names are not
          available.
        </p>
      ),
    });
  }

  const visible = chips.filter((chip) => !live.includes(chip.id));
  if (visible.length === 0) return null;

  return (
    <div ref={container} className={cn("relative flex shrink-0 items-center gap-1.5", className)}>
      {visible.map((chip) => (
        <div key={chip.id} className="group/chip relative">
          <button
            type="button"
            aria-expanded={openId === chip.id}
            onClick={() => setOpenId((open) => (open === chip.id ? null : chip.id))}
            className={cn(
              "flex items-center gap-1 whitespace-nowrap rounded-full border px-2 py-0.5 text-xs",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
              chip.tone === "warn"
                ? "border-pressure-warn/40 text-pressure-warn hover:bg-pressure-warn/10"
                : "border-border/60 text-muted-foreground hover:bg-accent hover:text-foreground",
              openId === chip.id && "bg-accent",
            )}
          >
            {chip.icon}
            <span>{chip.label}</span>
          </button>

          <button
            type="button"
            onClick={() => dismiss(chip.id)}
            title={`Dismiss "${chip.label}" for this scan`}
            className={cn(
              "absolute -right-1 -top-1 rounded-full border border-border bg-background p-0.5",
              "text-muted-foreground opacity-0 transition-opacity hover:text-foreground",
              "focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
              "group-hover/chip:opacity-100",
            )}
          >
            <X aria-hidden className="size-2.5" />
            <span className="sr-only">Dismiss {chip.label}</span>
          </button>

          {openId === chip.id && (
            // Opens over the content rather than pushing it: a panel that
            // resized the tree every time someone asked a question about a
            // caveat is what made the old banners intolerable.
            <div className="absolute right-0 top-full z-50 mt-1 flex w-[28rem] max-w-[80vw] flex-col gap-2 rounded-lg border border-border bg-popover p-3 text-xs shadow-xl">
              {chip.detail}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
