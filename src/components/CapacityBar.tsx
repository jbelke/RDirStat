/**
 * The segmented capacity bar for the volume picker.
 *
 * The whole reason this component is not a one-line progress bar is the
 * arithmetic in docs/05-UI.md:
 *
 *   "930 GB used plus 26 GB available does not reach 995 GB, and the 39 GB gap
 *    is purgeable space macOS has not yet reclaimed. Purgeable is therefore
 *    drawn *inside* capacity, never added beside it — segments that sum past
 *    the total are how a capacity UI loses trust."
 *
 * So the three segments are derived, not summed:
 *
 * - `available`   = `statfs` available bytes — free right now.
 * - `purgeable`   = `importantAvailable - available`, clamped at zero. macOS
 *                   would hand this back under pressure but has not yet.
 * - `used`        = `total - importantAvailable`, i.e. everything left. This is
 *                   the residual on purpose: it can never push the bar past
 *                   100%, because it is defined as what the other two are not.
 *
 * When the platform gives no "available for important usage" figure the middle
 * segment simply does not exist and `used = total - available`. No estimate is
 * invented for it.
 *
 * Local Time Machine snapshots get a badge, not a segment: `tmutil` publishes
 * no authoritative byte total to subtract from `statfs`, and drawing a guess
 * would be the exact failure this component exists to avoid.
 */

import { capacitySegments, pressureClass } from "@/lib/capacity";
import { formatSI } from "@/lib/format";
import { cn } from "@/lib/utils";

// The arithmetic lives in `@/lib/capacity` so it can be exercised without a
// DOM. Re-exported here because every existing call site imports it from the
// component it belongs to.
export { capacitySegments, pressureClass };
export type { CapacitySegments } from "@/lib/capacity";

export interface CapacityBarProps {
  total: number;
  available: number;
  importantAvailable: number | null;
  /** Renders the `▓ used / ▒ purgeable / ░ available` key under the bar. */
  showLegend?: boolean;
  className?: string;
}

export function CapacityBar({
  total,
  available,
  importantAvailable,
  showLegend = true,
  className,
}: CapacityBarProps) {
  const segments = capacitySegments(total, available, importantAvailable);
  const percent = (value: number) => (segments.total > 0 ? (value / segments.total) * 100 : 0);
  const usedPercent = percent(segments.used);

  return (
    <div className={cn("flex flex-col gap-1.5", className)}>
      <div
        role="meter"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(usedPercent)}
        aria-valuetext={`${formatSI(segments.used)} of ${formatSI(segments.total)} used`}
        className="flex h-2.5 w-full overflow-hidden rounded-full bg-capacity-free"
      >
        <div
          className={cn("h-full", pressureClass(segments.pressure))}
          style={{ width: `${usedPercent}%` }}
        />
        {segments.purgeable > 0 && (
          <div
            className="h-full bg-capacity-purgeable"
            style={{ width: `${percent(segments.purgeable)}%` }}
          />
        )}
      </div>

      {showLegend && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <LegendSwatch className={pressureClass(segments.pressure)}>
            used {formatSI(segments.used)}
          </LegendSwatch>
          {segments.purgeable > 0 && (
            <LegendSwatch className="bg-capacity-purgeable">
              {formatSI(segments.purgeable)} purgeable
            </LegendSwatch>
          )}
          <LegendSwatch className="bg-capacity-free">
            {formatSI(segments.available)} available
          </LegendSwatch>
        </div>
      )}
    </div>
  );
}

function LegendSwatch({ className, children }: { className: string; children: React.ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span aria-hidden className={cn("size-2 rounded-[2px]", className)} />
      {children}
    </span>
  );
}
