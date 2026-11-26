/**
 * The arithmetic behind TRANSFER > Backup, kept free of React and Tauri so
 * `node --test` can run it (see `backup.test.ts`).
 *
 * Three things live here:
 *
 * - **The live job reducer.** A big backup emits one `item` event per file;
 *   rendering each would make the page the bottleneck of the copy it watches.
 *   So events are folded into counters, and the only list kept is the last
 *   {@link RECENT_LIMIT} events that are worth reading: phase changes, rounds,
 *   errors, and items that did not land. Copied and cloned files only move
 *   counters and the "last item" line.
 * - **The fill bar.** What the drive holds now, what this run adds, what the
 *   profile wants beyond that, against the 75% fill limit.
 * - **Draft checks** that need no filesystem, mirroring `Profile::validate`
 *   in `rdirstat-organize`, plus the same-physical-disk refusal when the
 *   volume list already says so. The backend still checks everything again;
 *   these exist so a mistake shows next to the field, before a save.
 */

import type {
  BackupEventsEvent,
  BackupJobSummary,
  BackupProfile,
  DestFill,
  Event,
  ItemOutcome,
  Phase,
  ServiceState,
} from "./bindings.ts";
import { formatSI } from "./format.ts";

// ---------------------------------------------------------------------------
// Service and phase vocabulary
// ---------------------------------------------------------------------------

export type Tone = "ok" | "warn" | "bad" | "idle";

export interface ServiceDescription {
  readonly label: string;
  readonly tone: Tone;
  /** What it means and what to do, one sentence. */
  readonly hint: string;
  /** Start service is the right next step. */
  readonly canStart: boolean;
}

export const SERVICE_STATES: Record<ServiceState, ServiceDescription> = {
  running: {
    label: "Running",
    tone: "ok",
    hint: "The drive's backup service answers.",
    canStart: false,
  },
  stopped: {
    label: "Stopped",
    tone: "idle",
    hint: "No service runs for this drive. Starting one resumes its unfinished jobs.",
    canStart: true,
  },
  stale: {
    label: "Stale",
    tone: "warn",
    hint: "The last service was killed or crashed. Starting a new one takes over its lock and resumes.",
    canStart: true,
  },
  incompatible: {
    label: "Incompatible",
    tone: "bad",
    hint: "A service of another version answers. Stop it once its job is done, then start it from here.",
    canStart: false,
  },
  not_answering: {
    label: "Not answering",
    tone: "warn",
    hint: "Its process is alive but does not answer. It may be busy, or run on another Mac.",
    canStart: false,
  },
};

export const PHASE_LABELS: Record<Phase, string> = {
  queued: "Queued",
  waiting_for_mount: "Waiting for drive",
  planning: "Planning",
  applying: "Copying",
  paused: "Paused",
  indexing: "Indexing",
  done: "Done",
  incomplete: "Incomplete",
  cancelled: "Cancelled",
  stopped: "Stopped",
  failed: "Failed",
};

const TERMINAL: ReadonlySet<Phase> = new Set<Phase>(["done", "incomplete", "cancelled", "stopped", "failed"]);

/** No more events come unless the job is resumed. */
export function isTerminal(phase: Phase | null): boolean {
  return phase !== null && TERMINAL.has(phase);
}

export function phaseTone(phase: Phase | null): Tone {
  switch (phase) {
    case "done":
      return "ok";
    case "failed":
      return "bad";
    case "waiting_for_mount":
    case "incomplete":
    case "paused":
    case "stopped":
      return "warn";
    case "planning":
    case "applying":
    case "indexing":
    case "queued":
      return "ok";
    default:
      return "idle";
  }
}

// ---------------------------------------------------------------------------
// Live job reducer
// ---------------------------------------------------------------------------

/** The most events the page keeps for its log. */
export const RECENT_LIMIT = 200;

/** Seconds of history the rate is measured over. */
const RATE_WINDOW_MS = 15_000;
/** Most rate samples kept (one per page of events, not per event). */
const MAX_SAMPLES = 64;

export interface RateSample {
  readonly ts: number;
  /** Cumulative landed bytes at `ts`. */
  readonly bytes: number;
  /** Cumulative attempted items at `ts`. */
  readonly items: number;
}

/** One line of the job log. */
export interface LogLine {
  readonly seq: number;
  readonly ts: number;
  readonly tone: Tone;
  readonly text: string;
}

