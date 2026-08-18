/**
 * The details panel.
 *
 * docs/05-UI.md: "**Details is a panel, not a dialog.** Selection-driven,
 * always present." and "Destructive actions live in the details panel and the
 * context menu only, so Trash is never one stray click away from a
 * multi-selection."
 *
 * **"Always present" is now "always available".** The panel slides in over the
 * content by default and can be pinned to dock it, because selection-driven and
 * always-on-screen turn out to conflict: for most of a session there is no
 * selection, and a permanent 320-point column reading "select an item" spends
 * fixed width on a transient answer. Pinning restores the docked behaviour for
 * anyone reading details continuously. Nothing about *what* the panel shows,
 * or about the authority rules below, changes with it — a slid-in panel is the
 * same panel in a different place, still not a dialog: it takes no focus trap,
 * dims nothing, and never blocks the tree behind it.
 *
 * Four rules the markup enforces:
 *
 * 1. **Logical and allocated are shown side by side and never summed.** They
 *    are two measurements of the same object, not two halves of one number.
 * 2. **The path is display-only.** `DisplayPath` percent-escapes bytes that are
 *    not valid UTF-8 (macOS names are NFD byte strings and need not be UTF-8 at
 *    all), so it round-trips for *reading* but is explicitly not action
 *    authority. Every action goes back to Rust as a `NodeId` and is
 *    re-validated against the recorded dev/ino/kind there.
 * 3. **The virtual `<Files>` group cannot be revealed or trashed.** It has no
 *    directory entry. The panel says so instead of disabling a button silently.
 * 4. **The Trash drop zone is persistent**, per docs/05's "Deletion is a drop
 *    target", and it states scan-time bytes with the APFS caveat rather than
 *    promising recovered space.
 * 5. **Deletion is off until it is armed.** The drop target is persistent, but
 *    it is not *live* until the user flips the switch above it, and the switch
 *    goes back off by itself on every new scan (see `state/store.ts`). A drop
 *    zone that is always hot sits one careless drag away from moving a folder
 *    the user only meant to inspect; the arming gesture is the effort that buys
 *    the difference back. While disarmed the zone refuses the drag outright —
 *    `dropEffect = "none"` — so the cursor says no before the mouse is
 *    released.
 */

import { Check, CircleAlert, Copy, Eye, Fingerprint, HardDriveDownload, Loader, Lock, Package, Pin, PinOff, Trash2, X } from "lucide-react";
import { useState } from "react";

import { CategoryChip } from "@/components/cells/CategoryChip";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { formatCount, formatMtime, formatSI } from "@/lib/format";
import { useFileDigest, useNodeDetails, useStructureDigest } from "@/lib/queries";
import { cn } from "@/lib/utils";
import { FLAGS, hasFlag, INCOMPLETE_SUBTREE, isVirtualGroup } from "@/lib/wire";

export interface DetailsPanelProps {
  generation: number;
  /** The sole selection, or `null` for none / multiple. */
  node: number | null;
  selectionCount: number;
  onReveal?: (node: number) => void;
  onTrash?: (nodes: number[]) => void;
  /** Opens the move-to-another-volume flow for one node. */
  onRelocate?: (node: number) => void;
  /** Nodes dropped onto the trash zone. Same authority path as the button. */
  onTrashDropped?: (nodes: number[]) => void;
  /** Whether the destructive actions are live. Off by default; see the store. */
  deletionArmed?: boolean;
  /** Flips the arming. Omitted means the switch is not offered at all. */
  onArmDeletion?: (armed: boolean) => void;
  /** Docked rather than sliding over the content. */
  pinned?: boolean;
  /** Flips the pin. Omitted means the pin control is not offered. */
  onTogglePin?: () => void;
  /** Dismisses a sliding panel. Omitted while pinned, where there is nothing to dismiss. */
  onClose?: () => void;
  className?: string;
}

