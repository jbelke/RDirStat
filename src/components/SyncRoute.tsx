/**
 * Two folders, side by side, and the ability to close the gap between them.
 *
 * The panel used to be a form: type a source, type a destination, press Check,
 * read a list of what would be copied. That answered "what will happen if I
 * press the button" but never answered the question people actually arrive
 * with, which is **"how do these two folders differ?"** — and it could not
 * answer it, because the engine behind it deliberately never looked at the
 * destination's own contents.
 *
 * So the comparison came first and the copy became a consequence of it. The
 * panes are the product now; the copy is one button under them.
 *
 * ## Why the comparison is symmetric and the copy is not
 *
 * `syncDiff` takes a **left** and a **right**, not a source and a destination.
 * Direction is a decision the user makes *after* seeing the difference, and
 * baking it into the comparison would mean re-reading both trees every time
 * they changed their mind. Flipping the direction control is therefore free:
 * the panes do not move and nothing is re-read.
 *
 * Copying is the opposite — it is entirely about direction — and it still goes
 * through the original `syncPlan` → token → `syncApply` sequence rather than
 * acting on the comparison. A comparison is a view and mints no token; the
 * token means "the user reviewed a specific additive set", and there must not
 * be a second way to authorize a write.
 *
 * ## Both-ways is a union, never a mirror
 *
 * "Copy both ways" gives each side what the other has. It does **not** make the
 * two folders identical, because that would require deleting things, and
 * nothing here deletes anything. A file present on only one side is copied to
 * the other; a file present on both is left alone on both. That is why the
 * "if both sides have it" choice is disabled in this mode: "use the source" has
 * no meaning when both sides are the source.
 *
 * The two directions run in sequence, not together, and each one re-plans from
 * scratch against the disk as it is at that moment. So the second direction
 * sees the files the first just copied — they are present on both sides by
 * then, and are not copied back. Ordering does the work that a conflict rule
 * would otherwise have to.
 */

import {
  AlignJustify,
  ArrowLeft,
  ArrowLeftRight,
  ArrowRight,
  Check,
  Columns2,
  FileDown,
  Loader2,
  Rows2,
  SearchCheck,
  TriangleAlert,
} from "lucide-react";
import { useState } from "react";

import { PathField } from "@/components/PathField";
import { SegmentedControl } from "@/components/SegmentedControl";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import {
  syncApply,
  syncDiff,
  syncPlan,
  type CompareMode,
  type OnDiffer,
  type SyncDiffEntryView,
  type SyncDiffView,
  type SyncReportView,
  type SyncWarningView,
} from "@/lib/ipc";
import { cn } from "@/lib/utils";

/** Which way files move. Not a property of the comparison — see the docblock. */
type Direction = "to_right" | "both" | "to_left";

/** How the two sides are arranged on screen. */
type PaneLayout = "split" | "stacked" | "unified";

/**
 * How many rows are put in the DOM.
 *
 * The backend already caps its listing, but even that cap is three panes'
 * worth of nodes and enough to make scrolling stutter. Surfaced in the footer
 * rather than applied quietly — a listing that stops early while claiming to
 * be complete is worse than a slow one.
 */
const ROWS_ON_SCREEN = 1_000;

const ICON = "size-3.5";

const LAYOUT_OPTIONS = [
  {
    value: "split" as PaneLayout,
    label: <Columns2 aria-hidden className={ICON} />,
    srLabel: "Side by side",
    title: "Side by side",
  },
  {
    value: "stacked" as PaneLayout,
    label: <Rows2 aria-hidden className={ICON} />,
    srLabel: "Top and bottom",
    title: "Top and bottom",
  },
  {
    value: "unified" as PaneLayout,
    label: <AlignJustify aria-hidden className={ICON} />,
    srLabel: "One combined list",
    title: "One combined list",
  },
];

const DIRECTION_OPTIONS = [
  {
    value: "to_right" as Direction,
    label: <ArrowRight aria-hidden className={ICON} />,
    srLabel: "Copy left to right",
    title: "Give the right side what only the left has",
  },
  {
    value: "both" as Direction,
    label: <ArrowLeftRight aria-hidden className={ICON} />,
    srLabel: "Copy both ways",
    title: "Give each side what only the other has. Nothing is deleted or overwritten.",
  },
  {
    value: "to_left" as Direction,
    label: <ArrowLeft aria-hidden className={ICON} />,
    srLabel: "Copy right to left",
    title: "Give the left side what only the right has",
  },
];