export interface LiveJob {
  readonly dest: string;
  readonly jobId: string;
  readonly phase: Phase | null;
  readonly label: string | null;
  readonly round: number;
  /** Why, for waits, stops and failures. */
  readonly detail: string | null;
  /** The service answered on the last page. */
  readonly connected: boolean;
  readonly finished: boolean;
  /** Highest seq folded in. Events at or below it are ignored. */
  readonly lastSeq: number;
  readonly copied: number;
  readonly cloned: number;
  /** Logical bytes copied or cloned. */
  readonly landedBytes: number;
  readonly alreadyLanded: number;
  readonly failed: number;
  /** Changed since the scan or during the copy. */
  readonly changed: number;
  readonly occupied: number;
  /** Position in the current round's plan. */
  readonly itemIndex: number;
  readonly itemTotal: number;
  readonly lastItem: string | null;
  readonly samples: readonly RateSample[];
  readonly recent: readonly LogLine[];
}

type Counts = BackupJobSummary["state"]["counts"];

function tally(counts: Counts, outcome: ItemOutcome): { items: number; bytes: number } {
  const entry = counts[outcome];
  return { items: entry?.items ?? 0, bytes: entry?.bytes ?? 0 };
}

/** The live view of a job as its summary describes it, before any event. */
export function liveFromSummary(dest: string, summary: BackupJobSummary): LiveJob {
  const { state } = summary;
  const copied = tally(state.counts, "copied");
  const cloned = tally(state.counts, "cloned");
  return {
    dest,
    jobId: summary.spec.id,
    phase: state.phase,
    label: state.label ?? null,
    round: state.round,
    detail: state.last_error ?? null,
    connected: summary.active,
    finished: !summary.unfinished,
    lastSeq: state.last_seq,
    copied: copied.items,
    cloned: cloned.items,
    landedBytes: copied.bytes + cloned.bytes,
    alreadyLanded: tally(state.counts, "already_landed").items,
    failed: tally(state.counts, "failed").items,
    changed: tally(state.counts, "changed_since_scan").items + tally(state.counts, "changed_during_copy").items,
    occupied: tally(state.counts, "destination_occupied").items,
    itemIndex: state.items_done,
    itemTotal: state.items_total,
    lastItem: state.current_item ?? null,
    samples: [],
    recent: [],
  };
}

const ITEM_TEXT: Partial<Record<ItemOutcome, string>> = {
  failed: "Failed",
  changed_since_scan: "Changed since the scan, skipped",
  changed_during_copy: "Changed while copying, not receipted",
  destination_occupied: "Something else is at the destination, left alone",
};

/** The log line an event earns, or `null` for an item that simply landed. */
export function logLine(event: Event): LogLine | null {
  const base = { seq: event.seq, ts: event.ts_ms };
  switch (event.event) {
    case "phase": {
      const where = event.label ? ` · ${event.label} round ${event.round}` : "";
      const why = event.detail ? ` — ${event.detail}` : "";
      return { ...base, tone: phaseTone(event.phase), text: `${PHASE_LABELS[event.phase]}${where}${why}` };
    }
    case "round": {
      const how = event.complete ? "complete" : event.stopped ? `stopped: ${event.stopped}` : "not complete";
      return {
        ...base,
        tone: event.complete ? "ok" : "warn",
        text:
          `${event.label} round ${event.round} ${how} — landed ${event.landed.items.toLocaleString()} ` +
          `(${formatSI(event.landed.bytes)}), already there ${event.already_landed.items.toLocaleString()}, ` +
          `not landed ${event.not_landed.items.toLocaleString()}`,
      };
    }
    case "error":
      return { ...base, tone: "bad", text: event.label ? `${event.label}: ${event.message}` : event.message };
    case "item": {
      const text = ITEM_TEXT[event.outcome];
      if (text === undefined) return null;
      const why = event.detail ? ` — ${event.detail}` : "";
      return { ...base, tone: event.outcome === "failed" ? "bad" : "warn", text: `${text}: ${event.relative}${why}` };
    }
  }
}

type Mutable = {
  -readonly [K in keyof LiveJob]: LiveJob[K];
};

/**
 * Folds one `backup:events` payload into the job. Pure; returns `job`
 * unchanged when the payload is for another job or brings nothing new.
 */
