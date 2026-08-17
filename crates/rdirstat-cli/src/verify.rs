//! `rdirstat verify` — scan a tree and diff the allocated total against `du`.
//!
//! This one command catches, in a single run, every accounting bug that is
//! otherwise invisible until someone reports a wrong number:
//!
//! - **symlink following** — `du` does not follow symlinks either, so a scanner
//!   that does reports a larger total, usually much larger;
//! - **hard-link double counting** — `du` counts each `(dev, ino)` once per
//!   invocation, so a scanner without the policy over-reports by the size of
//!   every extra link;
//! - **dot-entry accounting** — a reader that synthesizes `.` and `..` inflates
//!   both the entry count and, if it descends them, the byte total without
//!   bound;
//! - **device-boundary crossing** — `du -x` and `--cross-filesystems=false`
//!   must see the same namespace, and if only one of them stops at a mount
//!   point the totals separate by an entire volume.
//!
//! The comparison is only valid when both tools use the same policy, so the
//! invocation is built from the scan options rather than hard-coded: `-x` is
//! dropped exactly when `--cross-filesystems` is passed, and verification runs
//! with exclusions **off** because `du` has no equivalent rule set.
//!
//! On APFS, directories report `st_blocks == 0`, so the expected disagreement
//! is zero bytes and the default tolerance is zero. `du -sk` rounds up to whole
//! kibibytes, which is why agreement is judged in kibibytes as well as bytes.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use rdirstat_core::format_si;
use serde_json::{Value, json};

use crate::cli::VerifyArgs;
use crate::engine::{self, Outcome};
use crate::options;

/// What `du` reported.
#[derive(Clone, Debug)]
pub(crate) struct DuResult {
    /// Kibibytes, as printed by `du -sk`.
    pub(crate) kib: u64,
    /// The exact argv, so a recorded result can be reproduced.
    pub(crate) command: String,
    /// Lines `du` wrote to stderr — usually permission errors, which mean the
    /// two tools are both looking at a floor.
    pub(crate) stderr_lines: usize,
}

/// The comparison.
#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    /// Allocated bytes the scanner counted.
    pub(crate) scan_bytes: u64,
    /// Those bytes rounded up to kibibytes, the unit `du -sk` prints.
    pub(crate) scan_kib: u64,
    /// What `du` said.
    pub(crate) du: DuResult,
    /// `scan_bytes` minus `du.kib * 1024`.
    pub(crate) delta_bytes: i128,
    /// Whether the two agree within the tolerance.
    pub(crate) agrees: bool,
}

/// Runs `du -sk` (plus `-x` unless the scan crossed filesystems).
///
/// # Errors
///
/// If `du` cannot be spawned, exits non-zero with no parsable output, or prints
/// something this code refuses to guess at.
pub(crate) fn run_du(path: &Path, cross_filesystems: bool) -> Result<DuResult> {
    let mut command = Command::new("/usr/bin/du");
    if cross_filesystems {
        command.arg("-sk");
    } else {
        command.args(["-skx"]);
    }
    command.arg(path);
    let rendered = format!(
        "/usr/bin/du {} {}",
        if cross_filesystems { "-sk" } else { "-skx" },
        path.display()
    );
    let output = command
        .output()
        .with_context(|| format!("could not run `{rendered}`"))?;

    // `du` exits non-zero when any path was unreadable but still prints the
    // total, so the exit status alone is not a reason to give up.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr_lines = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let first = stdout
        .lines()
        .next_back()
        .with_context(|| format!("`{rendered}` printed nothing"))?;
    let field = first
        .split_whitespace()
        .next()
        .with_context(|| format!("`{rendered}` printed an unparsable line: {first}"))?;
    let kib: u64 = field
        .parse()
        .with_context(|| format!("`{rendered}` printed a non-numeric size: {field}"))?;

    Ok(DuResult {
        kib,
        command: rendered,
        stderr_lines,
    })
}

/// Scans `path` and compares the allocated total with `du`.
///
/// # Errors
///
/// If the scan fails fatally or `du` cannot be read.
pub(crate) fn run(args: &VerifyArgs, progress: bool) -> Result<(Outcome, Verdict)> {
    if !args.path.is_dir() {
        bail!("{} is not a directory", args.path.display());
    }
    let outcome = engine::scan(&engine::Config {
        root: args.path.clone(),
        options: options::for_verify(args),
        progress,
    })
    .with_context(|| format!("scanning {}", args.path.display()))?;

    let du = run_du(&args.path, args.cross_filesystems)?;
    let verdict = compare(outcome.scan.totals.allocated, du, args.tolerance_bytes);
    Ok((outcome, verdict))
}

