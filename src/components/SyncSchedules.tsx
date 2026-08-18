/**
 * Folder syncs that run without anybody present.
 *
 * ## What the user is authorising here is a policy, not a set
 *
 * Everywhere else in this app, writing to disk is authorised by looking at a
 * plan and confirming that exact list. A schedule cannot work that way — the
 * whole point is that the files at 03:00 are not the files you reviewed on
 * Tuesday — so what is being authorised is narrower and more durable: *this
 * source, this destination, additive only*. The backend has a separate entry
 * point for that authority which cannot accept a confirmation token and cannot
 * be asked to overwrite; see `sync::apply_scheduled`.
 *
 * That is why there is no "if both sides have it" control on this form. It is
 * not an omission and it is not a default — it is a setting that does not
 * exist on this path.
 *
 * ## Additive is not the same as harmless
 *
 * Nothing here deletes or overwrites, and the panel still warns. A scheduled
 * sync consumes space on the far side, pushes files somewhere that was set up
 * months ago and may have been forgotten, and can saturate a metered or mobile
 * link. None of that is destruction and all of it is a surprise, and "additive"
 * is not an answer to any of it.
 */

import { CircleAlert, Clock, Play, Plus, Trash2, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";

import { PathField } from "@/components/PathField";
import { SegmentedControl } from "@/components/SegmentedControl";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import {
  deleteSyncSchedule,
  runSyncScheduleNow,
  saveSyncSchedule,
  syncSchedules,
  type CompareMode,
  type ScheduleOutcomeView,
  type SyncScheduleView,
} from "@/lib/ipc";
import { cn } from "@/lib/utils";

/** Mirrors `schedules::MINIMUM_INTERVAL_MINUTES`; the backend clamps regardless. */
const MINIMUM_MINUTES = 15;

const INTERVALS = [
  { minutes: 60, label: "Hourly" },
  { minutes: 60 * 6, label: "Every 6 hours" },
  { minutes: 60 * 24, label: "Daily" },
  { minutes: 60 * 24 * 7, label: "Weekly" },
];

const COMPARE_OPTIONS = [
  { value: "quick" as CompareMode, label: "Quick", title: "Compares name and size." },
  {
    value: "verify" as CompareMode,
    label: "Verify",
    title: "Also compares the contents of same-sized files. Reads both sides in full, every run.",
  },
];

export function SyncSchedules({ className }: { className?: string }) {
  const [rows, setRows] = useState<readonly SyncScheduleView[]>([]);
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    void syncSchedules()
      .then((loaded) => live && setRows(loaded))
      .catch(() => {
        // No backend (a browser dev server). An empty list is the honest
        // rendering of "we could not ask".
      });
    return () => {
      live = false;
    };
  }, []);

  const guard = async (id: string, work: () => Promise<readonly SyncScheduleView[]>) => {
    setBusy(id);
    setError(null);
    try {
      setRows(await work());
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className={cn("flex flex-col gap-2 rounded border border-border/60 p-3", className)}>
      <div className="flex items-baseline justify-between gap-4">
        <span className="text-sm font-medium">Scheduled syncs</span>
        {!adding && (
          <Button variant="outline" size="sm" onClick={() => setAdding(true)}>
            <Plus aria-hidden />
            Add
          </Button>
        )}
      </div>
      <p className="text-xs text-muted-foreground">
        Copies files the other side is missing, on a timer, with nobody watching. It never deletes
        and never overwrites — but it still uses space on the far side, still writes to a folder you
        set up once and may have forgotten, and still uses the network if the destination is on it.
      </p>

      {rows.length === 0 && !adding && (
        <p className="text-xs text-muted-foreground">Nothing is scheduled.</p>
      )}

      <ul className="flex flex-col gap-2">
        {rows.map((row) => (
          <li key={row.id} className="rounded border border-border/60 p-2">
            <div className="flex items-baseline gap-2">
              <span className="min-w-0 flex-1 truncate text-xs font-medium">{row.name}</span>
              <span className="shrink-0 text-[10px] uppercase tracking-wide text-muted-foreground">
                {describeInterval(row.everyMinutes)}
              </span>
              <label className="flex shrink-0 items-center gap-1 text-[10px] text-muted-foreground">
                <input
                  type="checkbox"
                  checked={row.enabled}
                  disabled={busy !== null}
                  onChange={(event) =>
                    void guard(row.id, () =>
                      saveSyncSchedule({
                        id: row.id,
                        name: row.name,
                        source: row.source,
                        destination: row.destination,
                        compareMode: row.compareMode,
                        everyMinutes: row.everyMinutes,
                        enabled: event.target.checked,
                      }),
                    )
                  }
                />
                On
              </label>
              <Button
                variant="ghost"
                size="sm"
                disabled={busy !== null}
                title="Run this now, with the same checks the timer uses"
                onClick={() => void guard(row.id, () => runSyncScheduleNow(row.id))}
              >
                <Play aria-hidden />
              </Button>
              <Button
                variant="ghost"
                size="sm"
                disabled={busy !== null}
                title="Remove this schedule"
                onClick={() => void guard(row.id, () => deleteSyncSchedule(row.id))}
              >
                <Trash2 aria-hidden />
              </Button>
            </div>
            <p className="mt-1 truncate font-mono text-[10px] text-muted-foreground">
              {row.source} → {row.destination}
            </p>
            <LastRun row={row} running={busy === row.id} />
          </li>
        ))}
      </ul>

      {adding && (
        <AddForm
          busy={busy === "new"}
          onCancel={() => setAdding(false)}
          onSave={async (draft) => {
            await guard("new", () => saveSyncSchedule({ id: "", ...draft }));
            setAdding(false);
          }}
        />
      )}

      {error !== null && (
        <p className="flex items-start gap-1.5 text-xs text-destructive">
          <TriangleAlert aria-hidden className="mt-0.5 size-3 shrink-0" />
          {error}
        </p>
      )}
    </section>
  );
}

