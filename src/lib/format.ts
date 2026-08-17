/**
 * Byte and percentage formatting — a faithful mirror of `rdirstat_core`'s
 * `format_si` / `format_iec` / `format_percent`.
 *
 * Why mirror instead of asking Rust? Because every one of these is called once
 * per visible row per render. Round-tripping a string through IPC for a number
 * already on the wire would put an await inside a virtualized scroll.
 *
 * The rules, copied from the frozen contract so the two implementations can be
 * diffed by eye:
 *
 * - **Decimal SI by default.** macOS Finder is decimal; a disagreeing number
 *   reads as a bug (docs/05-UI.md).
 * - Three significant digits chosen from the **unrounded** integer part:
 *   `<10` -> 2 decimals, `<100` -> 1, else 0.
 * - Counts below the first threshold print exactly: `999 B`, not `1.00 kB`.
 * - Rounding is **half away from zero** and **carries units**:
 *   `999_600_000_000` -> `1.00 TB`, never `1000 GB`.
 * - No float ever touches a byte count. Rust uses `u128`; this uses `BigInt`.
 *
 * The float ban is not pedantry. `0.1 + 0.2` aside, a `f64` loses integer
 * precision above 2^53 and the app is expected to display allocated totals for
 * an 8 TB volume — well inside `f64` today, but the moment someone sums two of
 * them into a "total across volumes" the guarantee is gone. Integer math has no
 * such cliff.
 */

const SI_UNITS = ["B", "kB", "MB", "GB", "TB", "PB", "EB"] as const;
const IEC_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] as const;

const POW10 = [1n, 10n, 100n];

/**
 * Round `numerator / denominator` to the nearest integer, halves away from
 * zero. Both arguments must be non-negative; byte counts are `u64` in Rust and
 * cannot be negative.
 */
function roundHalfAwayFromZero(numerator: bigint, denominator: bigint): bigint {
  const quotient = numerator / denominator;
  const remainder = numerator % denominator;
  return remainder * 2n >= denominator ? quotient + 1n : quotient;
}

function formatScaled(bytes: bigint, base: bigint, units: readonly string[]): string {
  if (bytes < base) {
    return `${bytes.toString()} ${units[0]}`;
  }

  // Largest unit whose threshold the value reaches, capped at the table.
  let exponent = 0;
  let scale = 1n;
  while (exponent + 1 < units.length && bytes / (scale * base) > 0n) {
    scale *= base;
    exponent += 1;
  }

  for (;;) {
    const integerPart = bytes / scale;
    const decimals = integerPart < 10n ? 2 : integerPart < 100n ? 1 : 0;
    const factor = POW10[decimals];
    const scaled = roundHalfAwayFromZero(bytes * factor, scale);

    // Rounding can carry past the unit: 999.6 GB must print as 1.00 TB, not
    // "1000 GB". Retry one unit up rather than emit a four-digit mantissa.
    if (scaled >= base * factor && exponent + 1 < units.length) {
      scale *= base;
      exponent += 1;
      continue;
    }

    const unit = units[exponent];
    if (decimals === 0) {
      return `${scaled.toString()} ${unit}`;
    }
    const whole = scaled / factor;
    const fraction = (scaled % factor).toString().padStart(decimals, "0");
    return `${whole.toString()}.${fraction} ${unit}`;
  }
}

function toBigInt(value: number | bigint | string): bigint {
  if (typeof value === "bigint") return value < 0n ? 0n : value;
  if (typeof value === "string") {
    try {
      const parsed = BigInt(value);
      return parsed < 0n ? 0n : parsed;
    } catch {
      return 0n;
    }
  }
  if (!Number.isFinite(value) || value <= 0) return 0n;
  return BigInt(Math.round(value));
}

/**
 * Decimal SI — the default everywhere in the UI, because Finder is decimal.
 *
 * `formatSI(0) === "0 B"`, `formatSI(999) === "999 B"`,
 * `formatSI(1000) === "1.00 kB"`, `formatSI(2_410_000_000) === "2.41 GB"`.
 */
export function formatSI(bytes: number | bigint | string): string {
  return formatScaled(toBigInt(bytes), 1000n, SI_UNITS);
}

/**
 * Binary IEC. Reserved for the settings toggle and the memory budget readout;
 * it is never the default, and it always labels its unit.
 */
export function formatIEC(bytes: number | bigint | string): string {
  return formatScaled(toBigInt(bytes), 1024n, IEC_UNITS);
}

/**
 * `part / whole` as `"33.3%"`, one decimal, half away from zero.
 * A zero denominator is `"0.0%"` — a share of nothing is not an error.
 */
export function formatPercent(part: number | bigint | string, whole: number | bigint | string): string {
  const numerator = toBigInt(part);
  const denominator = toBigInt(whole);
  if (denominator === 0n) return "0.0%";
  const tenths = roundHalfAwayFromZero(numerator * 1000n, denominator);
  const units = tenths / 10n;
  const fraction = tenths % 10n;
  return `${units.toString()}.${fraction.toString()}%`;
}

/**
 * The unitless ratio used for bar widths. Clamped to `[0, 1]` because a
 * partial subtree can legitimately report a child larger than the parent's
 * recorded total (see `flags::INCOMPLETE_SUBTREE`), and a 140%-wide bar is a
 * rendering bug rather than information.
 */
export function shareOf(part: number, whole: number): number {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return 0;
  const ratio = part / whole;
  if (ratio <= 0) return 0;
  return ratio >= 1 ? 1 : ratio;
}

const DATE_FORMAT = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "2-digit",
});

const DATE_TIME_FORMAT = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
});

/**
 * `mtime` is Unix **seconds** and **signed** — pre-1970 timestamps exist on
 * real disks and are not corruption. `i64::MIN` is the sentinel `DirTotals`
 * uses for "nothing observed"; anything at or below the JS date range renders
 * as an em dash rather than "Invalid Date".
 */
export function formatMtime(unixSeconds: number | bigint | string, withTime = false): string {
  const seconds = typeof unixSeconds === "number" ? unixSeconds : Number(unixSeconds);
  if (!Number.isFinite(seconds)) return "—";
  const millis = seconds * 1000;
  // The widest range `Date` can represent, per ECMA-262.
  if (millis < -8.64e15 || millis > 8.64e15) return "—";
  const date = new Date(millis);
  if (Number.isNaN(date.getTime())) return "—";
  return withTime ? DATE_TIME_FORMAT.format(date) : DATE_FORMAT.format(date);
}

/** `41` -> `"0:41"`, `3_661` -> `"1:01:01"`. Used by the scan progress strip. */
export function formatDuration(millis: number): string {
  if (!Number.isFinite(millis) || millis < 0) return "0:00";
  const total = Math.floor(millis / 1000);
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3600);
  const paddedSeconds = seconds.toString().padStart(2, "0");
  if (hours > 0) {
    return `${hours}:${minutes.toString().padStart(2, "0")}:${paddedSeconds}`;
  }
  return `${minutes}:${paddedSeconds}`;
}

const COMPACT_COUNT = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});

/** `12_000_000` -> `"12M"`. Entry counts in the progress strip and stat tiles. */
export function formatCount(count: number | bigint | string): string {
  const value = typeof count === "number" ? count : Number(count);
  if (!Number.isFinite(value)) return "0";
  return COMPACT_COUNT.format(value);
}
