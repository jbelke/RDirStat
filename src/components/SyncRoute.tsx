/**
 * Copy what one folder has and another is missing.
 *
 * The operation is deliberately narrow, and the narrowness is what makes it
 * safe enough to offer: **nothing in the destination is ever deleted**, and
 * nothing is overwritten unless the user asks for it explicitly. A file that
 * exists only in the destination is not touched, not listed, and not counted.
 * "Sync" in the mirroring sense is the version that eats data, and it is not
 * what this is.
 *
 * The panel is built the same way the move dialog is: **the plan is the
 * product**. Checking is a separate, non-destructive step that produces a
 * reviewable list, and the Copy button keys off the backend-minted token
 * rather than off the check having succeeded. Re-checking is free; copying is
 * not, and the two should never be one click.
 */

import { AlertTriangle, ArrowRight, Check, FileDown, Loader2, SearchCheck } from "lucide-react";
import { useState } from "react";

import { SegmentedControl } from "@/components/SegmentedControl";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import {
  syncApply,
  syncPlan,
  type CompareMode,
  type OnDiffer,
  type SyncPlanView,
  type SyncReportView,
} from "@/lib/ipc";
import { cn } from "@/lib/utils";

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
    title: "Overwrite the destination's copy when the two differ. The only destructive setting.",
  },
];

const REASON_LABEL: Record<string, string> = {
  missing: "not there",
  size_differs: "different size",
  content_differs: "different contents",
};

export function SyncRoute() {
  const [source, setSource] = useState("");
  const [destination, setDestination] = useState("");
  const [compareMode, setCompareMode] = useState<CompareMode>("quick");
  const [onDiffer, setOnDiffer] = useState<OnDiffer>("skip");

  const [plan, setPlan] = useState<SyncPlanView | null>(null);
  const [checking, setChecking] = useState(false);
  const [copying, setCopying] = useState(false);
  const [report, setReport] = useState<SyncReportView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const ready = source.trim().length > 0 && destination.trim().length > 0;

  /*
   * Checking is explicit, not reactive on every keystroke.
   *
   * A plan walks the whole source tree and, under Verify, reads both sides in
   * full. Re-running that as someone types a path would turn an idle text
   * field into sustained disk I/O. It is also the wrong mental model: this is
   * "check, review, then copy", and a result that appeared on its own would
   * invite treating it as live when it is a snapshot of a moment.
   */
  const check = async () => {
    setChecking(true);
    setError(null);
    setReport(null);
    try {
      setPlan(await syncPlan(source.trim(), destination.trim(), compareMode, onDiffer));
    } catch (cause) {
      setPlan(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setChecking(false);
    }
  };

  const copy = async () => {
    if (plan?.token == null) return;
    setCopying(true);
    setError(null);
    try {
      const next = await syncApply(
        source.trim(),
        destination.trim(),
        compareMode,
        onDiffer,
        plan.token,
      );
      setReport(next);
      // The plan described a state that no longer exists. Keeping it on screen
      // beside the result would invite a second copy of files already copied.
      setPlan(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCopying(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-auto p-4">
      <header className="mb-3">
        <h2 className="text-sm font-medium">Sync folders</h2>
        <p className="mt-1 text-xs text-muted-foreground">
          Copies files the destination is missing. Nothing in the destination is deleted, and
          nothing is overwritten unless you ask.
        </p>
      </header>

      <section className="flex flex-col gap-2">
        <PathField label="Source" placeholder="/Volumes/Archive/photos" value={source} onChange={setSource} />
        <PathField
          label="Destination"
          placeholder="/Volumes/Backup/photos"
          value={destination}
          onChange={setDestination}
        />
      </section>

      <section className="mt-3 flex flex-wrap items-center gap-4">
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
            value={onDiffer}
            onChange={setOnDiffer}
          />
        </Labelled>

        <Button
          className="ml-auto"
          variant="outline"
          disabled={!ready || checking || copying}
          onClick={() => void check()}
        >
          {checking ? <Loader2 aria-hidden className="animate-spin" /> : <SearchCheck aria-hidden />}
          {checking ? "Checking…" : "Check"}
        </Button>
      </section>

      {error !== null && (
        <Alert variant="destructive" className="mt-4">
          <AlertTriangle aria-hidden />
          <AlertTitle>This sync cannot run</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {report !== null && <SyncResult report={report} />}
      {plan !== null && <PlanReview plan={plan} copying={copying} onCopy={() => void copy()} />}
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

function PathField({
  label,
  placeholder,
  value,
  onChange,
}: {
  label: string;
  placeholder: string;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <label className="flex items-center gap-2">
      <span className="w-20 shrink-0 text-xs text-muted-foreground">{label}</span>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        spellCheck={false}
        onChange={(event) => onChange(event.target.value)}
        className="min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 font-mono text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      />
    </label>
  );
}

function PlanReview({
  plan,
  copying,
  onCopy,
}: {
  plan: SyncPlanView;
  copying: boolean;
  onCopy: () => void;
}) {
  const short = plan.bytesToCopy > plan.destinationAvailable;
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
        <Stat label="To copy" value={plan.totalToCopy.toLocaleString()} />
        <Stat label="Size" value={formatSI(plan.bytesToCopy)} />
        <Stat label="Already there" value={plan.alreadyPresent.toLocaleString()} />
        <Stat
          label={`Free (${plan.destinationFilesystem})`}
          value={formatSI(plan.destinationAvailable)}
          alarming={short}
        />
      </dl>

      {plan.warnings.length > 0 && (
        <Alert variant={plan.token === null ? "destructive" : "default"} className="mt-3">
          <AlertTriangle aria-hidden />
          <AlertTitle>{plan.token === null ? "Nothing will be copied" : "Before you copy"}</AlertTitle>
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
              <li key={entry.relativePath} className="flex items-center gap-2 border-b border-border/40 px-3 py-1 last:border-b-0">
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
              {plan.totalToCopy.toLocaleString()}. All of them will be copied.
            </p>
          )}
        </>
      )}

      <div className="mt-3 flex justify-end">
        <Button disabled={plan.token === null || copying} onClick={onCopy}>
          {copying ? <Loader2 aria-hidden className="animate-spin" /> : <FileDown aria-hidden />}
          {copying
            ? "Copying…"
            : plan.token === null
              ? "Nothing to copy"
              : `Copy ${plan.totalToCopy.toLocaleString()} file${plan.totalToCopy === 1 ? "" : "s"}`}
        </Button>
      </div>
    </section>
  );
}

function SyncResult({ report }: { report: SyncReportView }) {
  return (
    <div className="mt-4">
      <Alert variant={report.failures.length > 0 ? "destructive" : "default"}>
        {report.failures.length > 0 ? <AlertTriangle aria-hidden /> : <Check aria-hidden />}
        <AlertTitle>
          {report.failures.length === 0
            ? `Copied ${report.copied.toLocaleString()} file${report.copied === 1 ? "" : "s"}`
            : `Copied ${report.copied.toLocaleString()}, ${report.failures.length} failed`}
        </AlertTitle>
        <AlertDescription>
          {formatSI(report.bytesCopied)} written to {report.destination}. Nothing in the destination
          was removed.
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

function Stat({ label, value, alarming = false }: { label: string; value: string; alarming?: boolean }) {
  return (
    <div className="rounded border border-border/60 px-2 py-1.5">
      <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</dt>
      <dd className={cn("rds-numeric truncate text-sm", alarming && "text-pressure-critical")}>{value}</dd>
    </div>
  );
}