export function DetailsPanel({
  generation,
  node,
  selectionCount,
  onReveal,
  onTrash,
  onRelocate,
  onTrashDropped,
  deletionArmed = false,
  onArmDeletion,
  pinned = false,
  onTogglePin,
  onClose,
  className,
}: DetailsPanelProps) {
  const { data, error, isLoading } = useNodeDetails(generation, node);

  return (
    <aside
      aria-label="Details"
      className={cn(
        "flex min-h-0 w-80 shrink-0 flex-col gap-4 border-l border-border/60 p-4",
        className,
      )}
    >
      {(onTogglePin !== undefined || onClose !== undefined) && (
        <header className="-mt-1 flex shrink-0 items-center gap-1">
          <h2 className="flex-1 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Details
          </h2>
          {onTogglePin !== undefined && (
            <button
              type="button"
              onClick={onTogglePin}
              // aria-pressed rather than two labels: it is one control with a
              // state, and a screen reader should hear the state rather than
              // have to infer it from the verb changing.
              aria-pressed={pinned}
              title={pinned ? "Unpin — let the panel slide away" : "Pin — keep the panel open"}
              className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              {pinned ? (
                <Pin aria-hidden className="size-3.5 text-foreground" />
              ) : (
                <PinOff aria-hidden className="size-3.5" />
              )}
              <span className="sr-only">{pinned ? "Unpin details" : "Pin details open"}</span>
            </button>
          )}
          {/* No close button while pinned: pinning IS "stay open", so offering
            * a dismissal beside it asks the user to reconcile two controls that
            * disagree. Unpin is the way out of a pinned panel. */}
          {onClose !== undefined && !pinned && (
            <button
              type="button"
              onClick={onClose}
              title="Close (Esc)"
              className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <X aria-hidden className="size-3.5" />
              <span className="sr-only">Close details</span>
            </button>
          )}
        </header>
      )}
      {node === null && (
        <p className="text-sm text-muted-foreground">
          {selectionCount > 1
            ? `${selectionCount} items selected. Select a single item to see its details.`
            : "Select an item in the tree or the chart."}
        </p>
      )}

      {node !== null && isVirtualGroup(node) && (
        <Alert>
          <CircleAlert aria-hidden />
          <AlertTitle>Files group</AlertTitle>
          <AlertDescription>
            This row aggregates the files directly inside its directory. It is not a filesystem
            entry, so it cannot be revealed or moved to the Trash.
          </AlertDescription>
        </Alert>
      )}

      {isLoading && node !== null && (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader aria-hidden className="size-3.5 animate-spin" />
          Loading details…
        </p>
      )}

      {error !== null && (
        <Alert variant="destructive">
          <CircleAlert aria-hidden />
          <AlertTitle>Details unavailable</AlertTitle>
          <AlertDescription>{error.message}</AlertDescription>
        </Alert>
      )}

      {data !== undefined && (
        <>
          <header className="flex flex-col gap-1">
            <h2 className="break-words text-base font-medium leading-tight" data-selectable>
              {data.name}
            </h2>
            <p className="break-all font-mono text-[11px] text-muted-foreground" data-selectable>
              {data.path}
            </p>
          </header>

          <div className="grid grid-cols-2 gap-3">
            <Metric label="Logical" value={formatSI(data.logical)} />
            <Metric label="Allocated" value={formatSI(data.allocated)} />
          </div>

          <dl className="flex flex-col gap-2 text-xs">
            <Row label="Category">
              <CategoryChip category={data.category} />
            </Row>
            <Row label="Kind">
              <span className="capitalize">{data.kind.replace(/_/g, " ")}</span>
            </Row>
            <Row label="Modified">{formatMtime(data.mtime, true)}</Row>
            {data.isPackage && (
              <Row label="Package">
                <span className="inline-flex items-center gap-1">
                  <Package aria-hidden className="size-3" /> macOS package
                </span>
              </Row>
            )}
            {data.countedAt !== null && (
              <Row label="Counted at">
                <span className="break-all font-mono">{data.countedAt}</span>
              </Row>
            )}
          </dl>

          {data.subtree !== null && (
            <dl className="flex flex-col gap-2 rounded-md border border-border/60 p-3 text-xs">
              <Row label="Subtree logical">{formatSI(data.subtree.logical)}</Row>
              <Row label="Subtree allocated">{formatSI(data.subtree.allocated)}</Row>
              <Row label="Direct files">{formatCount(data.subtree.directFiles)}</Row>
              <Row label="Entries observed">{formatCount(data.subtree.observedEntries)}</Row>
              {data.subtree.unreadable > 0 && (
                <Row label="Unreadable">
                  <span className="text-pressure-warn">{formatCount(data.subtree.unreadable)}</span>
                </Row>
              )}
            </dl>
          )}

          <DigestSection
            key={data.node}
            generation={generation}
            node={data.node}
            isFile={data.kind === "file"}
          />

          {hasFlag(data.flags, INCOMPLETE_SUBTREE) && (
            <Alert variant="warning">
              <CircleAlert aria-hidden />
              <AlertTitle>These totals are a floor</AlertTitle>
              <AlertDescription>
                {describeIncomplete(data.flags)} Everything below this point is at least this large,
                and may be larger.
              </AlertDescription>
            </Alert>
          )}

          <div className="flex gap-2">
            <Button
              variant="outline"
              size="sm"
              className="flex-1"
              disabled={onReveal === undefined || isVirtualGroup(data.node)}
              onClick={() => onReveal?.(data.node)}
            >
              <Eye aria-hidden />
              Reveal
            </Button>
            <Button
              variant="destructive"
              size="sm"
              className="flex-1"
              disabled={onTrash === undefined || isVirtualGroup(data.node) || !deletionArmed}
              title={
                deletionArmed
                  ? undefined
                  : "Deletion is off. Arm it below to move items to the Trash."
              }
              onClick={() => onTrash?.([data.node])}
            >
              {deletionArmed ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
              Trash…
            </Button>
          </div>

          {/* Deleting is not the only answer to "this is too big". Very often
            * the user wants the bytes, just not on this volume — so the move
            * offer sits directly beneath Trash, at the same weight, rather
            * than being buried in a context menu. It ends by disposing of the
            * original, so it is behind the same arming switch. */}
          <Button
            variant="outline"
            size="sm"
            className="w-full"
            disabled={onRelocate === undefined || isVirtualGroup(data.node) || !deletionArmed}
            title={
              deletionArmed
                ? "Copy this to another volume, verify it, then leave a link behind"
                : "Moving is off. Arm destructive actions below to enable it."
            }
            onClick={() => onRelocate?.(data.node)}
          >
            {deletionArmed ? <HardDriveDownload aria-hidden /> : <Lock aria-hidden />}
            Move to another volume…
          </Button>
        </>
      )}

      <div className="mt-auto flex flex-col gap-2">
        <DeletionArmSwitch armed={deletionArmed} onChange={onArmDeletion} />
        <TrashDropZone armed={deletionArmed} onDrop={onTrashDropped} />
      </div>
    </aside>
  );
}

/**
 * The arming switch for every destructive action in the app.
 *
 * A plain checkbox with a switch role rather than a shadcn `Switch`: the
 * component set this build copied in does not include one, and the semantics
 * that matter here — a real focusable control, a real checked state, a label
 * that says what happens — are the input's, not the styling's.
 */
function DeletionArmSwitch({ armed, onChange }: { armed: boolean; onChange?: (armed: boolean) => void }) {
  return (
    <label
      className={cn(
        "flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-2 transition-colors",
        armed ? "border-destructive/60 bg-destructive/10" : "border-border",
        onChange === undefined && "cursor-not-allowed opacity-60",
      )}
    >
      <input
        type="checkbox"
        role="switch"
        checked={armed}
        disabled={onChange === undefined}
        onChange={(event) => onChange?.(event.currentTarget.checked)}
        className="size-4 shrink-0 accent-destructive"
      />
      <span className="min-w-0 flex-1">
        <span className={cn("block text-xs font-medium", armed ? "text-destructive" : "text-foreground")}>
          {armed ? "Deletion armed" : "Deletion off"}
        </span>
        <span className="block text-[10px] leading-tight text-muted-foreground">
          {armed
            ? "Trash and the drop target are live until the next scan."
            : "Trash and the drop target are inert until you switch this on."}
        </span>
      </span>
    </label>
  );
}

function describeIncomplete(flags: number): string {
  const causes: string[] = [];
  if (hasFlag(flags, FLAGS.UNREADABLE)) causes.push("part of it could not be opened");
  if (hasFlag(flags, FLAGS.EXCLUDED)) causes.push("part of it was excluded");
  if (hasFlag(flags, FLAGS.INCOMPLETE)) causes.push("the scan did not finish it");
  if (hasFlag(flags, FLAGS.AGGREGATED)) causes.push("small entries below it were aggregated");
  if (causes.length === 0) return "";
  return `${causes.join(", ")}.`.replace(/^./, (c) => c.toUpperCase());
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border border-border/60 px-3 py-2">
      <div className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="rds-numeric text-sm font-medium">{value}</div>
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-3">
      <dt className="shrink-0 text-muted-foreground">{label}</dt>
      <dd className="min-w-0 text-right">{children}</dd>
    </div>
  );
}

