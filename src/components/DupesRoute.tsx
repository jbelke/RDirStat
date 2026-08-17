/**
 * Possible duplicates — files that share a size, which is not the same thing as
 * sharing their contents.
 *
 * ## What this screen is allowed to claim
 *
 * The backend behind it (`rdirstat-core::dupes`) implements stages 1–2 of the
 * five-stage pipeline in docs/06-DATA.md: collapse repeated hard-link names, then
 * group regular files by logical size and discard the unique sizes. Stages 3–5 —
 * hashing the head and tail, full-hashing the survivors, and re-`fstat`ing to
 * catch a file that moved under the reader — do not exist yet, so **not one byte
 * of any file below has been read**.
 *
 * Every 4 KB `.plist` on a Mac is 4 KB. A same-size group is therefore a
 * shortlist of things worth comparing, and this component says so in the title,
 * in the column header, in the empty state, and next to every number. A screen
 * that renders "1.2 GB of duplicates" without having opened a file is lying to
 * the user about their disk, and it is a lie they may act on with a delete key.
 *
 * ## Why the recovery number is a range with a floor of zero
 *
 * `size × (copies − 1)` is what a naive duplicate finder prints. It is an upper
 * bound and it is unreachable two different ways: the files may simply differ, in
 * which case nothing is deletable at all; and on APFS the copies may already be
 * clones sharing physical blocks, in which case deleting one frees a directory
 * entry. So the column is "could free", the value is prefixed "up to", and the
 * expanded panel spells the range out as `0 B – X`.
 *
 * ## One copy is always protected
 *
 * docs/06 requires that a selection "cannot select the last member of a
 * cluster". Rather than model that as a selection rule that can be got wrong,
 * each cluster has a **kept** copy — the first listed by default, moveable with a
 * radio — and Trash is refused on it. The invariant then holds structurally: it
 * is not possible to reach a state where a cluster's last copy is the one being
 * removed, and it keeps holding for clusters whose member list is truncated.
 *
 * There is deliberately **no bulk delete here.** Trash acts on one file at a
 * time, through the same context menu the tree and the canvas use, and hands off
 * to the confirmation sheet. Offering "delete all selected" over unverified
 * candidates is exactly the mistake this whole file is written to avoid.
 */

import { ChevronRight, Copy, Eye, Info, Link2, Lock, Trash2, TriangleAlert } from "lucide-react";
import { Fragment, useState } from "react";

import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { formatCount, formatMtime, formatSI } from "@/lib/format";
import { cn } from "@/lib/utils";

/**
 * One candidate file.
 *
 * The logical size is not repeated per member: it is the cluster's grouping key
 * and identical for every member by definition. `allocated` is not — a sparse,
 * compressed, or cloned copy allocates less — and it is still not proof of
 * independent physical storage.
 *
 * Declared here rather than in `@/lib/ipc` on purpose: this file owns the view
 * shape, and the IPC layer maps the wire DTO onto it.
 */
export interface DupesMemberView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly mtime: number;
  readonly category: number;
  /** `nlink > 1` with no partner seen in this scan; deleting it may free nothing. */
  readonly hardLinked: boolean;
}

/** A set of files sharing one logical size. Worth comparing, not known to match. */
export interface DupesClusterView {
  readonly logicalBytes: number;
  /** Exact count, independent of how many rows `members` lists. */
  readonly memberCount: number;
  readonly members: readonly DupesMemberView[];
  readonly membersOmitted: number;
  /** Always `0` until contents are compared. Rendered, not assumed. */
  readonly potentialRecoveryLowerBytes: number;
  /** `logicalBytes × (memberCount − 1)`. An upper bound, never a promise. */
  readonly potentialRecoveryUpperBytes: number;
}

/** The bounded result of one candidate pass over a subtree. */
export interface DupesReportView {
  readonly clusters: readonly DupesClusterView[];
  readonly clustersFound: number;
  readonly clustersOmitted: number;
  readonly filesInClusters: number;
  /** Summed over every cluster *found*, so it does not shrink when the list is capped. */
  readonly potentialRecoveryUpperBytes: number;
  /** `false` until stages 3–5 exist. The copy on this screen branches on it. */
  readonly contentVerified: boolean;
  readonly clusterLimit: number;
  readonly memberLimit: number;
  readonly emptyFilesSkipped: number;
  readonly hardLinkRepeatsSkipped: number;
  /** Non-zero means clusters may be missing; the ones shown are still exact. */
  readonly filesUngrouped: number;
}

