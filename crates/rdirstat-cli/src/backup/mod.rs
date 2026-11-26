//! `rdirstat backup plan` and `rdirstat backup apply`.
//!
//! The contract is `.settings/docs/11-ORGANIZE.md`, Amendment A, and the
//! council call on `rds-fkk.10`:
//!
//! - The CLI applies a **Backup** and nothing else. A plan file whose intent is
//!   not `backup` is refused.
//! - The source is only read: `lstat`, `open(O_RDONLY)`, and `ditto` /
//!   `clonefile(2)` with the source as the side being read. Nothing here
//!   renames, trashes, removes, or links a source path.
//! - The token is SHA-256 over the plan rows, the source snapshot identity, the
//!   rules file hash, and the destination `disk_id`
//!   ([`rdirstat_organize::plan_token`]). Apply refuses the whole plan unless
//!   the token the user passes equals the token of the file it read.
//! - Apply re-stats every item `(dev, ino, size, mtime)` before copying it. A
//!   difference is `changed_since_scan` for that item; it is skipped, never
//!   copied in its new state.
//! - A source and destination on the same physical disk, or with an unknown
//!   `disk_id` on either side, are refused at plan time and again at apply.
//! - A receipt is written only for an item whose destination compared equal to
//!   its source. A failed copy or verify leaves the partial on disk and names
//!   it; nothing is deleted.

mod apply;
mod plan;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use rdirstat_organize::DiskFacts;

use crate::cli::{BackupApplyArgs, BackupCommand, BackupPlanArgs};

#[cfg(test)]
pub(crate) use apply::ApplyReport;
pub(crate) use apply::{ApplyRequest, apply};
#[cfg(test)]
pub(crate) use plan::PlanOutcome;
pub(crate) use plan::{PlanRequest, build_plan};

/// Exit code for an apply that ran to the end but left items not landed.
pub(crate) const EXIT_INCOMPLETE: u8 = 3;

/// The app's bundle identifier; its data dir holds the primary catalog.
const APP_IDENTIFIER: &str = "jbelke.rdirstat";

/// Where facts about the disk under a path come from.
///
/// The real probe asks the OS (`statfs`, `diskutil`). Tests substitute fixed
/// facts, because two temp dirs always share one disk and the real pair check
/// would (correctly) refuse them.
pub(crate) trait DiskProbe {
    /// Facts about the volume and physical disk holding `path`.
    fn facts(&self, path: &Path) -> Result<DiskFacts>;
}

/// [`DiskProbe`] backed by [`rdirstat_copy::disk_identity`].
pub(crate) struct SystemProbe;

impl DiskProbe for SystemProbe {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let identity = rdirstat_copy::disk_identity(path)
            .with_context(|| format!("cannot identify the disk of {}", path.display()))?;
        Ok(DiskFacts {
            disk_id: identity.disk_id,
            st_dev: identity.st_dev,
            filesystem: identity.filesystem,
            total_bytes: identity.total_bytes,
            available_bytes: identity.available_bytes,
        })
    }
}

/// Dispatches `rdirstat backup <subcommand>`.
pub(crate) fn run(command: &BackupCommand) -> Result<ExitCode> {
    match command {
        BackupCommand::Plan(args) => run_plan(args),
        BackupCommand::Apply(args) => run_apply(args),
    }
}

fn run_plan(args: &BackupPlanArgs) -> Result<ExitCode> {
    let catalog = catalog_dir(args.catalog.as_deref())?;
    let rules = match &args.rules {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
            rdirstat_organize::Rules::from_toml(&text).with_context(|| format!("rules file {}", path.display()))?
        }
        None => rdirstat_organize::Rules::default_backup(),
    };
    let request = PlanRequest {
        source: args.source.clone(),
        label: args.label.clone(),
        dest: args.dest.clone(),
        floor: args.floor,
        snapshot: args.snapshot.clone(),
        rules,
        out: args.out.clone(),
        catalog,
        progress: crate::wants_progress(args.progress),
    };
    let outcome = build_plan(&request, &SystemProbe)?;
    let text = if args.json {
        let mut text = serde_json::to_string_pretty(&plan::json(&outcome, args.top))?;
        text.push('\n');
        text
    } else {
        plan::text(&outcome, args.top)
    };
    print(&text)?;
    Ok(ExitCode::SUCCESS)
}

fn run_apply(args: &BackupApplyArgs) -> Result<ExitCode> {
    let request = ApplyRequest {
        plan: args.plan.clone(),
        token: args.token.clone(),
        catalog: catalog_dir(args.catalog.as_deref())?,
        progress: std::io::IsTerminal::is_terminal(&std::io::stderr()),
    };
    let report = apply(&request, &SystemProbe)?;
    let text = if args.json {
        let mut text = serde_json::to_string_pretty(&apply::json(&report))?;
        text.push('\n');
        text
    } else {
        apply::text(&report)
    };
    print(&text)?;
    if report.complete() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(EXIT_INCOMPLETE))
    }
}

fn print(text: &str) -> Result<()> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// `--catalog`, or `~/Library/Application Support/<bundle id>/catalog`, the
/// directory the desktop app's data dir resolves to.
fn catalog_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        bail!("HOME is not set; pass --catalog <dir>");
    };
    Ok(PathBuf::from(home)
        .join("Library/Application Support")
        .join(APP_IDENTIFIER)
        .join("catalog"))
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

/// Parses a byte size: a plain integer, or a number with a unit. Binary units
/// (`KiB`, `MiB`, `GiB`, `TiB`) are powers of 1024; decimal units (`KB`, `MB`,
/// `GB`, `TB`) are powers of 1000. Case-insensitive; `K`/`M`/`G`/`T` alone are
/// binary. Fractions are allowed (`1.5GiB`).
///
/// # Errors
///
/// A message for clap when the text is not a size or overflows `u64`.
pub(crate) fn parse_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "unknown size unit {other:?}; use B, KiB, MiB, GiB, TiB, KB, MB, GB, or TB"
            ));
        }
    };
    if number.is_empty() {
        return Err(format!("{text:?} is not a size"));
    }
    if let Ok(whole) = number.parse::<u64>() {
        return whole
            .checked_mul(multiplier)
            .ok_or_else(|| format!("{text:?} is too large"));
    }
    let Some((int_part, frac_part)) = number.split_once('.') else {
        return Err(format!("{text:?} is not a size"));
    };
    let int: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| format!("{text:?} is not a size"))?
    };
    if frac_part.is_empty() || frac_part.len() > 9 || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{text:?} is not a size"));
    }
    let frac: u64 = frac_part.parse().map_err(|_| format!("{text:?} is not a size"))?;
    let scale = 10_u64.pow(u32::try_from(frac_part.len()).unwrap_or(9));
    let whole = int
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{text:?} is too large"))?;
    let fraction = u128::from(frac) * u128::from(multiplier) / u128::from(scale);
    whole
        .checked_add(u64::try_from(fraction).map_err(|_| format!("{text:?} is too large"))?)
        .ok_or_else(|| format!("{text:?} is too large"))
}
