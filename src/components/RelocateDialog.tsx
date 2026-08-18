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

import { PathField } from "@/components/PathField";
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
  /**
   * The nodes to move. Empty closes the dialog.
   *
   * One and many share this surface deliberately: a migration is the same
   * operation whether it is one 58 GB disk image or twelve build folders, and
   * splitting it into two dialogs would mean two places for the safety rules
   * to drift apart.
   */
  nodes: readonly number[];
  /** The sole node's path, for the header. Omitted for a multi-selection. */
  sourcePath: string | null;
  /**
   * The scan root. Used only to work out which volume the source is *on*, by
   * longest-mount-point-prefix match, so that volume can be ranked last among
   * the destinations — moving to the disk you are trying to empty is the one
   * choice that cannot help.
   */
  scanRootPath: string | null;
  deletionArmed: boolean;
  /**
   * A destination proposed by the split view.
   *
   * Only ever a *starting point*: the field stays editable and the plan is
   * still minted by the backend against whatever the field finally says. A
   * pre-filled path that could not be changed would move the decision out of
   * the dialog that owns the safety rules and into the pane that suggested it.
   */
  initialDestination?: string | null;
  onClose: () => void;
  /** Called after a successful relocation: the tree on screen is now wrong. */
  onRelocated: (report: RelocateReportView) => void;
}

