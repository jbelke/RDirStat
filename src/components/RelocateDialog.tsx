/**
 * Move a subtree to another volume and leave a symlink behind.
 *
 * This is the affordance the treemap creates demand for: the user finds a
 * 58 GB `Docker.raw` and the only thing the app could previously offer was
 * Trash. Often they want the bytes — just not on the boot volume.
 *
 * The dialog is built around one rule: **the plan is the product**. Every edit
 * re-plans on the backend, and what comes back — warnings, risk tier, free
 * space, whether a token was issued at all — is rendered whether or not the
 * relocation can proceed. A confirmation UI that only knows how to draw the
 * happy path shows nothing at the moment the user most needs an explanation,
 * so an unactionable plan renders its reasons and disables the button rather
 * than disappearing.
 *
 * Three gates stand between the user and an irreversible action:
 *
 * 1. `deletionArmed` — the app-wide destructive-action switch. Relocation ends
 *    by disposing of the original, so it lives behind the same switch as
 *    Trash rather than inventing a second policy.
 * 2. The plan's `token`, minted by the backend and bound to this generation
 *    and this object's `(dev, ino)`. `null` means "not actionable"; the button
 *    keys off the token, never off the call having succeeded.
 * 3. An explicit confirm, with the destination and the disposal both restated
 *    in the button itself.
 */