/// The comparison itself, separated so it can be tested without a filesystem.
pub(crate) fn compare(scan_bytes: u64, du: DuResult, tolerance_bytes: u64) -> Verdict {
    let scan_kib = scan_bytes.div_ceil(1024);
    let du_bytes = i128::from(du.kib).saturating_mul(1024);
    let delta_bytes = i128::from(scan_bytes) - du_bytes;
    // Two ways to agree, both exact: the same kibibyte figure `du` printed, or
    // a byte delta inside the caller's tolerance. The first is the normal one —
    // `du -sk` rounds up, and so does `div_ceil`.
    let agrees = scan_kib == du.kib || delta_bytes.unsigned_abs() <= u128::from(tolerance_bytes);
    Verdict {
        scan_bytes,
        scan_kib,
        du,
        delta_bytes,
        agrees,
    }
}

/// Human-readable verdict.
pub(crate) fn text(outcome: &Outcome, verdict: &Verdict) -> String {
    let scan = &outcome.scan;
    let mut out = String::new();
    let _ignored = writeln!(out, "root        {}", scan.root_path.display());
    let _ignored = writeln!(out, "command     {}", verdict.du.command);
    let _ignored = writeln!(
        out,
        "scanner     {:>16} allocated  ({} KiB)",
        format_si(verdict.scan_bytes),
        verdict.scan_kib
    );
    let _ignored = writeln!(
        out,
        "du          {:>16} allocated  ({} KiB)",
        format_si(verdict.du.kib.saturating_mul(1024)),
        verdict.du.kib
    );
    let _ignored = writeln!(out, "delta       {:>16} bytes", verdict.delta_bytes);
    let _ignored = writeln!(
        out,
        "entries     {} observed, {} retained, {} hard-link repeats, {} symlinks",
        scan.counts.observed_entries, scan.counts.retained_nodes, scan.counts.hard_link_repeats, scan.counts.symlinks
    );
    if scan.counts.unreadable_dirs > 0 || verdict.du.stderr_lines > 0 {
        let _ignored = writeln!(
            out,
            "warning     {} unreadable directories here, {} stderr lines from du — both totals are a floor",
            scan.counts.unreadable_dirs, verdict.du.stderr_lines
        );
    }
    if scan.mutations > 0 {
        let _ignored = writeln!(
            out,
            "warning     {} entries changed during the scan; the tree was not quiescent",
            scan.mutations
        );
    }
    out.push_str(if verdict.agrees {
        "verdict     AGREE\n"
    } else {
        "verdict     DISAGREE\n"
    });
    out
}

/// Machine-readable verdict.
pub(crate) fn json(outcome: &Outcome, verdict: &Verdict) -> Value {
    let scan = &outcome.scan;
    json!({
        "root": scan.root_path.display().to_string(),
        "command": verdict.du.command,
        "scan_allocated_bytes": verdict.scan_bytes,
        "scan_allocated_kib": verdict.scan_kib,
        "du_kib": verdict.du.kib,
        "delta_bytes": verdict.delta_bytes.to_string(),
        "agrees": verdict.agrees,
        "du_stderr_lines": verdict.du.stderr_lines,
        "counts": scan.counts,
        "mutations": scan.mutations,
        "wall_ms": outcome.measurement.wall_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn du(kib: u64) -> DuResult {
        DuResult {
            kib,
            command: "test".to_owned(),
            stderr_lines: 0,
        }
    }

    #[test]
    fn exact_kibibyte_agreement_is_agreement() {
        let verdict = compare(104 * 1024, du(104), 0);
        assert!(verdict.agrees);
        assert_eq!(verdict.delta_bytes, 0);
    }

    #[test]
    fn a_partial_kibibyte_rounds_up_the_way_du_does() {
        // 512 bytes is one 512-block: `du -sk` prints 1.
        let verdict = compare(512, du(1), 0);
        assert!(verdict.agrees);
        assert_eq!(verdict.scan_kib, 1);
        assert_eq!(verdict.delta_bytes, -512);
    }

    #[test]
    fn a_doubled_hard_link_shows_up_as_a_disagreement() {
        let verdict = compare(208 * 1024, du(104), 0);
        assert!(!verdict.agrees);
        assert_eq!(verdict.delta_bytes, 104 * 1024);
    }

    #[test]
    fn a_tolerance_can_absorb_a_known_difference() {
        let verdict = compare(104 * 1024 + 4096, du(104), 8192);
        assert!(verdict.agrees);
    }
}