export function RelocateDialog({
  generation,
  nodes,
  sourcePath,
  scanRootPath,
  deletionArmed,
  initialDestination = null,
  onClose,
  onRelocated,
}: RelocateDialogProps) {
  const volumes = useVolumes();
  const [destination, setDestination] = useState(initialDestination ?? "");
  /*
   * Adopt a destination proposed by the split view.
   *
   * Keyed on the proposal itself rather than on `nodes`, so opening the dialog
   * again for the same folder re-seeds it, while anything typed in the field
   * afterwards survives — the effect does not run again until the proposal
   * changes.
   */
  useEffect(() => {
    if (initialDestination !== null) setDestination(initialDestination);
  }, [initialDestination]);

  const [mode, setMode] = useState<RelocateMode>("migrate");
  const [disposal, setDisposal] = useState<SourceDisposal>("trash");
  const [plans, setPlans] = useState<readonly RelocatePlanView[] | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planning, setPlanning] = useState(false);
  const [running, setRunning] = useState(false);
  /** How many of `nodes` have been attempted, for the progress line. */
  const [done, setDone] = useState(0);
  const [outcomes, setOutcomes] = useState<readonly BatchOutcome[] | null>(null);
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

  // A stable key for "the same selection", so the reset below fires when the
  // set changes rather than on every render that rebuilds the array.
  const selectionKey = nodes.join(",");

  // Reset everything when the dialog is opened on a different selection; a
  // stale plan from a previous target is the one thing that must never linger
  // in a confirmation UI.
  useEffect(() => {
    setPlans(null);
    setPlanError(null);
    setOutcomes(null);
    setDone(0);
    setFailure(null);
    setDestination("");
    setMode("migrate");
    setDisposal("trash");
  }, [selectionKey]);

  /*
   * Plan EVERY selected item, not just the first.
   *
   * A batch is only as safe as its worst member: one item landing on a
   * filesystem that cannot hold its metadata, or overlapping the destination,
   * has to surface before the user confirms twelve moves. So each gets its own
   * backend plan and its own token, and the summary below reports the whole
   * set rather than a sample of it.
   */
  useEffect(() => {
    const target = destination.trim();
    if (nodes.length === 0 || target.length === 0) {
      setPlans(null);
      setPlanError(null);
      return;
    }
    let cancelled = false;
    setPlanning(true);
    Promise.all(nodes.map((node) => relocatePlan(generation, node, target, mode, disposal)))
      .then((next) => {
        if (cancelled) return;
        setPlans(next);
        setPlanError(null);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setPlans(null);
        setPlanError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (!cancelled) setPlanning(false);
      });
    return () => {
      cancelled = true;
    };
  }, [generation, selectionKey, nodes, destination, mode, disposal]);

  /*
   * Applied one at a time, in order, and a failure does NOT abort the rest.
   *
   * Sequential rather than parallel because each move is disk-bound: running
   * twelve `ditto` copies at once would contend for the same two devices and
   * finish later than doing them in turn, while making the progress line
   * meaningless.
   *
   * Continuing past a failure is the deliberate part. Each item is
   * independently verified and independently disposed of, so item 4 failing
   * says nothing about item 5 — and stopping would strand a half-migrated set
   * with no record of which half. Every outcome is collected and reported.
   */
  const handleConfirm = useCallback(async () => {
    const ready = plans?.filter((plan) => plan.token !== null) ?? [];
    if (ready.length === 0) return;
    setRunning(true);
    setFailure(null);
    setDone(0);

    const collected: BatchOutcome[] = [];
    for (const plan of ready) {
      try {
        const report = await relocateApply(
          generation,
          plan.node,
          destination.trim(),
          mode,
          disposal,
          plan.token as string,
        );
        collected.push({ source: plan.source, report, error: null });
        onRelocated(report);
      } catch (cause) {
        collected.push({
          source: plan.source,
          report: null,
          error: cause instanceof Error ? cause.message : String(cause),
        });
      }
      setDone(collected.length);
    }

    setOutcomes(collected);
    setRunning(false);
  }, [generation, plans, destination, mode, disposal, onRelocated]);

  // Escape closes, as it does for every other modal on the platform — except
  // mid-run, when there is a copy in flight and nothing safe to cancel into.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !running) onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose, running]);

  if (nodes.length === 0) return null;

  const actionable = plans?.filter((plan) => plan.token !== null) ?? [];
  const refused = plans?.filter((plan) => plan.token === null) ?? [];
  // Blocked means NOTHING can move. A partially-blocked batch is not blocked —
  // it proceeds with the items that can, and says which it is leaving.
  const blocked = plans !== null && actionable.length === 0;
  const canConfirm = actionable.length > 0 && deletionArmed && !running && outcomes === null;

  /*
   * The label carries the state, not just the styling.
   *
   * A disabled brand-coloured button at 50% opacity still reads as "clickable"
   * on a dark surface, and for a destructive action that ambiguity is the
   * wrong way round: the user should never have to infer refusal from a shade
   * of purple. So the button says which of the three reasons applies.
   */
  const confirmLabel = running
    ? nodes.length > 1
      ? `Moving ${done + 1} of ${actionable.length}…`
      : "Copying and verifying…"
    : blocked
      ? "Cannot move"
      : !deletionArmed
        ? "Moving is off"
        : "Move and link";

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
            <h2 className="text-sm font-medium">
              {nodes.length === 1 ? "Move to another volume" : `Move ${nodes.length} items`}
            </h2>
            <p className="mt-0.5 truncate font-mono text-xs text-muted-foreground" title={sourcePath ?? undefined}>
              {nodes.length === 1
                ? (sourcePath ?? "…")
                : "Each keeps its own name at the destination."}
            </p>
          </div>
          <Button variant="ghost" size="icon" onClick={onClose} title="Close">
            <X aria-hidden />
            <span className="sr-only">Close</span>
          </Button>
        </header>

        <div className="min-h-0 flex-1 overflow-auto p-4">
          {outcomes !== null ? (
            <BatchResult outcomes={outcomes} />
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

              {plans !== null && (
                <PlanSummary plans={plans} actionable={actionable} refused={refused} blocked={blocked} />
              )}

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
          {!deletionArmed && outcomes === null && (
            <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Lock aria-hidden className="size-3.5" />
              Turn on destructive actions in the details panel to enable this.
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Button variant="ghost" onClick={onClose} disabled={running}>
              {outcomes === null ? "Cancel" : "Done"}
            </Button>
            {outcomes === null && (
              <Button
                onClick={() => void handleConfirm()}
                disabled={!canConfirm}
                variant={blocked ? "outline" : "default"}
              >
                {running && <Loader2 aria-hidden className="animate-spin" />}
                {confirmLabel}
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

      <PathField
        className="mt-2"
        inputId="relocate-destination"
        layout="stacked"
        label="Destination folder"
        placeholder="/Volumes/…"
        value={destination}
        onChange={onDestination}
        hint="Must already exist. The item keeps its own name inside it."
      />
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

/** One item's fate in a batch move. */
interface BatchOutcome {
  readonly source: string;
  readonly report: RelocateReportView | null;
  readonly error: string | null;
}

function PlanSummary({
  plans,
  actionable,
  refused,
  blocked,
}: {
  plans: readonly RelocatePlanView[];
  actionable: readonly RelocatePlanView[];
  refused: readonly RelocatePlanView[];
  blocked: boolean;
}) {
  const first = plans[0];
  if (first === undefined) return null;

  // Only the items that will actually move count toward the size and the
  // free-space arithmetic. Including refused ones would overstate the cost and
  // could claim there is not enough room for a move that fits.
  const moving = actionable.reduce((total, plan) => total + plan.allocated, 0);
  const remaining = first.destinationAvailable - moving;
  const unreadable = actionable.reduce((total, plan) => total + plan.unreadable, 0);

  // Deduped, because a batch bound for one destination produces the same
  // destination-level warning once per item — five copies of "this frees no
  // space" is noise that hides the one warning that is specific.
  const warnings = new Map<string, string>();
  for (const plan of plans) {
    for (const warning of plan.warnings) warnings.set(warning.message, warning.code);
  }

  return (
    <section className="mt-4">
      {plans.length === 1 ? (
        <div className="flex items-center gap-2 rounded border border-border/60 p-3 text-xs">
          <span className="min-w-0 flex-1 truncate font-mono" title={first.source}>
            {first.source}
          </span>
          <ArrowRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate font-mono" title={first.destination}>
            {first.destination}
          </span>
        </div>
      ) : (
        <ul className="max-h-32 overflow-auto rounded border border-border/60 text-xs">
          {plans.map((plan) => (
            <li
              key={plan.node}
              className={cn(
                "flex items-center gap-2 px-3 py-1",
                plan.token === null && "text-muted-foreground line-through",
              )}
              title={plan.token === null ? "This one cannot move — see below" : plan.source}
            >
              <span className="min-w-0 flex-1 truncate font-mono">{plan.source}</span>
              <span className="shrink-0 rds-numeric">{formatSI(plan.allocated)}</span>
            </li>
          ))}
        </ul>
      )}

      <dl className="mt-2 grid grid-cols-3 gap-2 text-xs">
        <Stat
          label={actionable.length === plans.length ? "To move" : `Moving ${actionable.length} of ${plans.length}`}
          value={formatSI(moving)}
        />
        <Stat
          label={`Free there (${first.destinationFilesystem})`}
          value={formatSI(first.destinationAvailable)}
        />
        <Stat
          label="Free there after"
          value={remaining >= 0 ? formatSI(remaining) : "not enough room"}
          alarming={remaining < 0}
        />
      </dl>

      {unreadable > 0 && (
        <p className="mt-2 text-xs text-muted-foreground">
          {unreadable.toLocaleString()} director{unreadable === 1 ? "y" : "ies"} could not be read during
          the scan, so the real size may be larger than shown.
        </p>
      )}

      {refused.length > 0 && !blocked && (
        <Alert className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>
            {refused.length} of {plans.length} will be skipped
          </AlertTitle>
          <AlertDescription>
            The rest will still move. Skipped items stay exactly where they are.
          </AlertDescription>
        </Alert>
      )}

      {warnings.size > 0 && (
        <Alert variant={blocked ? "destructive" : "default"} className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>{blocked ? "This move cannot proceed" : "Before you confirm"}</AlertTitle>
          <AlertDescription>
            <ul className="flex list-disc flex-col gap-1 pl-4">
              {[...warnings.keys()].map((message) => (
                <li key={message}>{message}</li>
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

/**
 * What actually happened, item by item.
 *
 * A batch reports per item rather than as a total, because "10 of 12 moved" is
 * not an outcome anyone can act on — the two that did not move are the whole
 * message, and the user needs to know WHICH two and why. Successes collapse to
 * a count; failures are listed in full.
 */
function BatchResult({ outcomes }: { outcomes: readonly BatchOutcome[] }) {
  const moved = outcomes.filter((outcome) => outcome.report !== null && outcome.error === null);
  const kept = moved.filter((outcome) => outcome.report?.disposal === "keep");
  const failed = outcomes.filter((outcome) => outcome.error !== null);
  const bytes = moved.reduce((total, outcome) => total + (outcome.report?.bytesVerified ?? 0), 0);
  const files = moved.reduce((total, outcome) => total + (outcome.report?.filesVerified ?? 0), 0);

  return (
    <div>
      <Alert variant={failed.length > 0 ? "destructive" : "default"}>
        {failed.length > 0 ? <AlertTriangle aria-hidden /> : <Check aria-hidden />}
        <AlertTitle>
          {failed.length === 0
            ? `Moved ${moved.length === 1 ? "1 item" : `${moved.length} items`}`
            : `Moved ${moved.length} of ${outcomes.length} — ${failed.length} failed`}
        </AlertTitle>
        <AlertDescription>
          {files.toLocaleString()} files ({formatSI(bytes)}) were copied and verified byte-for-byte.
          {moved.length > kept.length &&
            " Each original path is now a link to its new location, so existing references still work."}
        </AlertDescription>
      </Alert>

      {kept.length > 0 && (
        <Alert className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>
            {kept.length === 1 ? "One original was kept" : `${kept.length} originals were kept`}
          </AlertTitle>
          <AlertDescription>
            These contained sockets or pipes, which nothing can copy. Rather than lose them, the
            originals were left in place and no link was created — so these are copies, not moves.
          </AlertDescription>
        </Alert>
      )}

      {failed.length > 0 && (
        <ul className="mt-3 flex flex-col gap-2">
          {failed.map((outcome) => (
            <li key={outcome.source} className="rounded border border-border/60 p-2 text-xs">
              <p className="truncate font-mono" title={outcome.source}>
                {outcome.source}
              </p>
              <p className="mt-0.5 text-muted-foreground">{outcome.error}</p>
            </li>
          ))}
          <li className="text-xs text-muted-foreground">
            Anything that failed was left exactly where it was.
          </li>
        </ul>
      )}

      <p className="mt-3 text-xs text-muted-foreground">
        The sizes on screen still describe the volume as it was scanned. Re-scan to see the space
        return.
      </p>
    </div>
  );
}