const COMPARE_OPTIONS = [
  {
    value: "quick" as CompareMode,
    label: "Quick",
    title: "Compares name and size. Fast enough for a large folder.",
  },
  {
    value: "verify" as CompareMode,
    label: "Verify",
    title: "Also compares the contents of same-sized files. Reads both sides in full.",
  },
];

const DIFFER_OPTIONS = [
  {
    value: "skip" as OnDiffer,
    label: "Keep theirs",
    title: "A file that exists on both sides is left alone, even if it differs.",
  },
  {
    value: "replace" as OnDiffer,
    label: "Use source",
    title: "Overwrite the other side's copy when the two differ. The only destructive setting.",
  },
];

/** The gutter glyph says which way THIS row would move, if any. */
const STATUS_GLYPH: Record<SyncDiffEntryView["status"], string> = {
  only_left: "→",
  only_right: "←",
  differs: "≠",
  same: "=",
};

const STATUS_TONE: Record<SyncDiffEntryView["status"], string> = {
  only_left: "text-brand",
  only_right: "text-brand",
  differs: "text-pressure-warn",
  same: "text-muted-foreground",
};

const STATUS_TITLE: Record<SyncDiffEntryView["status"], string> = {
  only_left: "Only on the left",
  only_right: "Only on the right",
  differs: "On both sides, and they differ",
  same: "The same on both sides",
};

/** A direction that copied nothing, carrying the plan's reasons for refusing. */
interface SyncRefusal {
  readonly source: string;
  readonly destination: string;
  readonly warnings: readonly SyncWarningView[];
  readonly bytesToCopy: number;
  readonly destinationAvailable: number;
}

/**
 * What one direction did. A refusal is a result, not the absence of one.
 *
 * `runOneWay` used to return `null` when the plan withheld its token, and the
 * caller pushed nothing — so a destination with no room reached the user as an
 * unchanged screen.
 */
type SyncOutcome =
  | { readonly kind: "copied"; readonly report: SyncReportView }
  | { readonly kind: "refused"; readonly refusal: SyncRefusal };