export function applyEvents(job: LiveJob, payload: BackupEventsEvent): LiveJob {
  if (payload.dest !== job.dest || payload.job_id !== job.jobId) return job;
  const fresh = payload.events.filter((event) => event.seq > job.lastSeq);
  if (fresh.length === 0 && payload.connected === job.connected && payload.finished === job.finished) {
    return job;
  }
  const next: Mutable = { ...job, connected: payload.connected, finished: payload.finished };
  const lines: LogLine[] = [];
  for (const event of fresh) {
    next.lastSeq = event.seq;
    switch (event.event) {
      case "phase":
        next.phase = event.phase;
        next.label = event.label ?? next.label;
        next.round = event.round;
        next.detail = event.detail ?? null;
        if (event.phase === "planning") {
          next.itemIndex = 0;
          next.itemTotal = 0;
        }
        break;
      case "item":
        next.label = event.label;
        next.round = event.round;
        next.itemIndex = event.index;
        next.itemTotal = event.total;
        next.lastItem = event.relative;
        switch (event.outcome) {
          case "copied":
            next.copied += 1;
            next.landedBytes += event.bytes;
            break;
          case "cloned":
            next.cloned += 1;
            next.landedBytes += event.bytes;
            break;
          case "already_landed":
            next.alreadyLanded += 1;
            break;
          case "changed_since_scan":
          case "changed_during_copy":
            next.changed += 1;
            break;
          case "destination_occupied":
            next.occupied += 1;
            break;
          case "failed":
            next.failed += 1;
            break;
        }
        break;
      case "round":
      case "error":
        break;
    }
    const line = logLine(event);
    if (line !== null) lines.push(line);
  }
  const newest = fresh.at(-1);
  if (newest !== undefined) {
    next.samples = addSample(job.samples, {
      ts: newest.ts_ms,
      bytes: next.landedBytes,
      items: attempted(next),
    });
  }
  if (lines.length > 0) {
    const merged = job.recent.concat(lines);
    next.recent = merged.length > RECENT_LIMIT ? merged.slice(merged.length - RECENT_LIMIT) : merged;
  }
  return next;
}

function attempted(job: Pick<LiveJob, "copied" | "cloned" | "alreadyLanded" | "failed" | "changed" | "occupied">) {
  return job.copied + job.cloned + job.alreadyLanded + job.failed + job.changed + job.occupied;
}

/** Appends a sample and drops those older than the rate window. */
export function addSample(samples: readonly RateSample[], sample: RateSample): readonly RateSample[] {
  const kept = samples.filter((old) => sample.ts - old.ts <= RATE_WINDOW_MS && old.ts <= sample.ts);
  kept.push(sample);
  return kept.length > MAX_SAMPLES ? kept.slice(kept.length - MAX_SAMPLES) : kept;
}

export interface Throughput {
  /** Landed bytes per second, or `null` until two samples a second apart exist. */
  readonly bytesPerSecond: number | null;
  readonly itemsPerSecond: number | null;
}

export function throughput(samples: readonly RateSample[]): Throughput {
  const first = samples[0];
  const last = samples.at(-1);
  if (first === undefined || last === undefined || last.ts - first.ts < 1000) {
    return { bytesPerSecond: null, itemsPerSecond: null };
  }
  const seconds = (last.ts - first.ts) / 1000;
  return {
    bytesPerSecond: Math.max(0, (last.bytes - first.bytes) / seconds),
    itemsPerSecond: Math.max(0, (last.items - first.items) / seconds),
  };
}

/**
 * Milliseconds left in the current round at the current item rate, or
 * `null` when it cannot be said. Per round and per item, because the
 * events carry no byte total for the plan.
 */
