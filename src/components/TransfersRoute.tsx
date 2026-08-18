/**
 * Uploading a folder somewhere that is not a disk, and watching it happen.
 *
 * Same three-step shape as the local Sync panel — check, review, then copy —
 * because it is the same promise: **nothing at the destination is deleted, and
 * nothing is overwritten unless you ask.** The Start button keys off the
 * backend-minted token rather than off the check having succeeded, so a plan
 * that went stale between the review and the click is refused rather than
 * acted on.
 *
 * What is different, and why the queue exists at all: a remote copy is not a
 * local one. Uploading 400 GB to a NAS is an overnight job, so a transfer is a
 * record rather than a call. It survives closing this panel, it survives
 * quitting the app, and it is resumed by re-planning — which is why "what does
 * the destination not already have" is the only question this app ever asks.
 *
 * Two numbers this panel deliberately does not show:
 *
 * - **Free space at the destination.** A bucket does not publish one. The local
 *   plan has a real figure here and this one has `null`; rendering 0 would read
 *   as "full", which is worse than admitting it is unknown.
 * - **A time estimate.** Bandwidth to a home NAS over Wi-Fi is not stationary
 *   enough for one to mean anything, and a wrong estimate is what makes people
 *   cancel a job that was nearly finished.
 */

import {
  AlertTriangle,
  ArrowRight,
  Check,
  CloudUpload,
  Loader2,
  Pause,
  Play,
  SearchCheck,
  Trash2,
  X,
} from "lucide-react";
import { useState } from "react";

import { PathField } from "@/components/PathField";
import { RemoteTargets } from "@/components/RemoteTargets";
import { SegmentedControl } from "@/components/SegmentedControl";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import {
  isActiveJob,
  isResumableJob,
  type JobState,
  type OnDiffer,
  type RemoteCompare,
  type RemotePlanView,
  type TransferJobView,
} from "@/lib/ipc";
import {
  useClearTransfers,
  useEnqueueTransfer,
  usePlanRemote,
  useRemoteTargets,
  useTransferControl,
  useTransfers,
} from "@/lib/queries";
import { cn } from "@/lib/utils";

const COMPARE_OPTIONS = [
  {
    value: "quick" as RemoteCompare,
    label: "Quick",
    title: "Compares name and size against one listing of the destination. No extra requests.",
  },
  {
    value: "verify" as RemoteCompare,
    label: "Verify",
    title:
      "Also compares checksums where the destination publishes one. Reads every same-sized local file in full.",
  },
];

const DIFFER_OPTIONS = [
  {
    value: "skip" as OnDiffer,
    label: "Keep theirs",
    title: "A file that already exists at the destination is left alone, even if it differs.",
  },
  {
    value: "replace" as OnDiffer,
    label: "Use source",
    title: "Overwrite the destination's copy when the two differ. The only destructive setting.",
  },
];

const REASON_LABEL: Record<string, string> = {
  missing: "not there",
  size_differs: "different size",
  content_differs: "different contents",
};

const STATE_LABEL: Record<JobState, string> = {
  queued: "Waiting",
  planning: "Checking the destination",
  running: "Uploading",
  paused: "Paused",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
};