export function SyncRoute() {
  const [left, setLeft] = useState("");
  const [right, setRight] = useState("");
  const [compareMode, setCompareMode] = useState<CompareMode>("quick");
  const [onDiffer, setOnDiffer] = useState<OnDiffer>("skip");
  const [direction, setDirection] = useState<Direction>("to_right");
  const [layout, setLayout] = useState<PaneLayout>("split");
  // Differences first, because in any real pair of folders the agreements
  // outnumber them by orders of magnitude and would spend the whole listing.
  const [differencesOnly, setDifferencesOnly] = useState(true);

  const [diff, setDiff] = useState<SyncDiffView | null>(null);
  const [comparing, setComparing] = useState(false);
  const [copying, setCopying] = useState(false);
  const [outcomes, setOutcomes] = useState<readonly SyncOutcome[]>([]);
  const [error, setError] = useState<string | null>(null);

  const ready = left.trim().length > 0 && right.trim().length > 0;

  // Both-ways cannot overwrite: there is no "the source" when both sides are.
  const effectiveOnDiffer: OnDiffer = direction === "both" ? "skip" : onDiffer;

  /*
   * Comparing is explicit, not reactive on every keystroke.
   *
   * It reads both trees, and under Verify reads both sides of every same-sized
   * file in full. Re-running that as someone types a path would turn an idle
   * text field into sustained disk I/O. It is also the wrong mental model:
   * this is "compare, look, then copy", and a result that appeared on its own
   * would invite treating it as live when it is a snapshot of a moment.
   */
  const compare = async () => {
    setComparing(true);
    setError(null);
    setOutcomes([]);
    try {
      setDiff(await syncDiff(left.trim(), right.trim(), compareMode, differencesOnly));
    } catch (cause) {
      setDiff(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setComparing(false);
    }
  };

  /**
   * One direction, planned and confirmed on its own.
   *
   * A withheld token is reported rather than swallowed: the plan already
   * carries the reasons it refused, and those are the only explanation the
   * user will ever get for a copy that did not happen.
   */
  const runOneWay = async (from: string, to: string): Promise<SyncOutcome> => {
    const plan = await syncPlan(from, to, compareMode, effectiveOnDiffer);
    if (plan.token === null) {
      return {
        kind: "refused",
        refusal: {
          source: plan.source,
          destination: plan.destination,
          warnings: plan.warnings,
          bytesToCopy: plan.bytesToCopy,
          destinationAvailable: plan.destinationAvailable,
        },
      };
    }
    const report = await syncApply(from, to, compareMode, effectiveOnDiffer, plan.token);
    return { kind: "copied", report };
  };

  const copy = async () => {
    setCopying(true);
    setError(null);
    try {
      const done: SyncOutcome[] = [];
      // Sequential, and each direction plans against the disk as it is when
      // its turn comes — which is what stops the second leg from copying back
      // what the first just delivered.
      if (direction !== "to_left") {
        done.push(await runOneWay(left.trim(), right.trim()));
      }
      if (direction !== "to_right") {
        done.push(await runOneWay(right.trim(), left.trim()));
      }
      setOutcomes(done);
      // The panes described a state that no longer exists. Re-read rather than
      // leave stale rows on screen inviting a second copy of the same files.
      setDiff(await syncDiff(left.trim(), right.trim(), compareMode, differencesOnly));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCopying(false);
    }
  };

  /** Exchanges the two folders, panes and all. */
  const swapSides = () => {
    setLeft(right);
    setRight(left);
    setDiff(null);
    setOutcomes([]);
  };

  const toCopy = countForDirection(diff, direction, effectiveOnDiffer);

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-auto p-4">
      <header className="mb-3 flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-medium">Compare folders</h2>
          <p className="mt-1 text-xs text-muted-foreground">
            Shows what each side has. Copying only ever adds files — nothing is deleted, and nothing
            is overwritten unless you ask.
          </p>
        </div>
        <SegmentedControl
          label="How to arrange the two folders"
          options={LAYOUT_OPTIONS}
          value={layout}
          onChange={setLayout}
        />
      </header>

      <section className="grid grid-cols-[1fr_auto_1fr] items-start gap-3">
        <PathField
          inputId="sync-left"
          label="Left"
          layout="stacked"
          placeholder="/Volumes/Archive/photos"
          value={left}
          onChange={setLeft}
        />
        <div className="flex flex-col items-center gap-1 pt-5">
          <SegmentedControl
            label="Which way to copy"
            options={DIRECTION_OPTIONS}
            value={direction}
            onChange={setDirection}
          />
          <button
            type="button"
            onClick={swapSides}
            className="rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
            title="Exchange the two folders"
          >
            swap sides
          </button>
        </div>
        <PathField
          inputId="sync-right"
          label="Right"
          layout="stacked"
          placeholder="/Volumes/Backup/photos"
          value={right}
          onChange={setRight}
        />
      </section>

      <section className="mt-3 flex flex-wrap items-end gap-4">
        <Labelled label="Compare by">
          <SegmentedControl
            label="How to compare files"
            options={COMPARE_OPTIONS}
            value={compareMode}
            onChange={setCompareMode}
          />
        </Labelled>
        <Labelled label="If both sides have it">
          <SegmentedControl
            label="What to do when a file exists on both sides"
            options={DIFFER_OPTIONS}
            value={effectiveOnDiffer}
            onChange={setOnDiffer}
            className={cn(direction === "both" && "pointer-events-none opacity-50")}
          />
        </Labelled>
        <label className="flex items-center gap-1.5 pb-1 text-xs text-muted-foreground">
          <input
            type="checkbox"
            checked={differencesOnly}
            onChange={(event) => setDifferencesOnly(event.target.checked)}
          />
          Only differences
        </label>

        <Button
          className="ml-auto"
          variant="outline"
          disabled={!ready || comparing || copying}
          onClick={() => void compare()}
        >
          {comparing ? <Loader2 aria-hidden className="animate-spin" /> : <SearchCheck aria-hidden />}
          {comparing ? "Comparing…" : "Compare"}
        </Button>
      </section>

      {direction === "both" && (
        <p className="mt-2 text-xs text-muted-foreground">
          Both ways gives each side what only the other has. It never deletes and never
          overwrites, so a file that exists on both sides with different contents is left alone on
          both — the two folders can still differ afterwards, and that is the guarantee, not a
          failure.
        </p>
      )}

      {error !== null && (
        <Alert variant="destructive" className="mt-4">
          <TriangleAlert aria-hidden />
          <AlertTitle>These folders cannot be compared</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {outcomes.map((outcome) =>
        outcome.kind === "copied" ? (
          <SyncResult key={`copied:${outcome.report.destination}`} report={outcome.report} />
        ) : (
          <SyncRefused key={`refused:${outcome.refusal.destination}`} refusal={outcome.refusal} />
        ),
      )}

      {diff !== null && (
        <>
          <Summary diff={diff} />
          <Panes diff={diff} layout={layout} />
          <div className="mt-3 flex items-center gap-3">
            <p className="text-xs text-muted-foreground">
              {diff.entriesTruncated
                ? "Listing capped. The counts above are complete."
                : `${diff.entries.length.toLocaleString()} row${diff.entries.length === 1 ? "" : "s"}${
                    diff.entries.length > ROWS_ON_SCREEN
                      ? `, showing the first ${ROWS_ON_SCREEN.toLocaleString()}`
                      : ""
                  }`}
            </p>
            <Button
              className="ml-auto"
              disabled={toCopy === 0 || copying || comparing}
              onClick={() => void copy()}
            >
              {copying ? <Loader2 aria-hidden className="animate-spin" /> : <FileDown aria-hidden />}
              {copying
                ? "Copying…"
                : toCopy === 0
                  ? "Nothing to copy"
                  : `Copy ${toCopy.toLocaleString()} file${toCopy === 1 ? "" : "s"} ${
                      direction === "both" ? "both ways" : direction === "to_right" ? "→" : "←"
                    }`}
            </Button>
          </div>
        </>
      )}

      {diff === null && !comparing && error === null && (
        <p className="mt-6 text-xs text-muted-foreground">
          Pick two folders and press Compare. Nothing is read until you do.
        </p>
      )}
    </div>
  );
}

/**
 * How many files the chosen direction would copy.
 *
 * Read off the comparison rather than by asking the backend for a plan: the
 * comparison already counted exactly this, and planning to label a button
 * would walk both trees again every time the direction toggle moved.
 */
function countForDirection(
  diff: SyncDiffView | null,
  direction: Direction,
  onDiffer: OnDiffer,
): number {
  if (diff === null) return 0;
  // Both-ways never overwrites, so a differing file counts for neither leg.
  if (direction === "both") return diff.onlyLeft + diff.onlyRight;
  const replaced = onDiffer === "replace" ? diff.differing : 0;
  return (direction === "to_right" ? diff.onlyLeft : diff.onlyRight) + replaced;
}

function Summary({ diff }: { diff: SyncDiffView }) {
  return (
    <dl className="mt-4 grid grid-cols-2 gap-2 text-xs sm:grid-cols-4">
      <Stat
        label="Only on the left"
        value={diff.onlyLeft.toLocaleString()}
        note={formatSI(diff.bytesOnlyLeft)}
      />
      <Stat
        label="Only on the right"
        value={diff.onlyRight.toLocaleString()}
        note={formatSI(diff.bytesOnlyRight)}
      />
      <Stat label="Differ" value={diff.differing.toLocaleString()} alarming={diff.differing > 0} />
      <Stat label="Identical" value={diff.same.toLocaleString()} />
    </dl>
  );
}

function Panes({ diff, layout }: { diff: SyncDiffView; layout: PaneLayout }) {
  const rows = diff.entries.slice(0, ROWS_ON_SCREEN);

  if (layout === "unified") {
    return (
      <Frame>
        <Header left={diff.left} right={diff.right} combined />
        <ul>
          {rows.map((entry) => (
            <li key={entry.relativePath} className={ROW}>
              <Glyph status={entry.status} />
              <span className="min-w-0 flex-1 truncate font-mono" title={entry.relativePath}>
                {entry.relativePath}
              </span>
              <Size bytes={entry.leftBytes} />
              <Size bytes={entry.rightBytes} />
            </li>
          ))}
        </ul>
      </Frame>
    );
  }

  if (layout === "stacked") {
    return (
      <div className="mt-2 flex flex-col gap-2">
        <Side rows={rows} side="left" path={diff.left} />
        <Side rows={rows} side="right" path={diff.right} />
      </div>
    );
  }

  // Side by side. One grid, three columns, one row per path — which is what
  // makes the two panes line up without any scroll-syncing between them.
  return (
    <Frame>
      <Header left={diff.left} right={diff.right} />
      <ul>
        {rows.map((entry) => (
          <li
            key={entry.relativePath}
            className="grid grid-cols-[1fr_auto_1fr] items-center gap-2 border-b border-border/40 px-3 py-1 last:border-b-0"
          >
            <Cell entry={entry} side="left" />
            <Glyph status={entry.status} />
            <Cell entry={entry} side="right" />
          </li>
        ))}
      </ul>
    </Frame>
  );
}

const ROW = "flex items-center gap-2 border-b border-border/40 px-3 py-1 last:border-b-0";

function Frame({ children }: { children: React.ReactNode }) {
  return (
    <div className="mt-2 max-h-[28rem] overflow-auto rounded border border-border/60 text-xs">
      {children}
    </div>
  );
}

function Header({
  left,
  right,
  combined = false,
}: {
  left: string;
  right: string;
  combined?: boolean;
}) {
  if (combined) {
    return (
      <div className={cn(ROW, "sticky top-0 bg-background font-medium")}>
        <span className="w-4 shrink-0" />
        <span className="min-w-0 flex-1 truncate">Path</span>
        <span className="w-20 shrink-0 truncate text-right" title={left}>
          {basename(left)}
        </span>
        <span className="w-20 shrink-0 truncate text-right" title={right}>
          {basename(right)}
        </span>
      </div>
    );
  }
  return (
    <div className="sticky top-0 grid grid-cols-[1fr_auto_1fr] items-center gap-2 border-b border-border/60 bg-background px-3 py-1 font-medium">
      <span className="min-w-0 truncate font-mono" title={left}>
        {left}
      </span>
      <span className="w-4" />
      <span className="min-w-0 truncate font-mono" title={right}>
        {right}
      </span>
    </div>
  );
}

/** One pane of the stacked layout: a whole side, rows still in shared order. */
function Side({
  rows,
  side,
  path,
}: {
  rows: readonly SyncDiffEntryView[];
  side: "left" | "right";
  path: string;
}) {
  return (
    <div className="max-h-56 overflow-auto rounded border border-border/60 text-xs">
      <div className={cn(ROW, "sticky top-0 bg-background font-mono font-medium")} title={path}>
        <span className="min-w-0 flex-1 truncate">{path}</span>
      </div>
      <ul>
        {rows.map((entry) => (
          <li key={entry.relativePath} className={ROW}>
            <Glyph status={entry.status} />
            <Cell entry={entry} side={side} />
          </li>
        ))}
      </ul>
    </div>
  );
}

/** One side's view of a row — its own copy, or the gap where it has none. */
function Cell({ entry, side }: { entry: SyncDiffEntryView; side: "left" | "right" }) {
  const bytes = side === "left" ? entry.leftBytes : entry.rightBytes;
  if (bytes === null) {
    return <span className="truncate text-muted-foreground/40">—</span>;
  }
  return (
    <span className="flex min-w-0 items-center gap-2">
      <span
        className={cn("min-w-0 flex-1 truncate font-mono", STATUS_TONE[entry.status])}
        title={entry.relativePath}
      >
        {entry.relativePath}
      </span>
      <span className="rds-numeric w-16 shrink-0 text-right text-muted-foreground">
        {formatSI(bytes)}
      </span>
    </span>
  );
}

function Glyph({ status }: { status: SyncDiffEntryView["status"] }) {
  return (
    <span
      className={cn("w-4 shrink-0 text-center", STATUS_TONE[status])}
      title={STATUS_TITLE[status]}
      aria-label={STATUS_TITLE[status]}
      role="img"
    >
      {STATUS_GLYPH[status]}
    </span>
  );
}

function Size({ bytes }: { bytes: number | null }) {
  return (
    <span className="rds-numeric w-20 shrink-0 text-right text-muted-foreground">
      {bytes === null ? "—" : formatSI(bytes)}
    </span>
  );
}

function basename(path: string): string {
  const parts = path.split("/").filter((part) => part.length > 0);
  return parts[parts.length - 1] ?? path;
}

function Labelled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1">
      <span className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

/**
 * A direction that copied nothing, and the plan's own reasons why.
 *
 * `sync.rs` computes these codes — `nothing-to-do`, `metadata-loss`,
 * `no-room` — and withholds the confirmation token when it refuses. The plan
 * used to be read for its token alone and discarded, so "the destination has
 * no room" reached the user as a screen that simply did not change.
 *
 * Severity deliberately stays off `destructive`. That variant means a copy ran
 * and files failed; nothing ran here. And a refusal whose only reason is that
 * the folders already match is not a problem at all, so it keeps the plain
 * check — the same axis the red-success bug established.
 */
function SyncRefused({ refusal }: { refusal: SyncRefusal }) {
  const benign = refusal.warnings.length > 0 && refusal.warnings.every((warning) => warning.code === "nothing-to-do");
  const shortfall = refusal.bytesToCopy - refusal.destinationAvailable;

  return (
    <div className="mt-4">
      <Alert>
        {benign ? <Check aria-hidden /> : <TriangleAlert aria-hidden />}
        <AlertTitle>
          {benign ? "Nothing to copy" : `Nothing was copied to ${refusal.destination}`}
        </AlertTitle>
        <AlertDescription>
          {benign
            ? `${refusal.source} has nothing that ${refusal.destination} is missing.`
            : "The copy was refused before it started, so both folders are unchanged."}
        </AlertDescription>
      </Alert>

      {!benign && refusal.warnings.length > 0 && (
        <ul className="mt-2 flex flex-col gap-1">
          {refusal.warnings.map((warning) => (
            <li key={warning.code} className="rounded border border-border/60 p-2 text-xs">
              <p className="font-mono text-muted-foreground">{warning.code}</p>
              <p className="mt-0.5">{warning.message}</p>
              {warning.code === "no-room" && shortfall > 0 && (
                <p className="mt-0.5 text-muted-foreground">
                  Needs {formatSI(refusal.bytesToCopy)}, {formatSI(refusal.destinationAvailable)} free —{" "}
                  {formatSI(shortfall)} short.
                </p>
              )}
            </li>
          ))}
        </ul>
      )}

      {/* A refusal with no stated reason is still a refusal; say so rather than
          rendering an empty list and letting the screen imply success. */}
      {!benign && refusal.warnings.length === 0 && (
        <p className="mt-2 text-xs text-muted-foreground">
          The plan gave no reason. This is a bug — please report it.
        </p>
      )}
    </div>
  );
}

function SyncResult({ report }: { report: SyncReportView }) {
  return (
    <div className="mt-4">
      <Alert variant={report.failures.length > 0 ? "destructive" : "default"}>
        {report.failures.length > 0 ? <TriangleAlert aria-hidden /> : <Check aria-hidden />}
        <AlertTitle>
          {report.failures.length === 0
            ? `Copied ${report.copied.toLocaleString()} file${report.copied === 1 ? "" : "s"}`
            : `Copied ${report.copied.toLocaleString()}, ${report.failures.length} failed`}
        </AlertTitle>
        <AlertDescription>
          {formatSI(report.bytesCopied)} written to {report.destination}. Nothing was removed.
        </AlertDescription>
      </Alert>

      {report.failures.length > 0 && (
        <ul className="mt-2 flex flex-col gap-1">
          {report.failures.map((failure) => (
            <li key={failure.relativePath} className="rounded border border-border/60 p-2 text-xs">
              <p className="truncate font-mono" title={failure.relativePath}>
                {failure.relativePath}
              </p>
              <p className="mt-0.5 text-muted-foreground">{failure.reason}</p>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Stat({
  label,
  value,
  note,
  alarming = false,
}: {
  label: string;
  value: string;
  note?: string;
  alarming?: boolean;
}) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5">
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className={cn("rds-numeric truncate text-sm", alarming && "text-pressure-warn")}>
        {value}
        {note !== undefined && (
          <span className="ml-1.5 text-[10px] text-muted-foreground">{note}</span>
        )}
      </dd>
    </div>
  );
}
