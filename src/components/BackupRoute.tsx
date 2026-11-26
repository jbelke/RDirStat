/**
 * TRANSFER > Backup: drives, profiles, a recommended preview, the live job,
 * and what each drive already holds.
 *
 * The backup itself runs in a service that lives on the destination drive
 * (`<dest>/.rdirstat/bin`), launched detached, so a job survives this app
 * quitting. This page is a window onto it: it starts services, saves
 * profiles, queues jobs, and follows `backup:events`. Nothing here copies.
 *
 * ## Why the live panel shows counters, not files
 *
 * A backup of a full disk lands hundreds of thousands of files. One DOM row
 * per file would make the page the slowest part of the copy. Events are
 * folded into counters by `applyEvents` (`@/lib/backup`), flushed to React at
 * most four times a second, and the only list is the last 200 events worth
 * reading: phases, rounds, errors, and files that did not land.
 *
 * ## Why preview cancel only stops waiting
 *
 * `backup_recommend` walks every source in the sidecar and has no cancel
 * handle. "Stop waiting" drops the answer when it arrives; the walk itself
 * finishes in the background. The button says so rather than implying the
 * disk went quiet.
 */

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  CirclePause,
  CirclePlay,
  CircleStop,
  FilePlus,
  FolderPlus,
  HardDrive,
  ListChecks,
  Loader2,
  Pencil,
  Play,
  Plus,
  Power,
  RefreshCw,
  SearchCheck,
  TriangleAlert,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  DEFAULT_FLOOR_ALLOCATED,
  DEFAULT_REVIEW_CAP,
  PHASE_LABELS,
  PROFILE_SCHEMA_VERSION,
  SERVICE_STATES,
  applyEvents,
  draftProblems,
  fillBar,
  formatEta,
  formatRate,
  formatSizeExact,
  isTerminal,
  labelFor,
  liveFromSummary,
  parseIndexStatus,
  parseSize,
  phaseTone,
  relativeInclude,
  roundEta,
  throughput,
  type DraftProblem,
  type LiveJob,
  type Tone,
} from "@/lib/backup";
import type { BackupProfileSource, Row_Serialize } from "@/lib/bindings";
import { formatDuration, formatSI } from "@/lib/format";
import {
  backupControl,
  backupIndexStatus,
  backupProfileSave,
  backupProfileValidate,
  backupProfiles,
  backupRecommend,
  backupServices,
  backupStartService,
  backupSubmitJob,
  backupWatch,
  subscribeBackupEvents,
  type BackupAction,
  type BackupDrive,
  type BackupEventsEvent,
  type BackupJobSummary,
  type BackupProfile,
  type BackupProfileValidation,
  type BackupRecommendation,
  type VolumeRow,
} from "@/lib/ipc";
import { useVolumes } from "@/lib/queries";
import { cn } from "@/lib/utils";

const SERVICES_KEY = ["backup", "services"] as const;
const profilesKey = (dest: string) => ["backup", "profiles", dest] as const;
const indexKey = (dest: string) => ["backup", "index", dest] as const;

/** How often live events reach React. */
const FLUSH_MS = 250;
/** Preview rows put in the DOM; the totals always cover every row. */
const PREVIEW_ROWS = 500;

const TONE_TEXT: Record<Tone, string> = {
  ok: "text-pressure-ok",
  warn: "text-pressure-warn",
  bad: "text-pressure-critical",
  idle: "text-muted-foreground",
};

const TONE_DOT: Record<Tone, string> = {
  ok: "bg-pressure-ok",
  warn: "bg-pressure-warn",
  bad: "bg-pressure-critical",
  idle: "bg-muted-foreground/50",
};