export interface DupesRouteProps {
  report: DupesReportView | undefined;
  isLoading: boolean;
  error: Error | null;
  /** Reveal one file in Finder. Same handler the tree and the canvas use. */
  onReveal?: (node: number) => void;
  /** Move one file to the Trash. Refused unless deletion is armed. */
  onTrash?: (node: number) => void;
  /** Whether deletion is armed; drives the Trash item's wording, not just its state. */
  trashEnabled?: boolean;
  className?: string;
}

/**
 * Clusters are keyed by their logical size, which is the grouping key and
 * therefore unique across the report. Node ids would work too but change between
 * generations, and this key survives a refetch so an open row stays open.
 */
function clusterKey(cluster: DupesClusterView): string {
  return String(cluster.logicalBytes);
}

export function DupesRoute({
  report,
  isLoading,
  error,
  onReveal,
  onTrash,
  trashEnabled = false,
  className,
}: DupesRouteProps) {
  const [expanded, setExpanded] = useState<string | null>(null);
  const [notesOpen, setNotesOpen] = useState(false);
  /** Cluster key -> the node id protected from deletion in that cluster. */
  const [kept, setKept] = useState<Record<string, number>>({});

  if (error !== null) {
    return (
      <div className={cn("p-6 text-sm text-pressure-critical", className)}>
        Duplicate candidates could not be computed: {error.message}
      </div>
    );
  }
  if (isLoading || report === undefined) {
    return <div className={cn("p-6 text-sm text-muted-foreground", className)}>Grouping by size…</div>;
  }

  const { clusters } = report;
  const verified = report.contentVerified;
  const widest = clusters.reduce((most, row) => Math.max(most, row.potentialRecoveryUpperBytes), 0);

  return (
    <div className={cn("flex min-h-0 flex-col gap-3 overflow-auto p-4", className)}>
      <header className="relative flex shrink-0 items-center gap-2">
        <h2 className="text-sm font-medium">{verified ? "Duplicates" : "Possible duplicates"}</h2>
        <span className="rds-numeric text-xs text-muted-foreground">
          {formatCount(report.clustersFound)} {report.clustersFound === 1 ? "group" : "groups"},{" "}
          {formatCount(report.filesInClusters)} files
        </span>
        <button
          type="button"
          aria-expanded={notesOpen}
          onClick={() => setNotesOpen((open) => !open)}
          title="How these groups are found"
          className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Info aria-hidden className="size-3.5" />
          <span className="sr-only">How these groups are found</span>
        </button>
        {notesOpen && (
          <div className="absolute left-0 top-full z-50 mt-1 flex w-[32rem] max-w-[80vw] flex-col gap-2 rounded-lg border border-border bg-popover p-3 text-xs text-muted-foreground shadow-xl">
            <p>
              These files were grouped by size alone. No file has been opened and no contents have
              been compared, so a group is a shortlist of things worth checking — not a set of
              copies.
            </p>
            <p>
              “Could free” is <strong>size × (copies − 1)</strong>, the most that could ever come
              back. If the files differ it is zero, and on APFS copies can already share their
              storage, in which case deleting one frees the name and nothing else.
            </p>
            <p>
              A second name for a file already counted (a hard link) is left out entirely, because
              deleting it recovers nothing. A file whose other names live outside this scan is
              marked <Link2 aria-hidden className="inline size-3 align-text-bottom" /> instead —
              nothing here can tell where those names are.
            </p>
            <p>
              Empty files are excluded. Every one of them is the same size, and none of them can
              free a byte.
            </p>
          </div>
        )}
      </header>

      {/* The one sentence a user must not miss, and it is not behind an icon. */}
      {!verified && (
        <p className="flex shrink-0 items-start gap-2 rounded-md border border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
          <TriangleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>
            Candidates only — these files share a size, which is not the same as sharing their
            contents. Confirming a duplicate means reading both files and comparing them, and that
            step has not run.
          </span>
        </p>
      )}

      {clusters.length === 0 ? (
        <div className="p-6 text-sm text-muted-foreground">
          No two files here share a size, so there is nothing to compare.
        </div>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-border/60 text-xs text-muted-foreground">
              <th scope="col" className="py-1.5 text-left font-normal">
                File size
              </th>
              <th scope="col" className="py-1.5 text-right font-normal">
                Copies
              </th>
              <th scope="col" className="py-1.5 text-right font-normal">
                Could free (upper bound)
              </th>
              <th scope="col" className="py-1.5 pl-3 text-left font-normal">
                Share of upside
              </th>
            </tr>
          </thead>
          <tbody>
            {clusters.map((cluster) => {
              const key = clusterKey(cluster);
              const open = expanded === key;
              return (
                <Fragment key={key}>
                  <tr
                    className={cn(
                      "cursor-pointer border-b border-border/30 hover:bg-accent/40",
                      open && "bg-accent/30",
                    )}
                    onClick={() => setExpanded(open ? null : key)}
                  >
                    <th scope="row" className="py-2 text-left font-normal">
                      <div className="flex items-center gap-1">
                        <ChevronRight
                          aria-hidden
                          className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")}
                        />
                        <span className="rds-numeric font-medium tabular-nums">
                          {formatSI(cluster.logicalBytes)}
                        </span>
                        <span className="text-xs text-muted-foreground">each</span>
                      </div>
                    </th>
                    <td className="rds-numeric py-2 text-right tabular-nums">
                      {formatCount(cluster.memberCount)}
                    </td>
                    <td className="rds-numeric py-2 text-right tabular-nums">
                      {/* "up to" is part of the value, not a tooltip. */}
                      <span className="text-muted-foreground">up to </span>
                      {formatSI(cluster.potentialRecoveryUpperBytes)}
                    </td>
                    <td className="py-2 pl-3">
                      <div className="h-1.5 w-24 overflow-hidden rounded-full bg-muted">
                        <div
                          className="h-full rounded-full bg-brand"
                          // Scaled against the largest upper bound, not the disk:
                          // this bar ranks the groups against each other and says
                          // nothing about the volume.
                          style={{
                            width:
                              widest > 0
                                ? `${(cluster.potentialRecoveryUpperBytes / widest) * 100}%`
                                : "0%",
                          }}
                        />
                      </div>
                    </td>
                  </tr>
                  {open && (
                    <tr className="border-b border-border/30">
                      <td colSpan={4} className="p-0">
                        <ClusterMembers
                          cluster={cluster}
                          keptNode={kept[key] ?? cluster.members[0]?.node ?? null}
                          onKeep={(node) => setKept((current) => ({ ...current, [key]: node }))}
                          onReveal={onReveal}
                          onTrash={onTrash}
                          trashEnabled={trashEnabled}
                        />
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      )}

      <footer className="flex shrink-0 flex-col gap-1 text-[11px] text-muted-foreground">
        <div>
          Across every group found: up to {formatSI(report.potentialRecoveryUpperBytes)} could be
          freed, and possibly none of it.
        </div>
        {report.clustersOmitted > 0 && (
          <div>
            Showing the {formatCount(clusters.length)} groups with the most upside; {" "}
            {formatCount(report.clustersOmitted)} more are not listed (cap{" "}
            {formatCount(report.clusterLimit)}). The total above counts all of them.
          </div>
        )}
        {(report.hardLinkRepeatsSkipped > 0 || report.emptyFilesSkipped > 0) && (
          <div>
            Excluded: {formatCount(report.hardLinkRepeatsSkipped)} repeated hard-link{" "}
            {report.hardLinkRepeatsSkipped === 1 ? "name" : "names"} (a second name for a file
            already counted frees nothing) and {formatCount(report.emptyFilesSkipped)} empty{" "}
            {report.emptyFilesSkipped === 1 ? "file" : "files"} (all the same size, none of them
            worth anything).
          </div>
        )}
        {report.filesUngrouped > 0 && (
          <div className="text-pressure-critical">
            {formatCount(report.filesUngrouped)} files could not be grouped: this subtree has more
            distinct file sizes than the size table holds. The groups above are exact, but some are
            missing.
          </div>
        )}
      </footer>
    </div>
  );
}

/**
 * The files in one candidate group.
 *
 * A listing, not a leaderboard: the point of opening a group is to see what is in
 * it. The list is capped by the backend and says what it dropped, and the kept
 * copy is protected from Trash so the group can never be emptied from here.
 */
function ClusterMembers({
  cluster,
  keptNode,
  onKeep,
  onReveal,
  onTrash,
  trashEnabled,
}: {
  cluster: DupesClusterView;
  keptNode: number | null;
  onKeep: (node: number) => void;
  onReveal?: (node: number) => void;
  onTrash?: (node: number) => void;
  trashEnabled: boolean;
}) {
  const key = clusterKey(cluster);

  if (cluster.members.length === 0) {
    return (
      <div className="px-4 py-3 text-xs text-muted-foreground">
        None of these {formatCount(cluster.memberCount)} files could have its path reconstructed, so
        none is listed — acting on a row that names no file is worse than a short list.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2 bg-background/60 px-4 py-3">
      <div className="text-xs text-muted-foreground">
        {/* The range, written out. A single number here reads as a promise. */}
        Range if these turn out to be copies: {formatSI(cluster.potentialRecoveryLowerBytes)} –{" "}
        {formatSI(cluster.potentialRecoveryUpperBytes)}. The floor is zero because no contents have
        been compared.
      </div>
      <ul className="flex flex-col">
        {cluster.members.map((member) => {
          const isKept = member.node === keptNode;
          return (
            <ContextMenu key={member.node}>
              <ContextMenuTrigger asChild>
                <li
                  className={cn(
                    "flex cursor-default items-baseline gap-3 rounded-sm border-b border-border/20 py-1 last:border-0 hover:bg-accent/40",
                    isKept && "bg-accent/20",
                  )}
                >
                  {/* One protected copy per group, always. This is the docs/06
                    * rule — a selection can never take a group's last member —
                    * expressed as state that cannot express the violation. */}
                  <label className="flex shrink-0 items-center gap-1 text-[11px] text-muted-foreground">
                    <input
                      type="radio"
                      name={`dupes-keep-${key}`}
                      checked={isKept}
                      onChange={() => onKeep(member.node)}
                      className="size-3 accent-brand"
                    />
                    <span className={cn("w-8", isKept ? "text-foreground" : "opacity-0")}>keep</span>
                  </label>
                  <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={member.path}>
                    {member.path}
                  </span>
                  {member.hardLinked && (
                    <span
                      className="flex shrink-0 items-center gap-1 text-[11px] text-muted-foreground"
                      title="Also reachable under another name that is not in this scan — deleting this one may free nothing."
                    >
                      <Link2 aria-hidden className="size-3" />
                      hard link
                    </span>
                  )}
                  <span
                    className="rds-numeric w-24 shrink-0 text-right text-xs tabular-nums"
                    title="Allocated bytes. Differs from the group's size for sparse, compressed, and cloned files."
                  >
                    {formatSI(member.allocated)}
                  </span>
                  <span className="rds-numeric w-24 shrink-0 text-right text-[11px] text-muted-foreground tabular-nums">
                    {formatMtime(member.mtime)}
                  </span>
                </li>
              </ContextMenuTrigger>
              <ContextMenuContent>
                <ContextMenuItem disabled={onReveal === undefined} onSelect={() => onReveal?.(member.node)}>
                  <Eye aria-hidden />
                  Reveal in Finder
                </ContextMenuItem>
                <ContextMenuItem onSelect={() => void navigator.clipboard.writeText(member.path)}>
                  <Copy aria-hidden />
                  Copy Path
                </ContextMenuItem>
                <ContextMenuSeparator />
                {/* Three reasons this can be refused, and the label says which. */}
                <ContextMenuItem
                  variant="destructive"
                  disabled={onTrash === undefined || !trashEnabled || isKept}
                  onSelect={() => onTrash?.(member.node)}
                >
                  {trashEnabled && !isKept ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
                  {isKept
                    ? "Move to Trash… (kept copy)"
                    : trashEnabled
                      ? "Move to Trash…"
                      : "Move to Trash… (deletion off)"}
                </ContextMenuItem>
              </ContextMenuContent>
            </ContextMenu>
          );
        })}
      </ul>
      <div className="flex flex-col gap-0.5 text-[11px] text-muted-foreground">
        {cluster.membersOmitted > 0 && (
          <div>
            {formatCount(cluster.members.length)} of {formatCount(cluster.memberCount)} shown;{" "}
            {formatCount(cluster.membersOmitted)} more are not listed. The count and the bound above
            cover all of them.
          </div>
        )}
        <div>
          The kept copy cannot be trashed from here. Removing the others is only safe once their
          contents have actually been compared.
        </div>
      </div>
    </div>
  );
}