export function TransfersRoute() {
  const targets = useRemoteTargets();
  const jobs = useTransfers();

  const [source, setSource] = useState("");
  const [target, setTarget] = useState("");
  const [compare, setCompare] = useState<RemoteCompare>("quick");
  const [onDiffer, setOnDiffer] = useState<OnDiffer>("skip");
  const [plan, setPlan] = useState<RemotePlanView | null>(null);

  const planning = usePlanRemote();
  const enqueue = useEnqueueTransfer();
  const clear = useClearTransfers();

  const rows = targets.data ?? [];
  const chosen = target.length > 0 ? target : (rows[0]?.name ?? "");
  const request = { source: source.trim(), target: chosen, compare, onDiffer };
  const ready = request.source.length > 0 && chosen.length > 0;

  /*
   * Checking is explicit, not reactive on every keystroke — the same reasoning
   * as the local Sync panel, with an extra cost: this one also lists the whole
   * destination over the network, which is somebody else's server.
   */
  const check = () => {
    setPlan(null);
    planning.mutate(request, { onSuccess: setPlan });
  };

  const start = () => {
    if (plan?.token == null) return;
    enqueue.mutate(
      { request, token: plan.token },
      {
        // The plan described a moment that has now been acted on. Leaving it
        // on screen next to a running job invites queueing the same upload
        // twice.
        onSuccess: () => setPlan(null),
      },
    );
  };

  const finished = (jobs.data ?? []).filter((job) => !isActiveJob(job.state) && job.state !== "paused");

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-auto p-4">
      <header className="mb-3">
        <h2 className="text-sm font-medium">Transfers</h2>
        <p className="mt-1 text-xs text-muted-foreground">
          Uploads files a remote destination is missing. Nothing there is deleted, and nothing is
          overwritten unless you ask. Transfers keep going if you close this panel, and are picked
          up again if you quit.
        </p>
      </header>

      <RemoteTargets className="mb-4" />

      <section className="flex flex-col gap-2 border-t border-border/60 pt-4">
        <h3 className="text-sm font-medium">Upload a folder</h3>
        <PathField
          inputId="transfer-source"
          label="Folder"
          placeholder="/Volumes/Archive/photos"
          value={source}
          onChange={setSource}
        />
        <div className="flex items-center gap-2">
          <label className="w-20 shrink-0 text-xs text-muted-foreground" htmlFor="transfer-target">
            To
          </label>
          <select
            id="transfer-target"
            value={chosen}
            disabled={rows.length === 0}
            onChange={(event) => setTarget(event.target.value)}
            className="min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 text-xs disabled:opacity-60"
          >
            {rows.length === 0 && <option value="">Add a destination above first</option>}
            {rows.map((row) => (
              <option key={row.name} value={row.name}>
                {row.name}
              </option>
            ))}
          </select>
        </div>
      </section>

      <section className="mt-3 flex flex-wrap items-center gap-4">
        <Labelled label="Compare by">
          <SegmentedControl
            label="How to compare files"
            options={COMPARE_OPTIONS}
            value={compare}
            onChange={setCompare}
          />
        </Labelled>
        <Labelled label="If it is already there">
          <SegmentedControl
            label="What to do when a file exists at the destination"
            options={DIFFER_OPTIONS}
            value={onDiffer}
            onChange={setOnDiffer}
          />
        </Labelled>

        <Button
          className="ml-auto"
          variant="outline"
          disabled={!ready || planning.isPending}
          onClick={check}
        >
          {planning.isPending ? (
            <Loader2 aria-hidden className="animate-spin" />
          ) : (
            <SearchCheck aria-hidden />
          )}
          {planning.isPending ? "Checking…" : "Check"}
        </Button>
      </section>

      {planning.isError && (
        <Alert variant="destructive" className="mt-4">
          <AlertTriangle aria-hidden />
          <AlertTitle>This destination could not be checked</AlertTitle>
          <AlertDescription>{planning.error.message}</AlertDescription>
        </Alert>
      )}
      {enqueue.isError && (
        <Alert variant="destructive" className="mt-4">
          <AlertTriangle aria-hidden />
          <AlertTitle>This transfer could not be started</AlertTitle>
          <AlertDescription>{enqueue.error.message}</AlertDescription>
        </Alert>
      )}

      {plan !== null && <PlanReview plan={plan} starting={enqueue.isPending} onStart={start} />}

      <section className="mt-6 border-t border-border/60 pt-4">
        <header className="mb-2 flex items-center justify-between">
          <h3 className="text-sm font-medium">Queue</h3>
          {finished.length > 0 && (
            <Button variant="outline" size="sm" disabled={clear.isPending} onClick={() => clear.mutate()}>
              <Trash2 aria-hidden />
              Clear {finished.length} finished
            </Button>
          )}
        </header>

        {(jobs.data ?? []).length === 0 ? (
          <p className="rounded border border-dashed border-border/60 p-4 text-center text-xs text-muted-foreground">
            Nothing queued.
          </p>
        ) : (
          <ul className="flex flex-col gap-1">
            {(jobs.data ?? []).map((job) => (
              <JobRow key={job.id} job={job} />
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

function Labelled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1">
      <span className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

function PlanReview({
  plan,
  starting,
  onStart,
}: {
  plan: RemotePlanView;
  starting: boolean;
  onStart: () => void;
}) {
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

      <dl className="mt-2 grid grid-cols-4 gap-2 text-xs">
        <Stat label="To upload" value={plan.totalToCopy.toLocaleString()} />
        <Stat label="Size" value={formatSI(plan.bytesToCopy)} />
        <Stat label="Already there" value={plan.alreadyPresent.toLocaleString()} />
        {/*
         * Not a free-space figure. A bucket does not publish one, and the
         * honest answer is the absence of a number rather than a zero.
         */}
        <Stat
          label="Space there"
          value="unknown"
          hint="Remote storage does not report how much room is left."
        />
      </dl>

      {plan.warnings.length > 0 && (
        <Alert variant={plan.token === null ? "destructive" : "default"} className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>{plan.token === null ? "Nothing will be uploaded" : "Before you upload"}</AlertTitle>
          <AlertDescription>
            <ul className="flex list-disc flex-col gap-1 pl-4">
              {plan.warnings.map((warning) => (
                <li key={warning.code}>{warning.message}</li>
              ))}
            </ul>
          </AlertDescription>
        </Alert>
      )}

      {plan.entries.length > 0 && (
        <>
          <ul className="mt-3 max-h-72 overflow-auto rounded border border-border/60 text-xs">
            {plan.entries.map((entry) => (
              <li
                key={entry.relativePath}
                className="flex items-center gap-2 border-b border-border/40 px-3 py-1 last:border-b-0"
              >
                <span className="min-w-0 flex-1 truncate font-mono" title={entry.relativePath}>
                  {entry.relativePath}
                </span>
                <span className="shrink-0 text-[10px] text-muted-foreground">
                  {REASON_LABEL[entry.reason] ?? entry.reason}
                </span>
                <span className="rds-numeric w-20 shrink-0 text-right">{formatSI(entry.bytes)}</span>
              </li>
            ))}
          </ul>
          {plan.entriesTruncated && (
            <p className="mt-1 text-xs text-muted-foreground">
              Showing the first {plan.entries.length.toLocaleString()} of{" "}
              {plan.totalToCopy.toLocaleString()}. All of them will be uploaded.
            </p>
          )}
        </>
      )}

      <div className="mt-3 flex justify-end">
        <Button disabled={plan.token === null || starting} onClick={onStart}>
          {starting ? <Loader2 aria-hidden className="animate-spin" /> : <CloudUpload aria-hidden />}
          {starting
            ? "Starting…"
            : plan.token === null
              ? "Nothing to upload"
              : `Upload ${plan.totalToCopy.toLocaleString()} file${plan.totalToCopy === 1 ? "" : "s"}`}
        </Button>
      </div>
    </section>
  );
}

function JobRow({ job }: { job: TransferJobView }) {
  const control = useTransferControl();
  const active = isActiveJob(job.state);

  // By bytes, not by files: a job of one 40 GB file and ten thousand small
  // ones would otherwise sit at 0% for an hour and then jump.
  const fraction = job.bytesTotal > 0 ? Math.min(1, job.bytesDone / job.bytesTotal) : 0;

  return (
    <li className="rounded border border-border/60 px-3 py-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="min-w-0 flex-1 truncate font-mono" title={job.source}>
          {job.source}
        </span>
        <ArrowRight aria-hidden className="size-3 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-mono" title={job.destination}>
          {job.destination}
        </span>

        <span
          className={cn(
            "w-32 shrink-0 text-right text-[10px] uppercase tracking-wide",
            job.state === "failed" && "text-destructive",
            job.state === "done" && "text-muted-foreground",
          )}
        >
          {STATE_LABEL[job.state]}
        </span>

        {active && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={control.isPending}
            onClick={() => control.mutate({ id: job.id, action: "pause" })}
          >
            <Pause aria-hidden />
          </Button>
        )}
        {isResumableJob(job.state) && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={control.isPending}
            onClick={() => control.mutate({ id: job.id, action: "resume" })}
          >
            <Play aria-hidden />
          </Button>
        )}
        {(active || job.state === "paused") && (
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={control.isPending}
            onClick={() => control.mutate({ id: job.id, action: "cancel" })}
          >
            <X aria-hidden />
          </Button>
        )}
        {job.state === "done" && job.failures.length === 0 && (
          <Check aria-hidden className="size-4 shrink-0 text-muted-foreground" />
        )}
      </div>

      {(active || job.state === "paused") && (
        <div className="mt-1.5 flex items-center gap-2">
          <div className="h-1 min-w-0 flex-1 overflow-hidden rounded bg-border/60">
            <div
              className="h-full bg-foreground/60 transition-[width] duration-300"
              style={{ width: `${(fraction * 100).toFixed(1)}%` }}
            />
          </div>
          <span className="rds-numeric shrink-0 text-[10px] text-muted-foreground">
            {formatSI(job.bytesDone)} of {formatSI(job.bytesTotal)} ·{" "}
            {job.filesDone.toLocaleString()}/{job.filesTotal.toLocaleString()} files
          </span>
        </div>
      )}

      {job.state === "done" && (
        <p className="mt-1 text-[11px] text-muted-foreground">
          {formatSI(job.bytesDone)} uploaded across {job.filesDone.toLocaleString()} file
          {job.filesDone === 1 ? "" : "s"}
          {job.failures.length > 0 && `, ${job.failures.length} failed`}. Nothing at the destination
          was removed.
        </p>
      )}

      {job.message !== null && <p className="mt-1 text-[11px] text-muted-foreground">{job.message}</p>}

      {job.failures.length > 0 && (
        <ul className="mt-1 flex max-h-32 flex-col gap-0.5 overflow-auto">
          {job.failures.map((failure) => (
            <li key={failure.relativePath} className="text-[11px]">
              <span className="font-mono text-muted-foreground">{failure.relativePath}</span>{" "}
              <span className="text-destructive">{failure.reason}</span>
            </li>
          ))}
          {job.failuresTruncated && (
            <li className="text-[11px] text-muted-foreground">
              …and more. Only the first {job.failures.length} are recorded.
            </li>
          )}
        </ul>
      )}
    </li>
  );
}

function Stat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5" title={hint}>
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className="rds-numeric truncate text-sm">{value}</dd>
    </div>
  );
}