/**
 * The persistent Trash drop target.
 *
 * It accepts a drag carrying `application/x-rdirstat-nodes` — a JSON array of
 * NodeIds. Ids, not paths: a path string pulled out of the DOM is not
 * authority, and the backend re-validates each id against the generation before
 * it touches the filesystem. Dropping does not delete; it opens the
 * confirmation flow, which obtains a token bound to the current generation.
 *
 * **Persistent is not the same as live.** While `armed` is false the zone
 * refuses the drag (`dropEffect = "none"`, no `preventDefault`), so the drop
 * never fires and the cursor shows the refusal during the drag rather than
 * after it. The zone stays visible either way: hiding it would make the
 * capability feel missing instead of switched off.
 */
export const NODE_DRAG_MIME = "application/x-rdirstat-nodes";

function TrashDropZone({ armed, onDrop }: { armed: boolean; onDrop?: (nodes: number[]) => void }) {
  const [active, setActive] = useState(false);

  return (
    <div
      aria-disabled={!armed}
      onDragOver={(event) => {
        if (!event.dataTransfer.types.includes(NODE_DRAG_MIME)) return;
        if (!armed) {
          // No preventDefault: the browser's default is "reject this drop".
          event.dataTransfer.dropEffect = "none";
          return;
        }
        event.preventDefault();
        event.dataTransfer.dropEffect = "move";
        setActive(true);
      }}
      onDragLeave={() => setActive(false)}
      onDrop={(event) => {
        event.preventDefault();
        setActive(false);
        if (!armed) return;
        const raw = event.dataTransfer.getData(NODE_DRAG_MIME);
        if (raw.length === 0) return;
        try {
          const parsed: unknown = JSON.parse(raw);
          if (!Array.isArray(parsed)) return;
          const nodes = parsed.filter((value): value is number => typeof value === "number");
          if (nodes.length > 0) onDrop?.(nodes);
        } catch {
          // A malformed payload is a bug in the drag source, not user input.
        }
      }}
      className={cn(
        "flex flex-col items-center gap-1 rounded-lg border border-dashed px-4 py-6 text-center text-xs transition-colors",
        !armed && "border-border/60 text-muted-foreground/50",
        armed && !active && "border-border text-muted-foreground",
        armed && active && "border-destructive bg-destructive/10 text-destructive",
      )}
    >
      {armed ? <Trash2 aria-hidden className="size-5" /> : <Lock aria-hidden className="size-5" />}
      <span>
        {armed
          ? "Drag items here to move them to the Trash"
          : "Deletion is off — this target will not accept a drop"}
      </span>
      <span className="text-[10px]">
        {armed
          ? "Finder’s Put Back keeps working. Recovered space can be smaller than the reported size because APFS shares blocks."
          : "Switch deletion on above if you mean to remove something."}
      </span>
    </div>
  );
}