import { AlertTriangle, ArrowRight, Check, HardDrive, Loader2, Lock, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import {
  relocateApply,
  relocatePlan,
  type RelocateMode,
  type RelocatePlanView,
  type RelocateReportView,
  type SourceDisposal,
  type VolumeRow,
} from "@/lib/ipc";
import { useVolumes } from "@/lib/queries";
import { cn } from "@/lib/utils";

export interface RelocateDialogProps {
  generation: number;
  /** The node to move. `null` closes the dialog. */
  node: number | null;
  /** Its path, for the header, before any plan has come back. */
  sourcePath: string | null;
  /**
   * The scan root. Used only to work out which volume the source is *on*, by
   * longest-mount-point-prefix match, so that volume can be ranked last among
   * the destinations — moving to the disk you are trying to empty is the one
   * choice that cannot help.
   */
  scanRootPath: string | null;
  deletionArmed: boolean;
  onClose: () => void;
  /** Called after a successful relocation: the tree on screen is now wrong. */
  onRelocated: (report: RelocateReportView) => void;
}

export function RelocateDialog({
  generation,
  node,
  sourcePath,
  scanRootPath,
  deletionArmed,
  onClose,
  onRelocated,
}: RelocateDialogProps) {
  const volumes = useVolumes();
  const [destination, setDestination] = useState("");
  const [mode, setMode] = useState<RelocateMode>("migrate");
  const [disposal, setDisposal] = useState<SourceDisposal>("trash");
  const [plan, setPlan] = useState<RelocatePlanView | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planning, setPlanning] = useState(false);
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<RelocateReportView | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  /**
   * Volumes worth offering.
   *
   * macOS-owned volumes (Preboot, VM, Update) are never a destination, and a
   * volume on the same device as the source is listed last rather than hidden:
   * moving within a device is a legitimate reorganization, it just frees
   * nothing, and the backend says so in a warning.
   */
  const sourceMount = useMemo(
    () => longestMountPrefix(volumes.data ?? [], scanRootPath),
    [volumes.data, scanRootPath],
  );

  const candidates = useMemo(() => {
    const rows = (volumes.data ?? []).filter((volume) => !volume.isSystem);
    return [...rows].sort((a, b) => {
      const aSame = a.mountPoint === sourceMount ? 1 : 0;
      const bSame = b.mountPoint === sourceMount ? 1 : 0;
      if (aSame !== bSame) return aSame - bSame;
      return b.availableBytes - a.availableBytes;
    });
  }, [volumes.data, sourceMount]);

  // Reset everything when the dialog is opened on a different node; a stale
  // plan from a previous target is the one thing that must never linger in a
  // confirmation UI.
  useEffect(() => {
    setPlan(null);
    setPlanError(null);
    setReport(null);
    setFailure(null);
    setDestination("");
    setMode("migrate");
    setDisposal("trash");
  }, [node]);

  useEffect(() => {
    if (node === null || destination.trim().length === 0) {
      setPlan(null);
      setPlanError(null);
      return;
    }
    let cancelled = false;
    setPlanning(true);
    relocatePlan(generation, node, destination.trim(), mode, disposal)
      .then((next) => {
        if (cancelled) return;
        setPlan(next);
        setPlanError(null);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setPlan(null);
        setPlanError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (!cancelled) setPlanning(false);
      });
    return () => {
      cancelled = true;
    };
  }, [generation, node, destination, mode, disposal]);

  const handleConfirm = useCallback(async () => {
    if (node === null || plan?.token == null) return;
    setRunning(true);
    setFailure(null);
    try {
      const next = await relocateApply(generation, node, destination.trim(), mode, disposal, plan.token);
      setReport(next);
      onRelocated(next);
    } catch (cause) {
      setFailure(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRunning(false);
    }
  }, [generation, node, destination, mode, disposal, plan?.token, onRelocated]);

  if (node === null) return null;

  const blocked = plan !== null && plan.token === null;
  const canConfirm = plan?.token != null && deletionArmed && !running && report === null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Move to another volume"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-6"
    >
      <div className="flex max-h-full w-full max-w-2xl flex-col overflow-hidden rounded-lg border border-border bg-background shadow-2xl">
        <header className="flex shrink-0 items-start gap-3 border-b border-border/60 p-4">
          <div className="min-w-0 flex-1">
            <h2 className="text-sm font-medium">Move to another volume</h2>
            <p className="mt-0.5 truncate font-mono text-xs text-muted-foreground" title={sourcePath ?? undefined}>
              {sourcePath ?? "…"}
            </p>
          </div>
          <Button variant="ghost" size="icon" onClick={onClose} title="Close">
            <X aria-hidden />
            <span className="sr-only">Close</span>
          </Button>
        </header>

        <div className="min-h-0 flex-1 overflow-auto p-4">
          {report !== null ? (
            <RelocateResult report={report} />
          ) : (
            <>
              <DestinationPicker
                candidates={candidates}
                loading={volumes.isLoading}
                sourceMount={sourceMount}
                destination={destination}
                onDestination={setDestination}
              />

              <ModeControls mode={mode} disposal={disposal} onMode={setMode} onDisposal={setDisposal} />

              {planning && (
                <p className="mt-4 flex items-center gap-2 text-xs text-muted-foreground">
                  <Loader2 aria-hidden className="size-3.5 animate-spin" />
                  Checking the destination…
                </p>
              )}

              {planError !== null && (
                <Alert variant="destructive" className="mt-4">
                  <AlertTriangle aria-hidden />
                  <AlertTitle>This move cannot be planned</AlertTitle>
                  <AlertDescription>{planError}</AlertDescription>
                </Alert>
              )}

              {plan !== null && <PlanSummary plan={plan} blocked={blocked} />}

              {failure !== null && (
                <Alert variant="destructive" className="mt-4">
                  <AlertTriangle aria-hidden />
                  <AlertTitle>The move did not complete</AlertTitle>
                  <AlertDescription>
                    {failure}
                    <span className="mt-1 block text-xs">
                      Unless this says otherwise, the original was left exactly where it was.
                    </span>
                  </AlertDescription>
                </Alert>
              )}
            </>
          )}
        </div>

        <footer className="flex shrink-0 items-center gap-3 border-t border-border/60 p-4">
          {!deletionArmed && report === null && (
            <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Lock aria-hidden className="size-3.5" />
              Turn on destructive actions in the details panel to enable this.
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Button variant="ghost" onClick={onClose}>
              {report === null ? "Cancel" : "Done"}
            </Button>
            {report === null && (
              <Button onClick={() => void handleConfirm()} disabled={!canConfirm}>
                {running ? (
                  <>
                    <Loader2 aria-hidden className="animate-spin" />
                    Copying and verifying…
                  </>
                ) : (
                  <>Move and link</>
                )}
              </Button>
            )}
          </div>
        </footer>
      </div>
    </div>
  );
}

/**
 * Which mounted volume a path lives on.
 *
 * Longest matching mount point wins, which is the only correct answer when
 * mounts nest — `/` matches everything, so a path under `/Volumes/tuf8tb` has
 * to prefer the deeper mount. Matching is on whole path segments so
 * `/Volumes/Backup2` is not read as living on `/Volumes/Backup`.
 */
function longestMountPrefix(volumes: readonly VolumeRow[], path: string | null): string | null {
  if (path === null) return null;
  let best: string | null = null;
  for (const volume of volumes) {
    const mount = volume.mountPoint;
    const matches = path === mount || mount === "/" || path.startsWith(mount.endsWith("/") ? mount : `${mount}/`);
    if (matches && (best === null || mount.length > best.length)) best = mount;
  }
  return best;
}

function DestinationPicker({
  candidates,
  loading,
  sourceMount,
  destination,
  onDestination,
}: {
  candidates: readonly VolumeRow[];
  loading: boolean;
  sourceMount: string | null;
  destination: string;
  onDestination: (value: string) => void;
}) {
  return (
    <section>
      <h3 className="text-xs font-medium text-muted-foreground">Destination</h3>
      {loading && <p className="mt-2 text-xs text-muted-foreground">Reading volumes…</p>}
      <ul className="mt-2 flex flex-col gap-1">
        {candidates.map((volume) => {
          const selected = destination === volume.mountPoint;
          const sameDevice = volume.mountPoint === sourceMount;
          return (
            <li key={volume.mountPoint}>
              <button
                type="button"
                onClick={() => onDestination(volume.mountPoint)}
                className={cn(
                  "flex w-full items-center gap-3 rounded border px-3 py-2 text-left transition-colors",
                  selected ? "border-brand bg-accent" : "border-border/60 hover:bg-accent/50",
                )}
              >
                <HardDrive aria-hidden className="size-4 shrink-0 text-muted-foreground" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm">{volume.name}</span>
                  <span className="block truncate font-mono text-xs text-muted-foreground">
                    {volume.mountPoint}
                  </span>
                </span>
                <span className="shrink-0 text-right text-xs text-muted-foreground">
                  <span className="block">{formatSI(volume.availableBytes)} free</span>
                  {sameDevice && <span className="block text-[10px]">same volume as the source</span>}
                </span>
              </button>
            </li>
          );
        })}
      </ul>

      <label className="mt-2 block">
        <span className="text-xs text-muted-foreground">
          Destination folder — must already exist. The item keeps its own name inside it.
        </span>
        <input
          type="text"
          value={destination}
          onChange={(event) => onDestination(event.target.value)}
          placeholder="/Volumes/…"
          spellCheck={false}
          className="mt-1 w-full rounded border border-border/60 bg-transparent px-2 py-1 font-mono text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        />
      </label>
    </section>
  );
}

function ModeControls({
  mode,
  disposal,
  onMode,
  onDisposal,
}: {
  mode: RelocateMode;
  disposal: SourceDisposal;
  onMode: (mode: RelocateMode) => void;
  onDisposal: (disposal: SourceDisposal) => void;
}) {
  return (
    <section className="mt-4 grid grid-cols-2 gap-4">
      <Choice
        label="How"
        value={mode}
        onChange={onMode}
        options={[
          { value: "migrate", label: "Copy it there", hint: "The destination does not exist yet." },
          { value: "repoint", label: "Adopt a copy", hint: "The data is already at the destination." },
        ]}
      />
      <Choice
        label="Then the original"
        value={disposal}
        onChange={onDisposal}
        options={[
          { value: "trash", label: "Goes to the Trash", hint: "Recoverable. Space returns when emptied." },
          { value: "delete", label: "Is deleted", hint: "Space returns now. Not undoable." },
        ]}
      />
    </section>
  );
}

function Choice<T extends string>({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: T;
  onChange: (value: T) => void;
  options: readonly { value: T; label: string; hint: string }[];
}) {
  return (
    <div role="radiogroup" aria-label={label}>
      <h3 className="text-xs font-medium text-muted-foreground">{label}</h3>
      <div className="mt-2 flex flex-col gap-1">
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={value === option.value}
            onClick={() => onChange(option.value)}
            className={cn(
              "rounded border px-2 py-1.5 text-left transition-colors",
              value === option.value ? "border-brand bg-accent" : "border-border/60 hover:bg-accent/50",
            )}
          >
            <span className="block text-xs">{option.label}</span>
            <span className="block text-[10px] text-muted-foreground">{option.hint}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

function PlanSummary({ plan, blocked }: { plan: RelocatePlanView; blocked: boolean }) {
  const remaining = plan.destinationAvailable - plan.allocated;
  return (
    <section className="mt-4">
      <div className="flex items-center gap-2 rounded border border-border/60 p-3 text-xs">
        <span className="min-w-0 flex-1 truncate font-mono" title={plan.source}>
          {plan.source}
        </span>
        <ArrowRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-mono" title={plan.destination}>
          {plan.destination}
        </span>
      </div>

      <dl className="mt-2 grid grid-cols-3 gap-2 text-xs">
        <Stat label="To move" value={formatSI(plan.allocated)} />
        <Stat label="Free there now" value={formatSI(plan.destinationAvailable)} />
        <Stat
          label="Free there after"
          value={remaining >= 0 ? formatSI(remaining) : "not enough room"}
          alarming={remaining < 0}
        />
      </dl>

      {plan.unreadable > 0 && (
        <p className="mt-2 text-xs text-muted-foreground">
          {plan.retainedNodes.toLocaleString()} items counted, but {plan.unreadable.toLocaleString()}{" "}
          director{plan.unreadable === 1 ? "y" : "ies"} could not be read — the real size may be larger.
        </p>
      )}

      {plan.warnings.length > 0 && (
        <Alert variant={blocked ? "destructive" : "default"} className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>{blocked ? "This move cannot proceed" : "Before you confirm"}</AlertTitle>
          <AlertDescription>
            <ul className="flex list-disc flex-col gap-1 pl-4">
              {plan.warnings.map((warning) => (
                <li key={warning.code}>{warning.message}</li>
              ))}
            </ul>
          </AlertDescription>
        </Alert>
      )}
    </section>
  );
}

function Stat({ label, value, alarming = false }: { label: string; value: string; alarming?: boolean }) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5">
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className={cn("rds-numeric text-sm", alarming && "text-pressure-critical")}>{value}</dd>
    </div>
  );
}

function RelocateResult({ report }: { report: RelocateReportView }) {
  const kept = report.disposal === "keep";
  return (
    <div>
      <Alert variant={kept ? "default" : "default"}>
        {kept ? <AlertTriangle aria-hidden /> : <Check aria-hidden />}
        <AlertTitle>{kept ? "Copied, but the original was kept" : "Moved"}</AlertTitle>
        <AlertDescription>
          {report.filesVerified.toLocaleString()} items ({formatSI(report.bytesVerified)}) were copied and
          verified byte-for-byte.
          {report.symlinkCreated && " The original path is now a link to the new location, so existing references still work."}
          {kept && report.specialFiles > 0 && (
            <span className="mt-1 block">
              {report.specialFiles.toLocaleString()} socket{report.specialFiles === 1 ? "" : "s"} or pipe
              {report.specialFiles === 1 ? "" : "s"} could not be copied — nothing can copy those — so the
              original was left in place rather than losing them. No link was created.
            </span>
          )}
        </AlertDescription>
      </Alert>
      <p className="mt-3 font-mono text-xs text-muted-foreground">{report.destination}</p>
      <p className="mt-3 text-xs text-muted-foreground">
        The sizes on screen still describe the volume as it was scanned. Re-scan to see the space return.
      </p>
    </div>
  );
}
