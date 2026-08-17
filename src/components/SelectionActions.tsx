/**
 * What you can do with a multi-selection.
 *
 * The app could already select many rows and then do nothing with them, which
 * is the worst of both worlds: the gesture works, so the user reasonably
 * expects the follow-through. This is the follow-through.
 *
 * It is a bar rather than a menu buried in a right-click because the whole
 * point of a bulk selection is that it is *large* — the user has just picked
 * out twelve things across a long list, and making them hunt for the verb
 * afterwards invites them to lose the selection on a stray click. The bar also
 * carries the count and the total size, because "12 items · 47 GB" is the
 * number that decides whether the action is worth doing at all, and computing
 * it in your head from a scrolled list is not reasonable.
 *
 * Destructive verbs stay behind the app-wide arming switch, exactly as they do
 * in the details panel. A bulk action is *more* dangerous than a single one,
 * not less, so it does not get a shortcut around the policy.
 */

import { ArrowRightLeft, Eye, Lock, Trash2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { formatSI } from "@/lib/format";
import { cn } from "@/lib/utils";

export interface SelectionActionsProps {
  /** How many rows are selected. Zero renders nothing. */
  count: number;
  /** Their combined size, in the metric currently on screen. */
  bytes: number;
  /** Whether the destructive verbs are live. */
  armed: boolean;
  onMove?: () => void;
  onReveal?: () => void;
  onTrash?: () => void;
  onClear: () => void;
  className?: string;
}

export function SelectionActions({
  count,
  bytes,
  armed,
  onMove,
  onReveal,
  onTrash,
  onClear,
  className,
}: SelectionActionsProps) {
  // One row selected is just a selection; the details panel already speaks for
  // it. The bar earns its space only once the action is genuinely plural.
  if (count < 2) return null;

  return (
    <div
      role="toolbar"
      aria-label="Actions for the selected items"
      className={cn(
        "flex shrink-0 items-center gap-2 border-b border-border/60 bg-accent/40 px-3 py-1.5",
        className,
      )}
    >
      <span className="text-xs font-medium">
        {count.toLocaleString()} selected
        <span className="ml-1.5 font-normal text-muted-foreground">{formatSI(bytes)}</span>
      </span>

      <div className="ml-auto flex items-center gap-1">
        <Button
          variant="outline"
          size="sm"
          disabled={onMove === undefined || !armed}
          title={
            armed
              ? "Copy these to another folder, verify them, then leave links behind"
              : "Moving is off. Arm destructive actions in the details panel to enable it."
          }
          onClick={onMove}
        >
          {armed ? <ArrowRightLeft aria-hidden /> : <Lock aria-hidden />}
          Move…
        </Button>

        <Button variant="ghost" size="sm" disabled={onReveal === undefined} onClick={onReveal}>
          <Eye aria-hidden />
          Reveal
        </Button>

        <Button
          variant="ghost"
          size="sm"
          disabled={onTrash === undefined || !armed}
          title={armed ? undefined : "Deletion is off. Arm it in the details panel."}
          onClick={onTrash}
        >
          {armed ? <Trash2 aria-hidden /> : <Lock aria-hidden />}
          Trash…
        </Button>

        <Button variant="ghost" size="sm" onClick={onClear} title="Clear the selection (Esc)">
          <X aria-hidden />
          <span className="sr-only">Clear selection</span>
        </Button>
      </div>
    </div>
  );
}