/**
 * The last run, in the words of what actually happened.
 *
 * A refusal gets the alarming treatment rather than being folded into "did not
 * copy anything": the commonest refusal is a destination whose volume is not
 * mounted, which looks identical to a quiet success from the outside and is the
 * exact case where quiet is wrong.
 */
function LastRun({ row, running }: { row: SyncScheduleView; running: boolean }) {
  if (running) {
    return <p className="mt-1 text-[10px] text-muted-foreground">Running…</p>;
  }
  const last = row.history[0];
  if (last === undefined) {
    return (
      <p className="mt-1 flex items-center gap-1 text-[10px] text-muted-foreground">
        <Clock aria-hidden className="size-3" />
        {row.enabled ? "Has not run yet." : "Off."}
      </p>
    );
  }
  const when = new Date(last.atUnixMs).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
  return (
    <p
      className={cn(
        "mt-1 flex items-start gap-1 text-[10px]",
        last.outcome.kind === "copied" ? "text-muted-foreground" : "text-pressure-warn",
      )}
    >
      {last.outcome.kind !== "copied" && <CircleAlert aria-hidden className="mt-0.5 size-3 shrink-0" />}
      <span>
        {when} — {describeOutcome(last.outcome)}
      </span>
    </p>
  );
}

function describeOutcome(outcome: ScheduleOutcomeView): string {
  if (outcome.kind === "copied") {
    return outcome.files === 0
      ? "nothing to copy"
      : `copied ${outcome.files.toLocaleString()} file${outcome.files === 1 ? "" : "s"} (${formatSI(outcome.bytes)})`;
  }
  return outcome.kind === "refused" ? `did not run: ${outcome.reason}` : `problems: ${outcome.reason}`;
}

function describeInterval(minutes: number): string {
  const match = INTERVALS.find((interval) => interval.minutes === minutes);
  if (match !== undefined) return match.label;
  if (minutes % (60 * 24) === 0) return `every ${minutes / (60 * 24)} days`;
  if (minutes % 60 === 0) return `every ${minutes / 60} hours`;
  return `every ${minutes} min`;
}

interface Draft {
  name: string;
  source: string;
  destination: string;
  compareMode: CompareMode;
  everyMinutes: number;
  enabled: boolean;
}

function AddForm({
  busy,
  onSave,
  onCancel,
}: {
  busy: boolean;
  onSave: (draft: Draft) => Promise<void>;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [source, setSource] = useState("");
  const [destination, setDestination] = useState("");
  const [compareMode, setCompareMode] = useState<CompareMode>("quick");
  const [everyMinutes, setEveryMinutes] = useState(60 * 24);

  const ready = source.trim().length > 0 && destination.trim().length > 0;

  return (
    <div className="flex flex-col gap-2 rounded border border-border/60 bg-muted/20 p-3">
      <span className="text-xs font-medium">New schedule</span>
      <label className="flex items-center gap-2">
        <span className="w-20 shrink-0 text-xs text-muted-foreground">Name</span>
        <input
          type="text"
          value={name}
          placeholder="Photos to the archive disk"
          onChange={(event) => setName(event.target.value)}
          className="min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        />
      </label>
      <PathField label="From" placeholder="/Volumes/Archive/photos" value={source} onChange={setSource} />
      <PathField label="To" placeholder="/Volumes/Backup/photos" value={destination} onChange={setDestination} />

      <div className="flex flex-wrap items-end gap-4">
        <div className="flex flex-col gap-1">
          <span className="text-[10px] uppercase tracking-wide text-muted-foreground">How often</span>
          <select
            value={everyMinutes}
            onChange={(event) => setEveryMinutes(Number(event.target.value))}
            className="rounded border border-border/60 bg-transparent px-2 py-1 text-xs"
          >
            {INTERVALS.map((interval) => (
              <option key={interval.minutes} value={interval.minutes}>
                {interval.label}
              </option>
            ))}
          </select>
        </div>
        <div className="flex flex-col gap-1">
          <span className="text-[10px] uppercase tracking-wide text-muted-foreground">Compare by</span>
          <SegmentedControl
            label="How to compare files"
            options={COMPARE_OPTIONS}
            value={compareMode}
            onChange={setCompareMode}
          />
        </div>
        <div className="ml-auto flex items-center gap-2">
          <Button variant="outline" size="sm" disabled={busy} onClick={onCancel}>
            Cancel
          </Button>
          <Button
            size="sm"
            disabled={!ready || busy}
            onClick={() =>
              void onSave({
                name: name.trim(),
                source: source.trim(),
                destination: destination.trim(),
                compareMode,
                everyMinutes: Math.max(everyMinutes, MINIMUM_MINUTES),
                enabled: true,
              })
            }
          >
            Save
          </Button>
        </div>
      </div>
    </div>
  );
}