function message(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

export function BackupRoute() {
  const client = useQueryClient();
  const services = useQuery({
    queryKey: SERVICES_KEY,
    queryFn: backupServices,
    refetchInterval: 10_000,
  });
  const volumes = useVolumes();
  const drives = services.data ?? [];

  const [selectedDest, setSelectedDest] = useState<string | null>(null);
  const dest = selectedDest ?? drives[0]?.dest ?? null;
  const drive = drives.find((candidate) => candidate.dest === dest) ?? null;

  const [editing, setEditing] = useState<BackupProfile | null>(null);
  const [previewOf, setPreviewOf] = useState<{ dest: string; name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const live = useLiveJob();

  // Follow the first unfinished job on screen without being asked: the
  // backend re-attached it at app start, this only gives it a panel.
  const autoAttached = useRef(false);
  useEffect(() => {
    if (autoAttached.current || live.job !== null) return;
    for (const candidate of drives) {
      const job = candidate.jobs.find((summary) => summary.unfinished);
      if (job !== undefined) {
        autoAttached.current = true;
        live.follow(candidate.dest, job);
        return;
      }
    }
  }, [drives, live]);

  const refresh = useCallback(() => {
    void client.invalidateQueries({ queryKey: ["backup"] });
  }, [client]);

  const startService = async (target: string) => {
    setError(null);
    try {
      await backupStartService(target);
    } catch (cause) {
      setError(message(cause));
    } finally {
      refresh();
    }
  };

  const submit = async (target: string, name: string) => {
    setError(null);
    try {
      const summary = await backupSubmitJob(target, name);
      live.follow(target, summary, { watch: false });
    } catch (cause) {
      setError(message(cause));
    } finally {
      refresh();
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-auto p-4">
      <header className="mb-3 flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-medium">Backup</h2>
          <p className="mt-1 text-xs text-muted-foreground">
            Copies the largest files of each source to a drive that keeps its own backup service. A run
            only adds: nothing on the drive is deleted. Jobs keep running after this app quits.
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={refresh} disabled={services.isFetching}>
          <RefreshCw aria-hidden className={cn(services.isFetching && "animate-spin")} />
          Refresh
        </Button>
      </header>

      {error !== null && (
        <Alert variant="destructive" className="mb-3 pr-10">
          <TriangleAlert aria-hidden />
          <AlertTitle>That did not work</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
          <button
            type="button"
            onClick={() => setError(null)}
            aria-label="Dismiss this message"
            className="absolute right-2 top-2 rounded p-1 opacity-70 hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            <X aria-hidden className="size-4" />
          </button>
        </Alert>
      )}

      {live.job !== null && <LivePanel live={live} onError={setError} onChanged={refresh} />}

      <Section title="Drives">
        {services.isPending ? (
          <Muted>Looking for drives with backup records…</Muted>
        ) : services.isError ? (
          <Muted>{message(services.error)}</Muted>
        ) : drives.length === 0 ? (
          <Muted>
            No mounted drive has backup records yet. Create a profile below to make one a backup drive.
          </Muted>
        ) : (
          <ul className="flex flex-col gap-2">
            {drives.map((candidate) => (
              <DriveRow
                key={candidate.dest}
                drive={candidate}
                selected={candidate.dest === dest}
                onSelect={() => setSelectedDest(candidate.dest)}
                onStart={() => void startService(candidate.dest)}
                onFollow={(job) => live.follow(candidate.dest, job)}
                followingId={live.job?.dest === candidate.dest ? live.job.jobId : null}
              />
            ))}
          </ul>
        )}
      </Section>

      <Section
        title={drive !== null ? `Profiles on ${drive.name}` : "Profiles"}
        action={
          <Button
            variant="outline"
            size="sm"
            onClick={() => setEditing(newProfile(dest ?? ""))}
            disabled={editing !== null}
          >
            <Plus aria-hidden />
            New profile
          </Button>
        }
      >
        {editing !== null && (
          <ProfileEditor
            initial={editing}
            volumes={volumes.data ?? []}
            onCancel={() => setEditing(null)}
            onSaved={(saved) => {
              setSelectedDest(saved.dest);
              setEditing(null);
              refresh();
            }}
          />
        )}
        {dest !== null ? (
          <ProfileList
            dest={dest}
            busyJob={drive?.unfinished.length ? drive.unfinished[0] ?? null : null}
            onEdit={setEditing}
            onPreview={(name) => setPreviewOf({ dest, name })}
            onStart={(name) => void submit(dest, name)}
          />
        ) : (
          editing === null && <Muted>No profiles yet.</Muted>
        )}
      </Section>

      {previewOf !== null && (
        <Preview key={`${previewOf.dest}\u0000${previewOf.name}`} {...previewOf} onClose={() => setPreviewOf(null)} />
      )}

      {drive !== null && <History drive={drive} />}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Live job
// ---------------------------------------------------------------------------

interface LiveHandle {
  readonly job: LiveJob | null;
  follow: (dest: string, summary: BackupJobSummary, options?: { watch?: boolean }) => void;
  replace: (dest: string, summary: BackupJobSummary) => void;
  close: () => void;
}

/**
 * The followed job. Payloads are queued in a ref and folded in on a timer,
 * so a burst of pages costs one render, not one per page.
 */
function useLiveJob(): LiveHandle {
  const [job, setJob] = useState<LiveJob | null>(null);
  const queue = useRef<BackupEventsEvent[]>([]);

  useEffect(() => {
    const unsubscribe = subscribeBackupEvents((payload) => {
      queue.current.push(payload);
    });
    const timer = window.setInterval(() => {
      if (queue.current.length === 0) return;
      const pending = queue.current;
      queue.current = [];
      setJob((current) => (current === null ? current : pending.reduce(applyEvents, current)));
    }, FLUSH_MS);
    return () => {
      unsubscribe();
      window.clearInterval(timer);
    };
  }, []);

  const follow = useCallback((dest: string, summary: BackupJobSummary, options: { watch?: boolean } = {}) => {
    setJob(liveFromSummary(dest, summary));
    if (options.watch !== false && summary.unfinished) {
      // Idempotent on the backend; a failure only means no new events, and
      // the panel already shows the summary's state.
      void backupWatch(dest, summary.spec.id, summary.state.last_seq).catch(() => undefined);
    }
  }, []);

  const replace = useCallback((dest: string, summary: BackupJobSummary) => {
    setJob((current) => {
      const fresh = liveFromSummary(dest, summary);
      if (current === null || current.jobId !== fresh.jobId || current.dest !== dest) return fresh;
      // Keep the log and the rate window; the summary is the newer truth for the rest.
      return { ...fresh, lastSeq: Math.max(current.lastSeq, fresh.lastSeq), recent: current.recent, samples: current.samples };
    });
  }, []);

  const close = useCallback(() => setJob(null), []);

  return useMemo(() => ({ job, follow, replace, close }), [job, follow, replace, close]);
}

function LivePanel({
  live,
  onError,
  onChanged,
}: {
  live: LiveHandle;
  onError: (message: string) => void;
  onChanged: () => void;
}) {
  const job = live.job;
  const [acting, setActing] = useState<BackupAction | null>(null);
  if (job === null) return null;

  const rate = throughput(job.samples);
  const eta = roundEta(job, rate);
  const waiting = job.phase === "waiting_for_mount";
  const terminal = isTerminal(job.phase);

  const act = async (action: BackupAction) => {
    setActing(action);
    try {
      const outcome = await backupControl(job.dest, action, action === "shutdown" ? null : job.jobId);
      if (outcome.job !== null) live.replace(job.dest, outcome.job);
    } catch (cause) {
      onError(message(cause));
    } finally {
      setActing(null);
      onChanged();
    }
  };

  const percent = job.itemTotal > 0 ? Math.min(100, (job.itemIndex / job.itemTotal) * 100) : 0;

  return (
    <section aria-label="Running backup" className="mb-4 rounded border border-border/60 p-3">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className={cn("flex items-center gap-1.5 text-sm font-medium", TONE_TEXT[phaseTone(job.phase)])}>
          {!terminal && !waiting && job.phase !== "paused" && <Loader2 aria-hidden className="size-3.5 animate-spin" />}
          {job.phase === null ? "Unknown" : PHASE_LABELS[job.phase]}
        </span>
        <span className="text-xs text-muted-foreground">
          {job.label ?? "—"} · round {job.round} · <span className="font-mono">{job.jobId}</span> on {job.dest}
        </span>
        {!job.connected && !terminal && (
          <span className="text-xs text-muted-foreground" title="The panel keeps following the drive's own log">
            · service not answering, reading the drive's log
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          {job.phase === "paused" || terminal ? (
            <Button size="sm" variant="outline" disabled={acting !== null || job.phase === "done"} onClick={() => void act("resume")}>
              <CirclePlay aria-hidden />
              Resume
            </Button>
          ) : (
            <Button size="sm" variant="outline" disabled={acting !== null} onClick={() => void act("pause")}>
              <CirclePause aria-hidden />
              Pause
            </Button>
          )}
          <Button size="sm" variant="outline" disabled={acting !== null || terminal} onClick={() => void act("cancel")}>
            <CircleStop aria-hidden />
            Cancel
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={acting !== null || !job.connected}
            onClick={() => void act("shutdown")}
            title="Finish the current file, record the job as stopped, and stop the drive's service. The next service resumes it."
          >
            <Power aria-hidden />
            Stop service
          </Button>
          <Button size="icon" variant="ghost" className="size-8" onClick={live.close} aria-label="Close the job panel">
            <X aria-hidden />
          </Button>
        </div>
      </div>

      {waiting && (
        <Alert className="mt-3">
          <HardDrive aria-hidden />
          <AlertTitle>Waiting for the drive</AlertTitle>
          <AlertDescription>
            {job.detail ?? "A source or the destination is not mounted."} The job resumes by itself when it is
            back; nothing is lost.
          </AlertDescription>
        </Alert>
      )}
      {!waiting && job.detail !== null && (job.phase === "failed" || job.phase === "stopped" || job.phase === "incomplete") && (
        <p className="mt-2 text-xs text-muted-foreground">{job.detail}</p>
      )}

      <div
        role="progressbar"
        aria-label="Items in this round"
        aria-valuemin={0}
        aria-valuemax={job.itemTotal}
        aria-valuenow={job.itemIndex}
        className="mt-3 h-1.5 w-full overflow-hidden rounded-full bg-capacity-free"
      >
        <div className="h-full bg-brand transition-[width] duration-300 motion-reduce:transition-none" style={{ width: `${percent}%` }} />
      </div>

      <dl className="mt-3 grid grid-cols-2 gap-2 text-xs sm:grid-cols-4 lg:grid-cols-8">
        <Stat label="Landed" value={formatSI(job.landedBytes)} />
        <Stat label="Rate" value={formatRate(rate.bytesPerSecond)} />
        <Stat label="Round ETA" value={formatEta(eta)} />
        <Stat
          label="This round"
          value={job.itemTotal > 0 ? `${job.itemIndex.toLocaleString()} / ${job.itemTotal.toLocaleString()}` : "—"}
        />
        <Stat label="Copied" value={job.copied.toLocaleString()} />
        <Stat label="Cloned" value={job.cloned.toLocaleString()} />
        <Stat label="Changed" value={job.changed.toLocaleString()} tone={job.changed > 0 ? "warn" : undefined} />
        <Stat label="Failed" value={job.failed.toLocaleString()} tone={job.failed > 0 ? "bad" : undefined} />
      </dl>
      {job.lastItem !== null && !terminal && (
        <p className="mt-2 truncate font-mono text-[11px] text-muted-foreground" title={job.lastItem}>
          {job.lastItem}
        </p>
      )}

      {job.recent.length > 0 && (
        <details className="mt-2 text-xs">
          <summary className="cursor-pointer text-muted-foreground">
            Events worth reading ({job.recent.length}
            {job.recent.length >= 200 ? ", last 200" : ""})
          </summary>
          <ol className="mt-1 max-h-48 overflow-auto rounded border border-border/60">
            {job.recent
              .slice()
              .reverse()
              .map((line) => (
                <li key={line.seq} className="flex gap-2 border-b border-border/40 px-2 py-0.5 last:border-b-0">
                  <span className="rds-numeric shrink-0 whitespace-nowrap text-muted-foreground">
                    {new Date(line.ts).toLocaleTimeString()}
                  </span>
                  <span className={cn("min-w-0 break-words", line.tone === "ok" || line.tone === "idle" ? "" : TONE_TEXT[line.tone])}>
                    {line.text}
                  </span>
                </li>
              ))}
          </ol>
        </details>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

function DriveRow({
  drive,
  selected,
  onSelect,
  onStart,
  onFollow,
  followingId,
}: {
  drive: BackupDrive;
  selected: boolean;
  onSelect: () => void;
  onStart: () => void;
  onFollow: (job: BackupJobSummary) => void;
  followingId: string | null;
}) {
  const state = SERVICE_STATES[drive.service];
  const [starting, setStarting] = useState(false);
  const unfinished = drive.jobs.filter((job) => job.unfinished);

  return (
    <li className={cn("rounded border p-2 text-xs", selected ? "border-brand/60" : "border-border/60")}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <button
          type="button"
          onClick={onSelect}
          aria-pressed={selected}
          className="flex items-center gap-1.5 rounded text-sm font-medium hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <HardDrive aria-hidden className="size-3.5 text-muted-foreground" />
          {drive.name}
        </button>
        <span className="font-mono text-muted-foreground">{drive.dest}</span>
        <span className={cn("flex items-center gap-1", TONE_TEXT[state.tone])} title={state.hint}>
          <span aria-hidden className={cn("size-1.5 rounded-full", TONE_DOT[state.tone])} />
          {state.label}
        </span>
        {drive.health !== null && (
          <span className="text-muted-foreground">
            v{drive.health.version} · pid {drive.health.pid}
            {drive.health.stopping ? " · stopping" : ""}
          </span>
        )}
        {drive.health === null && drive.lock !== null && (
          <span className="text-muted-foreground">
            lock: pid {drive.lock.pid} on {drive.lock.host}
          </span>
        )}
        <div className="ml-auto">
          {state.canStart && (
            <Button
              size="sm"
              variant="outline"
              disabled={starting}
              onClick={() => {
                setStarting(true);
                Promise.resolve(onStart()).finally(() => setStarting(false));
              }}
            >
              {starting ? <Loader2 aria-hidden className="animate-spin" /> : <Play aria-hidden />}
              Start service
            </Button>
          )}
        </div>
      </div>
      {drive.service !== "running" && <p className="mt-1 text-muted-foreground">{state.hint}</p>}
      {unfinished.length > 0 && (
        <ul className="mt-2 flex flex-col gap-1">
          {unfinished.map((job) => (
            <li key={job.spec.id} className="flex flex-wrap items-center gap-x-2">
              <span className={cn("flex items-center gap-1", TONE_TEXT[phaseTone(job.state.phase)])}>
                <span aria-hidden className={cn("size-1.5 rounded-full", TONE_DOT[phaseTone(job.state.phase)])} />
                {PHASE_LABELS[job.state.phase]}
              </span>
              <span className="font-mono">{job.spec.id}</span>
              {job.spec.profile && <span className="text-muted-foreground">profile {job.spec.profile}</span>}
              <span className="text-muted-foreground">
                {job.state.label ?? ""} round {job.state.round}
              </span>
              {followingId !== job.spec.id && (
                <Button size="sm" variant="ghost" className="h-6 px-2" onClick={() => onFollow(job)}>
                  Show
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

function newProfile(dest: string): BackupProfile {
  return {
    schema_version: PROFILE_SCHEMA_VERSION,
    name: "",
    dest,
    rules: null,
    floor_allocated: DEFAULT_FLOOR_ALLOCATED,
    review_cap: DEFAULT_REVIEW_CAP,
    sources: [],
  };
}

function ProfileList({
  dest,
  busyJob,
  onEdit,
  onPreview,
  onStart,
}: {
  dest: string;
  busyJob: string | null;
  onEdit: (profile: BackupProfile) => void;
  onPreview: (name: string) => void;
  onStart: (name: string) => void;
}) {
  const profiles = useQuery({ queryKey: profilesKey(dest), queryFn: () => backupProfiles(dest) });
  const [checks, setChecks] = useState<Record<string, BackupProfileValidation | "checking">>({});
  const [starting, setStarting] = useState<string | null>(null);

  const validate = async (name: string) => {
    setChecks((current) => ({ ...current, [name]: "checking" }));
    try {
      const result = await backupProfileValidate(dest, name);
      setChecks((current) => ({ ...current, [name]: result }));
    } catch (cause) {
      setChecks((current) => ({ ...current, [name]: { ok: false, error: message(cause), check: null } }));
    }
  };

  if (profiles.isPending) return <Muted>Reading profiles…</Muted>;
  if (profiles.isError) return <Muted>{message(profiles.error)}</Muted>;
  if (profiles.data.length === 0) return <Muted>No profiles on this drive yet.</Muted>;

  return (
    <ul className="flex flex-col gap-2">
      {profiles.data.map((entry) => {
        const check = checks[entry.name];
        return (
          <li key={entry.name} className="rounded border border-border/60 p-2 text-xs">
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
              <span className="text-sm font-medium">{entry.name}</span>
              {entry.profile !== null && (
                <span className="min-w-0 truncate text-muted-foreground">
                  {entry.profile.sources.map((source) => sourceSummary(source)).join(", ")} · floor{" "}
                  {formatSizeExact(entry.profile.floor_allocated)}
                </span>
              )}
              <div className="ml-auto flex items-center gap-1">
                <Button size="sm" variant="ghost" disabled={entry.profile === null} onClick={() => entry.profile && onEdit(entry.profile)}>
                  <Pencil aria-hidden />
                  Edit
                </Button>
                <Button size="sm" variant="ghost" disabled={check === "checking"} onClick={() => void validate(entry.name)}>
                  {check === "checking" ? <Loader2 aria-hidden className="animate-spin" /> : <ListChecks aria-hidden />}
                  Check
                </Button>
                <Button size="sm" variant="outline" disabled={entry.profile === null} onClick={() => onPreview(entry.name)}>
                  <SearchCheck aria-hidden />
                  Preview
                </Button>
                <Button
                  size="sm"
                  disabled={entry.profile === null || starting !== null || busyJob !== null}
                  title={busyJob !== null ? `Job ${busyJob} is not finished on this drive` : undefined}
                  onClick={() => {
                    setStarting(entry.name);
                    Promise.resolve(onStart(entry.name)).finally(() => setStarting(null));
                  }}
                >
                  {starting === entry.name ? <Loader2 aria-hidden className="animate-spin" /> : <Play aria-hidden />}
                  Start backup
                </Button>
              </div>
            </div>
            {entry.error !== null && <p className="mt-1 text-pressure-critical">{entry.error}</p>}
            {check !== undefined && check !== "checking" && (
              <p className={cn("mt-1", check.ok ? "text-pressure-ok" : "text-pressure-critical")} role="status">
                {check.ok ? "Checked against the disks: ready to run." : check.error}
              </p>
            )}
          </li>
        );
      })}
    </ul>
  );
}

function sourceSummary(source: BackupProfileSource): string {
  if (source.include.length === 0) return `${source.label} (${source.root})`;
  return `${source.label} (${source.include.length} item${source.include.length === 1 ? "" : "s"} in ${source.root})`;
}

function ProfileEditor({
  initial,
  volumes,
  onCancel,
  onSaved,
}: {
  initial: BackupProfile;
  volumes: readonly VolumeRow[];
  onCancel: () => void;
  onSaved: (profile: BackupProfile) => void;
}) {
  const [draft, setDraft] = useState<BackupProfile>(initial);
  const [floorText, setFloorText] = useState(formatSizeExact(initial.floor_allocated));
  const [saving, setSaving] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);
  const isNew = initial.name === "";

  const userVolumes = volumes.filter((volume) => !volume.isSystem);
  const destinations = userVolumes.filter((volume) => volume.mountPoint !== "/");
  const diskOf = useCallback(
    (mount: string) => volumes.find((volume) => volume.mountPoint === mount)?.diskId ?? null,
    [volumes],
  );

  const floor = parseSize(floorText);
  const candidate: BackupProfile = { ...draft, floor_allocated: floor ?? Number.NaN };
  const problems = draftProblems(candidate, diskOf);
  const problemFor = (field: string) => problems.filter((problem) => problem.field === field);
  const visible = (field: string): DraftProblem[] => (touched ? problemFor(field) : []);

  const toggleSource = (root: string, on: boolean) => {
    setDraft((current) => ({
      ...current,
      sources: on
        ? [...current.sources, { label: uniqueLabel(labelFor(root), current.sources), root, include: [] }]
        : current.sources.filter((source) => source.root !== root),
    }));
  };

  const updateSource = (index: number, change: Partial<BackupProfileSource>) => {
    setDraft((current) => ({
      ...current,
      sources: current.sources.map((source, at) => (at === index ? { ...source, ...change } : source)),
    }));
  };

  const addIncludes = async (index: number, directory: boolean) => {
    const source = draft.sources[index];
    if (source === undefined) return;
    const chosen = await openDialog({
      title: directory ? `Folders to back up from ${source.root}` : `Files to back up from ${source.root}`,
      directory,
      multiple: true,
      defaultPath: source.root,
    });
    if (chosen === null) return;
    const paths = (Array.isArray(chosen) ? chosen : [chosen]).map((path) => relativeInclude(source.root, path));
    updateSource(index, { include: [...new Set([...source.include, ...paths])] });
  };

  const save = async () => {
    setTouched(true);
    setServerError(null);
    if (problems.length > 0 || floor === null) return;
    setSaving(true);
    try {
      await backupProfileSave(candidate);
      const check = await backupProfileValidate(candidate.dest, candidate.name);
      if (!check.ok) {
        setServerError(`Saved, but it will not run yet: ${check.error ?? "the check gave no reason"}`);
        return;
      }
      onSaved(candidate);
    } catch (cause) {
      setServerError(message(cause));
    } finally {
      setSaving(false);
    }
  };

  return (
    <form
      aria-label={isNew ? "New backup profile" : `Edit profile ${initial.name}`}
      className="mb-3 flex flex-col gap-3 rounded border border-border/60 p-3 text-xs"
      onSubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      <div className="flex flex-wrap items-start gap-4">
        <Field label="Name" htmlFor="backup-name" problems={visible("name")}>
          <input
            id="backup-name"
            className={INPUT}
            value={draft.name}
            readOnly={!isNew}
            placeholder="nightly"
            onChange={(event) => setDraft({ ...draft, name: event.target.value })}
          />
        </Field>
        <Field label="Destination drive" htmlFor="backup-dest" problems={visible("dest")}>
          <select
            id="backup-dest"
            className={INPUT}
            value={draft.dest}
            disabled={!isNew}
            onChange={(event) => setDraft({ ...draft, dest: event.target.value })}
          >
            <option value="">Choose a drive…</option>
            {destinations.map((volume) => (
              <option key={volume.mountPoint} value={volume.mountPoint}>
                {volume.name} — {formatSI(volume.availableBytes)} free
                {volume.diskId ? ` (${volume.diskId})` : ""}
              </option>
            ))}
            {draft.dest !== "" && !destinations.some((volume) => volume.mountPoint === draft.dest) && (
              <option value={draft.dest}>{draft.dest}</option>
            )}
          </select>
        </Field>
        <Field label="Smallest file" htmlFor="backup-floor" problems={visible("floor")} hint="Files below this size are left out.">
          <input
            id="backup-floor"
            className={cn(INPUT, "w-28")}
            value={floorText}
            onChange={(event) => setFloorText(event.target.value)}
          />
        </Field>
        <Field label="Review cap" htmlFor="backup-cap" problems={visible("cap")} hint="Largest candidates ranked per source.">
          <input
            id="backup-cap"
            type="number"
            min={1}
            className={cn(INPUT, "w-28")}
            value={Number.isFinite(draft.review_cap) ? draft.review_cap : ""}
            onChange={(event) => setDraft({ ...draft, review_cap: Number.parseInt(event.target.value, 10) })}
          />
        </Field>
      </div>

      <fieldset className="flex flex-col gap-1">
        <legend className="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">Source volumes</legend>
        {userVolumes.length === 0 && <Muted>No volumes listed.</Muted>}
        {userVolumes.map((volume) => {
          const index = draft.sources.findIndex((source) => source.root === volume.mountPoint);
          const source = draft.sources[index];
          const isDest = volume.mountPoint === draft.dest;
          const sameDisk = !isDest && volume.diskId !== null && volume.diskId === diskOf(draft.dest);
          return (
            <div key={volume.mountPoint} className="rounded border border-border/40 px-2 py-1.5">
              <label className={cn("flex items-center gap-2", isDest && "opacity-50")}>
                <input
                  type="checkbox"
                  checked={source !== undefined}
                  disabled={isDest}
                  onChange={(event) => toggleSource(volume.mountPoint, event.target.checked)}
                />
                <span className="font-medium">{volume.name}</span>
                <span className="font-mono text-muted-foreground">{volume.mountPoint}</span>
                <span className="text-muted-foreground">
                  {formatSI(volume.usedBytes)} used{volume.diskId ? ` · ${volume.diskId}` : ""}
                  {isDest ? " · the destination" : ""}
                </span>
                {sameDisk && <span className="text-pressure-warn">same disk as the destination</span>}
              </label>
              {source !== undefined && (
                <div className="mt-1.5 flex flex-col gap-1.5 pl-6">
                  <div className="flex flex-wrap items-center gap-2">
                    <label className="flex items-center gap-1.5">
                      <span className="text-muted-foreground">Label</span>
                      <input
                        className={cn(INPUT, "w-40")}
                        value={source.label}
                        onChange={(event) => updateSource(index, { label: event.target.value })}
                        aria-label={`Label for ${volume.name}`}
                      />
                    </label>
                    <span className="text-muted-foreground">
                      {source.include.length === 0 ? "Whole volume" : `${source.include.length} included`}
                    </span>
                    <Button type="button" size="sm" variant="ghost" className="h-7 px-2" onClick={() => void addIncludes(index, true)}>
                      <FolderPlus aria-hidden />
                      Folders
                    </Button>
                    <Button type="button" size="sm" variant="ghost" className="h-7 px-2" onClick={() => void addIncludes(index, false)}>
                      <FilePlus aria-hidden />
                      Files
                    </Button>
                  </div>
                  {source.include.length > 0 && (
                    <ul className="flex flex-col">
                      {source.include.map((include) => (
                        <li key={include} className="flex items-center gap-1">
                          <span className="min-w-0 truncate font-mono" title={include}>
                            {include}
                          </span>
                          <button
                            type="button"
                            aria-label={`Remove ${include}`}
                            className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                            onClick={() => updateSource(index, { include: source.include.filter((item) => item !== include) })}
                          >
                            <X aria-hidden className="size-3" />
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                  <Problems problems={problemFor(`source:${index}`)} />
                </div>
              )}
            </div>
          );
        })}
        <Problems problems={visible("sources")} />
      </fieldset>

      {serverError !== null && (
        <p role="alert" className="text-pressure-critical">
          {serverError}
        </p>
      )}

      <div className="flex items-center gap-2">
        <Button type="submit" size="sm" disabled={saving}>
          {saving && <Loader2 aria-hidden className="animate-spin" />}
          {saving ? "Saving and checking…" : "Save profile"}
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <span className="text-muted-foreground">Saved to the drive, then checked against the disks.</span>
      </div>
    </form>
  );
}

function uniqueLabel(base: string, sources: readonly BackupProfileSource[]): string {
  const taken = new Set(sources.map((source) => source.label));
  if (!taken.has(base)) return base;
  for (let n = 2; ; n += 1) {
    const label = `${base.slice(0, 60)}-${n}`;
    if (!taken.has(label)) return label;
  }
}

const INPUT =
  "h-8 rounded-md border border-border bg-transparent px-2 text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring read-only:text-muted-foreground disabled:opacity-60";

function Field({
  label,
  htmlFor,
  problems,
  hint,
  children,
}: {
  label: string;
  htmlFor: string;
  problems: readonly DraftProblem[];
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={htmlFor} className="text-[10px] uppercase tracking-wide text-muted-foreground">
        {label}
      </label>
      {children}
      {problems.length > 0 ? <Problems problems={problems} /> : hint && <span className="text-muted-foreground">{hint}</span>}
    </div>
  );
}

function Problems({ problems }: { problems: readonly DraftProblem[] }) {
  if (problems.length === 0) return null;
  return (
    <ul className="flex flex-col text-pressure-critical">
      {problems.map((problem) => (
        <li key={problem.message}>{problem.message}</li>
      ))}
    </ul>
  );
}

// ---------------------------------------------------------------------------
// Recommended preview
// ---------------------------------------------------------------------------

type PreviewState =
  | { readonly kind: "running"; readonly since: number }
  | { readonly kind: "done"; readonly result: BackupRecommendation }
  | { readonly kind: "error"; readonly message: string }
  | { readonly kind: "abandoned" };

function Preview({ dest, name, onClose }: { dest: string; name: string; onClose: () => void }) {
  const [state, setState] = useState<PreviewState>({ kind: "running", since: Date.now() });
  const [attempt, setAttempt] = useState(0);
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    let current = true;
    setState({ kind: "running", since: Date.now() });
    backupRecommend(dest, name).then(
      (result) => current && setState({ kind: "done", result }),
      (cause: unknown) => current && setState({ kind: "error", message: message(cause) }),
    );
    return () => {
      current = false;
    };
  }, [dest, name, attempt]);

  useEffect(() => {
    if (state.kind !== "running") return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [state.kind]);

  return (
    <Section
      title={`Recommended for ${name}`}
      action={
        <Button size="icon" variant="ghost" className="size-8" onClick={onClose} aria-label="Close the preview">
          <X aria-hidden />
        </Button>
      }
    >
      {state.kind === "running" && (
        <div className="flex items-center gap-3 text-xs" role="status">
          <Loader2 aria-hidden className="size-4 animate-spin text-muted-foreground" />
          <span>
            Walking the sources… {formatDuration(now - state.since)}. A large volume takes minutes.
          </span>
          <Button size="sm" variant="ghost" onClick={() => setState({ kind: "abandoned" })}>
            Stop waiting
          </Button>
        </div>
      )}
      {state.kind === "abandoned" && (
        <div className="flex items-center gap-3 text-xs">
          <Muted>Not waiting for this preview. The walk already started finishes in the background.</Muted>
          <Button size="sm" variant="outline" onClick={() => setAttempt((n) => n + 1)}>
            Run again
          </Button>
        </div>
      )}
      {state.kind === "error" && (
        <div className="flex items-center gap-3 text-xs">
          <p className="text-pressure-critical">{state.message}</p>
          <Button size="sm" variant="outline" onClick={() => setAttempt((n) => n + 1)}>
            Try again
          </Button>
        </div>
      )}
      {state.kind === "done" && <Recommendation result={state.result} />}
    </Section>
  );
}

const STATUS_TONE: Record<Row_Serialize["status"], string> = {
  planned: "text-pressure-ok",
  deferred: "text-pressure-warn",
  skipped: "text-muted-foreground",
};

function Recommendation({ result }: { result: BackupRecommendation }) {
  const [filter, setFilter] = useState<Row_Serialize["status"] | "all">("all");
  const { totals } = result;
  const rows = filter === "all" ? result.rows : result.rows.filter((row) => row.status === filter);
  const shown = rows.slice(0, PREVIEW_ROWS);

  return (
    <div className="flex flex-col gap-3 text-xs">
      <FillMeter result={result} />

      <dl className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <Stat label="Planned" value={totals.planned_items.toLocaleString()} note={formatSI(totals.planned_logical)} tone="ok" />
        <Stat label="Deferred" value={totals.deferred_items.toLocaleString()} note={formatSI(totals.deferred_logical)} tone={totals.deferred_items > 0 ? "warn" : undefined} />
        <Stat label="Skipped by rule" value={totals.skipped_items.toLocaleString()} note={formatSI(totals.skipped_allocated)} />
        <Stat label="Not considered" value={totals.not_considered_items.toLocaleString()} note={formatSI(totals.not_considered_allocated)} />
      </dl>

      {result.warnings.length > 0 && (
        <ul className="flex flex-col gap-0.5 text-pressure-warn">
          {result.warnings.map((warning) => (
            <li key={`${warning.code}:${warning.text}`}>{warning.text}</li>
          ))}
        </ul>
      )}

      <div className="flex items-center gap-2" role="group" aria-label="Show rows">
        {(["all", "planned", "deferred", "skipped"] as const).map((value) => (
          <button
            key={value}
            type="button"
            aria-pressed={filter === value}
            onClick={() => setFilter(value)}
            className={cn(
              "rounded px-2 py-0.5 capitalize focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
              filter === value ? "bg-accent font-medium text-accent-foreground" : "text-muted-foreground hover:bg-accent",
            )}
          >
            {value}
          </button>
        ))}
        <span className="ml-auto text-muted-foreground">
          {result.rows_truncated
            ? `First ${result.rows.length.toLocaleString()} of ${result.rows_total.toLocaleString()} rows; totals cover all.`
            : `${result.rows_total.toLocaleString()} rows`}
          {rows.length > PREVIEW_ROWS ? `, showing ${PREVIEW_ROWS}` : ""}
        </span>
      </div>

      <div className="max-h-[28rem] overflow-auto rounded border border-border/60">
        <table className="w-full border-collapse">
          <thead className="sticky top-0 bg-background">
            <tr className="border-b border-border/60 text-left font-medium">
              <th scope="col" className="px-2 py-1 text-right">#</th>
              <th scope="col" className="px-2 py-1">Source</th>
              <th scope="col" className="px-2 py-1">Path</th>
              <th scope="col" className="px-2 py-1">Status</th>
              <th scope="col" className="px-2 py-1 text-right">Size</th>
              <th scope="col" className="px-2 py-1">Why skipped</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((row) => (
              <tr key={`${row.source_label}\u0000${row.relative}`} className="border-b border-border/40 last:border-b-0">
                <td className="rds-numeric px-2 py-0.5 text-right text-muted-foreground">{row.rank}</td>
                <td className="px-2 py-0.5">{row.source_label}</td>
                <td className="max-w-[28rem] truncate px-2 py-0.5 font-mono" title={row.relative}>
                  {row.relative}
                  {row.rollup_files ? <span className="ml-1 text-muted-foreground">({row.rollup_files.toLocaleString()} files)</span> : null}
                </td>
                <td className={cn("px-2 py-0.5", STATUS_TONE[row.status])}>{row.status}</td>
                <td className="rds-numeric px-2 py-0.5 text-right">{formatSI(row.allocated)}</td>
                <td className="px-2 py-0.5 text-muted-foreground">
                  {row.status === "skipped" && (
                    <>
                      {row.reason}
                      {row.rule_id && <span className="ml-1 font-mono text-[10px]">{row.rule_id}</span>}
                    </>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function FillMeter({ result }: { result: BackupRecommendation }) {
  const bar = fillBar(result.fill);
  const { fill } = result;
  const limit = `${Math.round(bar.limit)}%`;
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex flex-wrap items-baseline gap-x-3">
        <span className={cn("font-medium", bar.fits ? "text-pressure-ok" : "text-pressure-warn")}>
          {bar.fits ? `Fits under the ${limit} fill limit` : `Does not fit under the ${limit} fill limit`}
        </span>
        <span className="text-muted-foreground">
          {bar.fits
            ? `After this run the drive is ${Math.round(fill.projected_fill_wanted * 100)}% full.`
            : `This run lands the planned rows (${Math.round(fill.projected_fill_planned * 100)}% full) and defers the rest; ${formatSI(bar.overBy)} over.`}
        </span>
      </div>
      <div
        role="meter"
        aria-label="Destination fill"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(bar.used + bar.planned)}
        aria-valuetext={`${formatSI(fill.used_bytes)} used, ${formatSI(fill.planned_logical)} planned, ${formatSI(
          Math.max(0, fill.wanted_logical - fill.planned_logical),
        )} deferred, of ${formatSI(fill.total_bytes)}; limit ${limit}`}
        className="relative flex h-2.5 w-full overflow-hidden rounded-full bg-capacity-free"
      >
        <div className="h-full bg-muted-foreground/50" style={{ width: `${bar.used}%` }} />
        <div className="h-full bg-brand" style={{ width: `${bar.planned}%` }} />
        <div className="h-full bg-pressure-warn/60" style={{ width: `${bar.deferred}%` }} />
        <div aria-hidden className="absolute inset-y-0 w-0.5 bg-foreground" style={{ left: `${bar.limit}%` }} />
      </div>
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-muted-foreground">
        <Swatch className="bg-muted-foreground/50">used {formatSI(fill.used_bytes)}</Swatch>
        <Swatch className="bg-brand">this run {formatSI(fill.planned_logical)}</Swatch>
        {bar.deferred > 0 && (
          <Swatch className="bg-pressure-warn/60">deferred {formatSI(fill.wanted_logical - fill.planned_logical)}</Swatch>
        )}
        <Swatch className="bg-foreground">limit {limit} of {formatSI(fill.total_bytes)}</Swatch>
      </div>
    </div>
  );
}

function Swatch({ className, children }: { className: string; children: React.ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span aria-hidden className={cn("size-2 rounded-[2px]", className)} />
      {children}
    </span>
  );
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

function History({ drive }: { drive: BackupDrive }) {
  const index = useQuery({
    queryKey: indexKey(drive.dest),
    queryFn: async () => parseIndexStatus(await backupIndexStatus(drive.dest)),
    staleTime: 60_000,
    retry: false,
  });
  const finished = drive.jobs
    .filter((job) => !job.unfinished)
    .sort((a, b) => (b.state.finished_at_ms ?? b.state.updated_at_ms) - (a.state.finished_at_ms ?? a.state.updated_at_ms))
    .slice(0, 20);

  return (
    <Section title={`What ${drive.name} holds`}>
      {index.isPending ? (
        <Muted>Reading the drive's index…</Muted>
      ) : index.isError ? (
        <Muted>{message(index.error)}</Muted>
      ) : index.data.sources.length === 0 ? (
        <Muted>The index lists no sources yet.</Muted>
      ) : (
        <div className="overflow-auto rounded border border-border/60 text-xs">
          <table className="w-full border-collapse">
            <thead>
              <tr className="border-b border-border/60 text-left font-medium">
                <th scope="col" className="px-2 py-1">Source</th>
                <th scope="col" className="px-2 py-1 text-right">Backed up</th>
                <th scope="col" className="px-2 py-1 text-right">As clones</th>
                <th scope="col" className="px-2 py-1 text-right">Pending</th>
                <th scope="col" className="px-2 py-1 text-right">Skipped</th>
                <th scope="col" className="px-2 py-1 text-right">Over the floor, not backed up</th>
                <th scope="col" className="px-2 py-1">Last plan</th>
              </tr>
            </thead>
            <tbody>
              {index.data.sources.map((source) => (
                <tr key={source.label} className="border-b border-border/40 last:border-b-0">
                  <td className="px-2 py-1 font-medium">{source.label}</td>
                  <TallyCell tally={source.backedUp} />
                  <TallyCell tally={source.cloned} />
                  <TallyCell tally={source.pending} warn={source.pending.items > 0} />
                  <TallyCell tally={source.skipped} />
                  {source.notBackedUp === null ? (
                    <td className="px-2 py-1 text-right text-muted-foreground">—</td>
                  ) : (
                    <TallyCell tally={source.notBackedUp} warn={source.notBackedUp.items > 0} />
                  )}
                  <td className="px-2 py-1 font-mono text-muted-foreground">{source.lastPlan ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {index.data.warnings.length > 0 && (
            <ul className="border-t border-border/60 px-2 py-1 text-pressure-warn">
              {index.data.warnings.map((warning) => (
                <li key={warning}>{warning}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      {finished.length > 0 && (
        <ul className="mt-3 flex flex-col gap-0.5 text-xs">
          {finished.map((job) => {
            const landed = (job.state.counts.copied?.bytes ?? 0) + (job.state.counts.cloned?.bytes ?? 0);
            const when = job.state.finished_at_ms ?? job.state.updated_at_ms;
            return (
              <li key={job.spec.id} className="flex flex-wrap items-center gap-x-3">
                <span className={cn("w-24", TONE_TEXT[phaseTone(job.state.phase)])}>{PHASE_LABELS[job.state.phase]}</span>
                <span className="rds-numeric w-40 text-muted-foreground">{new Date(when).toLocaleString()}</span>
                <span className="font-mono">{job.spec.id}</span>
                {job.spec.profile && <span className="text-muted-foreground">{job.spec.profile}</span>}
                <span className="rds-numeric text-muted-foreground">{formatSI(landed)} landed</span>
                {(job.state.counts.failed?.items ?? 0) > 0 && (
                  <span className="text-pressure-critical">{job.state.counts.failed?.items.toLocaleString()} failed</span>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}

function TallyCell({ tally, warn = false }: { tally: { items: number; bytes: number }; warn?: boolean }) {
  return (
    <td className={cn("rds-numeric px-2 py-1 text-right", warn && "text-pressure-warn")}>
      {formatSI(tally.bytes)}
      <span className="ml-1.5 text-[10px] text-muted-foreground">{tally.items.toLocaleString()}</span>
    </td>
  );
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

function Section({ title, action, children }: { title: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="mb-5" aria-label={title}>
      <div className="mb-2 flex items-center gap-2">
        <h3 className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">{title}</h3>
        {action !== undefined && <div className="ml-auto">{action}</div>}
      </div>
      {children}
    </section>
  );
}

function Muted({ children }: { children: React.ReactNode }) {
  return <p className="text-xs text-muted-foreground">{children}</p>;
}

function Stat({ label, value, note, tone }: { label: string; value: string; note?: string; tone?: Tone }) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5">
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className={cn("rds-numeric truncate text-sm", tone !== undefined && TONE_TEXT[tone])}>
        {value}
        {note !== undefined && <span className="ml-1.5 text-[10px] text-muted-foreground">{note}</span>}
      </dd>
    </div>
  );
}