/**
 * The digest row.
 *
 * A file and a directory are asked different questions here, and the row says
 * which one it answered rather than printing a bare hash. A file gets SHA-256
 * of its bytes — the digest `shasum -a 256` agrees with — and only when asked,
 * because reading the file costs what the file costs. A directory gets a fold
 * over the shape of its subtree, which reads no file data and returns
 * instantly, and is labelled "Structure" with the scope it covered.
 *
 * The label is the point. An unlabelled digest on a directory would let a
 * hash of names and sizes pass for a hash of contents, which would claim two
 * trees hold the same *data* when all that matched was their listings.
 *
 * Mounted with `key={node}` by the caller, so selecting a different file clears
 * a computed hash instead of showing the previous file's.
 */
function DigestSection({ generation, node, isFile }: { generation: number; node: number; isFile: boolean }) {
  const structure = useStructureDigest(generation, isFile ? null : node);
  const contents = useFileDigest();

  if (isFile) {
    const result = contents.data;
    return (
      <dl className="flex flex-col gap-2 rounded-md border border-border/60 p-3 text-xs">
        <Row label="SHA-256">
          {result === undefined ? (
            contents.isPending ? (
              <span className="inline-flex items-center gap-1 text-muted-foreground">
                <Loader aria-hidden className="size-3 animate-spin" /> hashing…
              </span>
            ) : (
              <Button size="sm" variant="outline" onClick={() => contents.mutate({ generation, node })}>
                <Fingerprint aria-hidden className="size-3" /> Compute
              </Button>
            )
          ) : (
            <DigestValue digest={result.digest} />
          )}
        </Row>
        {result !== undefined && (
          <p className="text-right text-[10px] text-muted-foreground">contents · {formatSI(result.bytes)} read</p>
        )}
        {contents.error !== null && (
          <p className="text-right text-[10px] text-pressure-warn">{contents.error.message}</p>
        )}
      </dl>
    );
  }

  const view = structure.data;
  return (
    <dl className="flex flex-col gap-2 rounded-md border border-border/60 p-3 text-xs">
      <Row label="Structure">
        {view === undefined ? (
          <span className="text-muted-foreground">{structure.isLoading ? "…" : "—"}</span>
        ) : (
          <DigestValue digest={view.digest} />
        )}
      </Row>
      {view !== undefined && (
        <p className="text-right text-[10px] leading-relaxed text-muted-foreground">
          {formatCount(view.entries)} entries · names, sizes, mtimes
          <br />
          not a content hash
        </p>
      )}
      {view?.truncated === true && (
        <p className="text-right text-[10px] text-pressure-warn">
          partial walk — not comparable with a whole-tree digest
        </p>
      )}
    </dl>
  );
}

/**
 * A digest, shortened for the panel and copyable in full.
 *
 * Shortened because 64 hex characters do not fit a 320px panel without
 * wrapping into an unreadable block, and nobody reads a SHA-256 by eye anyway —
 * they compare a few characters, or they copy it. The whole value is on the
 * `title` and on the clipboard; only the display is abbreviated.
 */
function DigestValue({ digest }: { digest: string }) {
  const [copied, setCopied] = useState(false);

  return (
    <span className="inline-flex items-center gap-1">
      <span className="font-mono" title={digest}>
        {digest.slice(0, 8)}…{digest.slice(-4)}
      </span>
      <button
        type="button"
        title="Copy the whole digest"
        onClick={() => {
          void navigator.clipboard.writeText(digest).then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1_200);
          });
        }}
        className="rounded p-0.5 opacity-70 hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        {copied ? <Check aria-hidden className="size-3" /> : <Copy aria-hidden className="size-3" />}
        <span className="sr-only">Copy the full digest</span>
      </button>
    </span>
  );
}
