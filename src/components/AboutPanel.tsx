/**
 * What this is, who wrote it, and whether it is current.
 *
 * ## The update check asks a question; it is not an updater
 *
 * It reaches GitHub's release API when the user presses the button, reports
 * what it found, and offers a link. It downloads nothing and replaces nothing.
 * A self-updater that can write to its own bundle is a large security surface,
 * and an unsigned one is a worse surface than no updater at all — see
 * `updates.rs` for the same argument from the other side of the IPC boundary.
 *
 * ## Four states, not two
 *
 * "Up to date" and "update available" are the easy pair. The two that get
 * skipped are the ones this app is actually in:
 *
 * - **nothing has been published yet**, which GitHub answers with a 404 and
 *   which is the truth for this repository today. Showing that as an error
 *   would report a fault that does not exist; showing it as "up to date" would
 *   claim something the API never said.
 * - **the check failed**, which is not knowing — deliberately distinct from
 *   knowing there is nothing.
 */

import { CircleCheck, ExternalLink, GitBranch, Loader2, RefreshCw, TriangleAlert } from "lucide-react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { checkForUpdates, type ReleaseCheckView } from "@/lib/ipc";
import { cn } from "@/lib/utils";

/** The author's page, as linked from the storage panel's credit. */
const AUTHOR_URL = "https://github.com/jbelke/";
const REPOSITORY_URL = "https://github.com/jbelke/RDirStat";

export interface AboutPanelProps {
  /** The running version, from the backend rather than from `package.json`. */
  version: string;
  /** Opens a URL in the user's browser. */
  onOpenUrl: (url: string) => void;
  className?: string;
}

export function AboutPanel({ version, onOpenUrl, className }: AboutPanelProps) {
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<ReleaseCheckView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const check = async () => {
    setChecking(true);
    setError(null);
    setResult(null);
    try {
      setResult(await checkForUpdates());
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setChecking(false);
    }
  };

  return (
    <div className={cn("flex min-h-0 flex-1 flex-col overflow-auto p-4", className)}>
      <section className="rounded border border-border/60 p-3">
        <h3 className="text-sm font-medium">RDirStat</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          A disk-usage tool for macOS: what is on the disk, what it costs, and where it could go
          instead.
        </p>
        <dl className="mt-3 flex flex-col gap-1 text-xs">
          <Field label="Version">
            <span className="rds-numeric">{version}</span>
          </Field>
          <Field label="Licence">AGPL-3.0-only, with a commercial licence available</Field>
        </dl>
      </section>

      <section className="mt-3 rounded border border-border/60 p-3">
        <h3 className="text-sm font-medium">Updates</h3>
        <div className="mt-2 flex items-center gap-2">
          <Button variant="outline" size="sm" disabled={checking} onClick={() => void check()}>
            {checking ? <Loader2 aria-hidden className="animate-spin" /> : <RefreshCw aria-hidden />}
            {checking ? "Checking…" : "Check for updates"}
          </Button>
          {result !== null && result.newerAvailable && (
            <Button size="sm" onClick={() => onOpenUrl(result.releasesUrl)}>
              <ExternalLink aria-hidden />
              Get {result.latest}
            </Button>
          )}
        </div>

        <p className="mt-2 flex items-start gap-1.5 text-xs text-muted-foreground">
          {error !== null ? (
            <>
              <TriangleAlert aria-hidden className="mt-0.5 size-3 shrink-0 text-destructive" />
              <span>Could not check: {error}</span>
            </>
          ) : result === null ? (
            <span>Asks GitHub what the newest release is. Nothing is downloaded or installed.</span>
          ) : result.latest === null ? (
            <span>No releases have been published yet, so there is nothing newer to offer.</span>
          ) : result.newerAvailable ? (
            <span>
              {result.latest} is available. You are running {result.current}.
            </span>
          ) : (
            <>
              <CircleCheck aria-hidden className="mt-0.5 size-3 shrink-0 text-pressure-ok" />
              <span>{result.current} is the newest release.</span>
            </>
          )}
        </p>
      </section>

      <section className="mt-3 rounded border border-border/60 p-3">
        <h3 className="text-sm font-medium">Source</h3>
        <div className="mt-2 flex flex-wrap gap-2">
          <Button variant="outline" size="sm" onClick={() => onOpenUrl(REPOSITORY_URL)}>
            <GitBranch aria-hidden />
            Repository
          </Button>
          <Button variant="ghost" size="sm" onClick={() => onOpenUrl(AUTHOR_URL)}>
            <ExternalLink aria-hidden />
            github.com/jbelke
          </Button>
        </div>
      </section>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="shrink-0 text-muted-foreground">{label}</dt>
      <dd className="min-w-0 truncate text-right">{children}</dd>
    </div>
  );
}