export function roundEta(job: Pick<LiveJob, "itemIndex" | "itemTotal" | "phase">, rate: Throughput): number | null {
  if (job.phase !== "applying") return null;
  const remaining = job.itemTotal - job.itemIndex;
  if (remaining <= 0 || rate.itemsPerSecond === null || rate.itemsPerSecond <= 0) return null;
  return (remaining / rate.itemsPerSecond) * 1000;
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

export function formatRate(bytesPerSecond: number | null): string {
  if (bytesPerSecond === null || !Number.isFinite(bytesPerSecond)) return "—";
  return `${formatSI(Math.round(bytesPerSecond))}/s`;
}

/** `"under a minute"`, `"12 min"`, `"3 h 05 min"`, `"—"` when unknown. */
export function formatEta(millis: number | null): string {
  if (millis === null || !Number.isFinite(millis) || millis < 0) return "—";
  const minutes = Math.round(millis / 60_000);
  if (minutes < 1) return "under a minute";
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  return `${hours} h ${(minutes % 60).toString().padStart(2, "0")} min`;
}

/** Bytes as typed in a floor field: `"100 MB"`, `"1.5G"`, `"104857600"`. */
export function parseSize(text: string): number | null {
  const match = /^\s*(\d+(?:\.\d+)?)\s*([kmgt]?)(i?)b?\s*$/iu.exec(text);
  if (match === null) return null;
  const [, digits = "", unit = "", binary = ""] = match;
  const powers: Record<string, number> = { "": 0, k: 1, m: 2, g: 3, t: 4 };
  const base = binary === "" ? 1000 : 1024;
  const value = Number(digits) * base ** (powers[unit.toLowerCase()] ?? 0);
  return Number.isFinite(value) ? Math.round(value) : null;
}

/**
 * A byte count written so {@link parseSize} reads it back exactly:
 * `104857600` -> `"100 MiB"`, `5000000` -> `"5 MB"`, else plain bytes. A
 * rounded display value in an editable field would change the floor on save.
 */
export function formatSizeExact(bytes: number): string {
  if (!Number.isInteger(bytes) || bytes <= 0) return String(bytes);
  const units = ["", "k", "M", "G", "T"];
  for (let power = 4; power >= 1; power -= 1) {
    const unit = units[power] ?? "";
    if (bytes % 1024 ** power === 0) return `${bytes / 1024 ** power} ${unit === "k" ? "K" : unit}iB`;
    if (bytes % 1000 ** power === 0) return `${bytes / 1000 ** power} ${unit}B`;
  }
  return String(bytes);
}

// ---------------------------------------------------------------------------
// Fill bar
// ---------------------------------------------------------------------------

export interface FillBar {
  /** Percent of the drive already used. */
  readonly used: number;
  /** Percent this run adds. */
  readonly planned: number;
  /** Percent wanted beyond this run (deferred). */
  readonly deferred: number;
  /** Where the fill limit sits, percent. */
  readonly limit: number;
  readonly fits: boolean;
  /** Bytes the wanted set exceeds the limit by; 0 when it fits. */
  readonly overBy: number;
}

/**
 * Segments of the destination bar, as percents of its capacity. Segments
 * never sum past 100: whatever would overflow is cut from the deferred end,
 * and {@link FillBar.overBy} says by how much the wanted set misses.
 */
export function fillBar(fill: DestFill): FillBar {
  const total = fill.total_bytes;
  if (total <= 0) return { used: 0, planned: 0, deferred: 0, limit: 75, fits: fill.fits, overBy: 0 };
  const pct = (bytes: number) => Math.max(0, (bytes / total) * 100);
  const used = Math.min(100, pct(fill.used_bytes));
  const planned = Math.min(100 - used, pct(fill.planned_logical));
  const deferred = Math.min(100 - used - planned, pct(fill.wanted_logical - fill.planned_logical));
  const limitBytes = total * fill.reserve_pressure;
  const overBy = fill.fits ? 0 : Math.max(0, fill.used_bytes + fill.wanted_logical - limitBytes);
  return { used, planned, deferred, limit: fill.reserve_pressure * 100, fits: fill.fits, overBy };
}

// ---------------------------------------------------------------------------
// Profile drafts
// ---------------------------------------------------------------------------

export const PROFILE_SCHEMA_VERSION = 1;
export const DEFAULT_FLOOR_ALLOCATED = 100 * 1024 * 1024;
export const DEFAULT_REVIEW_CAP = 50_000;
export const MAX_REVIEW_CAP = 1_000_000;

/** `[A-Za-z0-9._-]`, not starting with `.`, at most 64 bytes: `validate_label`. */
export function validLabel(label: string): boolean {
  return /^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/u.test(label);
}

/** A label for a volume root: its name in the label alphabet. */
export function labelFor(root: string): string {
  const name = root === "/" ? "Macintosh-HD" : (root.split("/").filter(Boolean).at(-1) ?? "source");
  const cleaned = name
    .replace(/[^A-Za-z0-9._-]+/gu, "-")
    .replace(/^[.-]+/u, "")
    .slice(0, 64);
  return cleaned.length > 0 ? cleaned : "source";
}

/** The physical disk of a mount point, from the volume list. */
export type DiskOf = (mountPoint: string) => string | null;

export interface DraftProblem {
  /** `"name"`, `"dest"`, `"floor"`, `"cap"`, `"sources"`, or `"source:<index>"`. */
  readonly field: string;
  readonly message: string;
}

function isUnder(root: string, path: string): boolean {
  if (root === "/") return path.startsWith("/");
  return path === root || path.startsWith(`${root}/`);
}

/** Every problem a draft has that needs no filesystem. Empty: it may be saved. */
export function draftProblems(profile: BackupProfile, diskOf: DiskOf): DraftProblem[] {
  const problems: DraftProblem[] = [];
  if (!validLabel(profile.name)) {
    problems.push({ field: "name", message: "Letters, digits, '.', '_' and '-' only; not starting with '.'." });
  }
  if (!profile.dest.startsWith("/")) problems.push({ field: "dest", message: "Choose a destination drive." });
  if (!Number.isFinite(profile.floor_allocated) || profile.floor_allocated < 0) {
    problems.push({ field: "floor", message: "Enter a size such as 100 MB." });
  }
  if (!Number.isInteger(profile.review_cap) || profile.review_cap < 1 || profile.review_cap > MAX_REVIEW_CAP) {
    problems.push({ field: "cap", message: `A whole number from 1 to ${MAX_REVIEW_CAP.toLocaleString()}.` });
  }
  if (profile.sources.length === 0) problems.push({ field: "sources", message: "Add at least one source volume." });
  const destDisk = profile.dest.startsWith("/") ? diskOf(profile.dest) : null;
  const seen = new Set<string>();
  profile.sources.forEach((source, index) => {
    const field = `source:${index}`;
    if (!validLabel(source.label)) {
      problems.push({ field, message: `Label "${source.label}": letters, digits, '.', '_' and '-' only.` });
    } else if (seen.has(source.label)) {
      problems.push({ field, message: `Label "${source.label}" is used twice.` });
    }
    seen.add(source.label);
    if (source.root === profile.dest) {
      problems.push({ field, message: "The destination cannot back up to itself." });
    } else if (destDisk !== null && diskOf(source.root) === destDisk) {
      problems.push({
        field,
        message: `${source.root} is on the same physical disk (${destDisk}) as the destination; a backup there does not survive the disk.`,
      });
    }
    for (const include of source.include) {
      if (include.startsWith("/") && !isUnder(source.root, include)) {
        problems.push({ field, message: `${include} is not under ${source.root}.` });
      }
    }
  });
  return problems;
}

/** Includes as the profile stores them: relative to the root when under it. */
export function relativeInclude(root: string, path: string): string {
  if (root === "/") return path.replace(/^\/+/u, "");
  return path.startsWith(`${root}/`) ? path.slice(root.length + 1) : path;
}

// ---------------------------------------------------------------------------
// Index status (`rdirstat backup status --json`)
// ---------------------------------------------------------------------------

export interface IndexTally {
  readonly items: number;
  readonly bytes: number;
}

export interface IndexSource {
  readonly label: string;
  readonly backedUp: IndexTally;
  readonly cloned: IndexTally;
  readonly lastPlan: string | null;
  readonly pending: IndexTally;
  readonly skipped: IndexTally;
  readonly notBackedUp: IndexTally | null;
  readonly largestNotBackedUp: readonly { path: string; bytes: number }[];
}

export interface IndexStatus {
  readonly dest: string;
  readonly sources: readonly IndexSource[];
  readonly warnings: readonly string[];
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function tallyOf(value: unknown): IndexTally {
  const fields = record(value);
  return { items: count(fields.items), bytes: count(fields.bytes) };
}

/**
 * Reads the CLI's status document defensively: its shape belongs to the
 * CLI, so a missing field reads as zero rather than throwing.
 */
export function parseIndexStatus(value: unknown): IndexStatus {
  const doc = record(value);
  const sources = Array.isArray(doc.sources) ? doc.sources : [];
  return {
    dest: typeof doc.dest === "string" ? doc.dest : "",
    warnings: Array.isArray(doc.warnings) ? doc.warnings.filter((w): w is string => typeof w === "string") : [],
    sources: sources.map((raw) => {
      const source = record(raw);
      const largest = Array.isArray(source.largest_not_backed_up) ? source.largest_not_backed_up : [];
      return {
        label: typeof source.source_label === "string" ? source.source_label : "?",
        backedUp: tallyOf(source.backed_up),
        cloned: tallyOf(source.cloned),
        lastPlan: typeof source.last_plan === "string" ? source.last_plan : null,
        pending: tallyOf(source.pending),
        skipped: tallyOf(source.skipped),
        notBackedUp: source.not_backed_up === null || source.not_backed_up === undefined ? null : tallyOf(source.not_backed_up),
        largestNotBackedUp: largest
          .filter((pair): pair is [string, number] => Array.isArray(pair) && typeof pair[0] === "string")
          .map(([path, bytes]) => ({ path, bytes: count(bytes) })),
      };
    }),
  };
}
