//! Byte-size formatting.
//!
//! **The default is decimal SI**: `1 kB = 1000 B`, `1 GB = 10^9 B`. macOS
//! Finder reports decimal SI, and a number that disagrees with the Finder
//! window next to it reads as a bug rather than as a different convention
//! (docs/05-UI.md#color-and-formatting). IEC is available behind a setting and
//! is the unit the memory budget is stated in, because "GB" is too ambiguous
//! for a memory gate (docs/00-OVERVIEW.md#success-criteria).
//!
//! Logical and allocated bytes are formatted by the same function but are never
//! summed together, reconciled, or presented as one number. They answer
//! different questions and APFS makes them genuinely disagree.
//!
//! # Precision
//!
//! Three significant digits, chosen from the *unrounded* integer part:
//!
//! | Integer part | Decimals | Example        |
//! | ------------ | -------: | -------------- |
//! | `< 10`       | 2        | `2.41 GB`      |
//! | `< 100`      | 1        | `18.2 GB`      |
//! | otherwise    | 0        | `926 GB`       |
//!
//! Byte counts below the first unit threshold print exactly, with no decimals
//! (`999 B`). Rounding is half-away-from-zero and carries into the next unit,
//! so `999_600_000_000` renders as `1.00 TB`, never `1000 GB`. Trailing zeros
//! are kept, so widths are stable in a right-aligned column.
//!
//! All arithmetic is integer arithmetic in `u128`. Nothing here converts a byte
//! count to a float, so no rounding is ever surprising and
//! `clippy::cast_possible_truncation` has nothing to catch.

const SI_UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
const IEC_UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// Formats `bytes` in decimal SI, e.g. `4.13 TB`. This is the product default.
///
/// ```
/// # use rdirstat_core::format_si;
/// assert_eq!(format_si(0), "0 B");
/// assert_eq!(format_si(999), "999 B");
/// assert_eq!(format_si(1_000), "1.00 kB");
/// assert_eq!(format_si(2_410_000_000), "2.41 GB");
/// assert_eq!(format_si(926_000_000_000), "926 GB");
/// assert_eq!(format_si(999_600_000_000), "1.00 TB");
/// ```
#[must_use]
pub fn format_si(bytes: u64) -> String {
    format_scaled(bytes, 1000, &SI_UNITS)
}

/// Formats `bytes` in binary IEC, e.g. `3.76 TiB`.
///
/// Reserved for the memory budget and for the explicit "show binary units"
/// setting. It is not the default, because Finder is not binary.
///
/// ```
/// # use rdirstat_core::format_iec;
/// assert_eq!(format_iec(1024), "1.00 KiB");
/// assert_eq!(format_iec(5 * 1024 * 1024 * 1024), "5.00 GiB");
/// ```
#[must_use]
pub fn format_iec(bytes: u64) -> String {
    format_scaled(bytes, 1024, &IEC_UNITS)
}

fn format_scaled(bytes: u64, base: u128, units: &[&str; 7]) -> String {
    let value = u128::from(bytes);
    if value < base {
        return format!("{value} {}", units[0]);
    }

    // Pick the largest unit whose divisor still leaves an integer part >= 1.
    let mut unit = 0_usize;
    let mut divisor: u128 = 1;
    while unit + 1 < units.len() && value / (divisor * base) > 0 {
        divisor *= base;
        unit += 1;
    }

    loop {
        let whole = value / divisor;
        let decimals: u32 = match whole {
            0..10 => 2,
            10..100 => 1,
            _ => 0,
        };
        let scale = 10_u128.pow(decimals);
        // Half-away-from-zero, exactly, in integers.
        let scaled = (value * scale + divisor / 2) / divisor;

        // Rounding can push the value into the next unit (999.6 GB -> 1.00 TB).
        if scaled >= 1000 * scale && unit + 1 < units.len() {
            divisor *= base;
            unit += 1;
            continue;
        }

        let integral = scaled / scale;
        let fraction = scaled % scale;
        let unit_name = units[unit];
        return if decimals == 0 {
            format!("{integral} {unit_name}")
        } else {
            format!(
                "{integral}.{fraction:0width$} {unit_name}",
                width = usize::try_from(decimals).unwrap_or(2)
            )
        };
    }
}

/// Formats a share of a whole as a percentage with one decimal, e.g. `34.2%`.
///
/// Returns `0.0%` when `whole` is zero rather than dividing by it. Integer
/// arithmetic throughout, for the same reason as [`format_si`].
#[must_use]
pub fn format_percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0%".to_owned();
    }
    let scaled = (u128::from(part) * 1000 + u128::from(whole) / 2) / u128::from(whole);
    format!("{}.{}%", scaled / 10, scaled % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_is_decimal_because_finder_is() {
        assert_eq!(format_si(1_000), "1.00 kB");
        assert_eq!(format_si(1_000_000), "1.00 MB");
        assert_eq!(format_si(1_000_000_000), "1.00 GB");
        assert_eq!(format_si(1_000_000_000_000), "1.00 TB");
        // The same count in IEC is a visibly different number. That is the
        // whole point of not mixing them.
        assert_eq!(format_iec(1_000_000_000), "954 MiB");
    }

    #[test]
    fn precision_steps_down_as_magnitude_grows() {
        assert_eq!(format_si(2_410_000_000), "2.41 GB");
        assert_eq!(format_si(18_200_000_000), "18.2 GB");
        assert_eq!(format_si(926_000_000_000), "926 GB");
    }

    #[test]
    fn small_counts_print_exactly() {
        assert_eq!(format_si(0), "0 B");
        assert_eq!(format_si(1), "1 B");
        assert_eq!(format_si(999), "999 B");
        assert_eq!(format_iec(1023), "1023 B");
    }

    #[test]
    fn rounding_carries_into_the_next_unit() {
        assert_eq!(format_si(999_600_000_000), "1.00 TB");
        assert_eq!(format_si(999_999_999_999_999_999), "1.00 EB");
    }

    #[test]
    fn largest_representable_count_does_not_overflow() {
        assert_eq!(format_si(u64::MAX), "18.4 EB");
        assert_eq!(format_iec(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn percent_handles_the_empty_tree() {
        assert_eq!(format_percent(0, 0), "0.0%");
        assert_eq!(format_percent(1, 3), "33.3%");
        assert_eq!(format_percent(3, 3), "100.0%");
    }
}
