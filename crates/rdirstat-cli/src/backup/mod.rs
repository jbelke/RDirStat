//! `rdirstat backup plan`, `apply`, `index`, and `status`.
//!
//! The contract is `.settings/docs/11-ORGANIZE.md`, Amendment A, and the
//! council call on `rds-fkk.10`:
//!
//! - The CLI applies a **Backup** and nothing else. A plan file whose intent is
//!   not `backup` is refused.
//! - The source is only read: `lstat`, `open(O_RDONLY)`, and `ditto` /
//!   `clonefile(2)` with the source as the side being read. Nothing here
//!   renames, trashes, removes, or links a source path.
//! - One carve-out: the app's own catalog directory
//!   (`~/Library/Application Support/<bundle id>/catalog`) is written even
//!   when it lies under the source root, as it does for the boot Data volume
//!   or a home folder. Plans, part files and their temp files go there, each
//!   created new; nothing that already exists is replaced. Any other catalog
//!   or plan path under the source root is refused
//!   ([`refuse_write_under_source`]).
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
//!
//! Amendment B: the drive describes itself. Apply writes the plan's rows
//! (`plans` parts) and carries the plan's scan entries to
//! `<dest>/.rdirstat/catalog`, then rebuilds `<dest>/index.parquet` and
//! `<dest>/index.sql`. `backup index` does that rebuild alone; `backup status`
//! reports from the same parts. Both read Parquet only and never walk the
//! drive.

mod apply;
mod plan;
mod profile;
mod recommend;
mod rollup;
mod service;
mod status;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use rdirstat_organize::DiskFacts;

use crate::cli::{
    BackupApplyArgs, BackupCommand, BackupControlArgs, BackupDestArgs, BackupPlanArgs, BackupServeArgs,
    BackupStatusArgs, BackupSubmitArgs, ControlAction,
};

pub(crate) use apply::ApplyReport;
pub(crate) use apply::{ApplyHooks, Gate, ItemResult, Outcome, apply_with};
pub(crate) use apply::{ApplyRequest, apply};
pub(crate) use plan::PlanOutcome;
pub(crate) use plan::{PlanRequest, build_plan};
pub(crate) use recommend::DEFAULT_LIMIT as RECOMMEND_LIMIT;

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
            volume_uuid: identity.volume_uuid,
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
        BackupCommand::Index(args) => run_index(args),
        BackupCommand::Status(args) => run_status(args),
        BackupCommand::Recommend(args) => profile::run_recommend(args),
        BackupCommand::Serve(args) => run_serve(args),
        BackupCommand::Jobs(args) => run_jobs(args),
        BackupCommand::Submit(args) => run_submit(args),
        BackupCommand::Control(args) => run_control(args),
    }
}

/// The API socket of `dest`'s service (see [`service::socket_for`]).
fn local_socket(dest: &Path) -> Result<PathBuf> {
    service::socket_for(dest, &SystemProbe, &service::LocalDirs::from_env())
}

/// How long a CLI call to the service may take.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn run_serve(args: &BackupServeArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    if args.detach {
        return detach(&dest, args.catalog.as_deref());
    }
    let running = service::start(service::ServeConfig {
        dest,
        probe: std::sync::Arc::new(SystemProbe),
        default_catalog: Some(catalog_dir(args.catalog.as_deref())?),
        app_catalog: catalog_dir(None).ok(),
        local: service::LocalDirs::from_env(),
        signals: true,
        on_item: None,
    })?;
    eprintln!(
        "rdirstat: backup service pid {} listening on {}",
        running.service.health().pid,
        running.socket.display()
    );
    running.join()?;
    Ok(ExitCode::SUCCESS)
}

/// Re-runs this binary as `backup serve` in a new session, logging to the
/// drive, and waits for it to answer `/v1/health`.
fn detach(dest: &Path, catalog: Option<&Path>) -> Result<ExitCode> {
    let run = rdirstat_jobs::run_dir(dest);
    std::fs::create_dir_all(&run).with_context(|| format!("cannot create {}", run.display()))?;
    let log_path = run.join(rdirstat_jobs::SERVE_LOG);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let exe = std::env::current_exe().context("cannot find this binary")?;
    let mut command = std::process::Command::new(exe);
    command.arg("backup").arg("serve").arg("--dest").arg(dest);
    if let Some(catalog) = catalog {
        command.arg("--catalog").arg(catalog);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    service::new_session(&mut command);
    let socket = local_socket(dest)?;
    let mut child = command.spawn().context("cannot start the backup service")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(health) = service::client::call::<rdirstat_jobs::Health>(
            &socket,
            "GET",
            "/v1/health",
            None,
            std::time::Duration::from_secs(2),
        ) && health.pid == child.id()
        {
            print(&format!(
                "Backup service pid {} on {}\nSocket: {}\nLog: {}\n",
                health.pid,
                dest.display(),
                socket.display(),
                log_path.display()
            ))?;
            return Ok(ExitCode::SUCCESS);
        }
        if let Some(status) = child.try_wait()? {
            bail!("the backup service exited ({status}); see {}", log_path.display());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "the backup service (pid {}) did not answer within 20 s; see {}",
                child.id(),
                log_path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn run_jobs(args: &BackupDestArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    let socket = local_socket(&dest)?;
    let (list, source) =
        match service::client::call::<rdirstat_jobs::JobList>(&socket, "GET", "/v1/jobs", None, CALL_TIMEOUT) {
            Ok(list) => (list, "service"),
            Err(service::client::CallError::Unreachable(_)) => (service::read_jobs(&dest)?, "drive"),
            Err(error) => return Err(error.into()),
        };
    let text = if args.json {
        let mut text = serde_json::to_string_pretty(&serde_json::json!({ "source": source, "jobs": list.jobs }))?;
        text.push('\n');
        text
    } else {
        service::jobs_text(&dest, &list, source)
    };
    print(&text)?;
    Ok(ExitCode::SUCCESS)
}

fn run_submit(args: &BackupSubmitArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    let body = std::fs::read(&args.job).with_context(|| format!("cannot read {}", args.job.display()))?;
    serde_json::from_slice::<rdirstat_jobs::NewJob>(&body)
        .with_context(|| format!("{} is not a job", args.job.display()))?;
    let summary: rdirstat_jobs::JobSummary =
        service::client::call(&local_socket(&dest)?, "POST", "/v1/jobs", Some(&body), CALL_TIMEOUT)?;
    if args.json {
        print(&format!("{}\n", serde_json::to_string_pretty(&summary)?))?;
    } else {
        print(&format!(
            "Queued job {} ({} sources)\n",
            summary.spec.id,
            summary.spec.sources.len()
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn run_control(args: &BackupControlArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    let socket = local_socket(&dest)?;
    if args.action == ControlAction::Shutdown {
        let health: rdirstat_jobs::Health = service::client::call(&socket, "POST", "/v1/shutdown", None, CALL_TIMEOUT)?;
        print(&format!(
            "Backup service pid {} is finishing the current item and stopping\n",
            health.pid
        ))?;
        return Ok(ExitCode::SUCCESS);
    }
    let Some(job) = args.job.as_deref() else {
        bail!("name the job");
    };
    let verb = match args.action {
        ControlAction::Pause => "pause",
        ControlAction::Resume => "resume",
        ControlAction::Cancel | ControlAction::Shutdown => "cancel",
    };
    let summary: rdirstat_jobs::JobSummary =
        service::client::call(&socket, "POST", &format!("/v1/jobs/{job}/{verb}"), None, CALL_TIMEOUT)?;
    print(&format!(
        "Job {}: {:?}, control {}\n",
        summary.spec.id,
        summary.state.phase,
        summary.control.word()
    ))?;
    Ok(ExitCode::SUCCESS)
}

/// The run id a plan's scan entries are written under:
/// `plan-<label>-<created_at_ms>`.
pub(crate) fn plan_id(plan: &rdirstat_organize::PlanFile) -> String {
    format!("plan-{}-{}", plan.source_label, plan.created_at_ms)
}

/// Spotlight caveats for the drive: the marker file, then `mdutil -s`.
fn spotlight_warnings(dest: &Path) -> Vec<String> {
    status::spotlight_marker_warning(dest)
        .into_iter()
        .chain(status::mdutil_warning(dest))
        .collect()
}

fn run_index(args: &BackupDestArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    let summary = rdirstat_catalog::rebuild_index(&dest)
        .with_context(|| format!("rebuilding the index of {}", dest.display()))?;
    let warnings = spotlight_warnings(&dest);
    let text = if args.json {
        let doc = serde_json::json!({
            "index": summary.index.to_string_lossy(),
            "sql": summary.sql.to_string_lossy(),
            "files": summary.rows,
            "logical": summary.bytes,
            "warnings": warnings,
        });
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        text
    } else {
        let mut text = format!(
            "Index: {} files, {} -> {}\nViews: duckdb -init {}\n",
            summary.rows,
            rdirstat_core::format_iec(summary.bytes),
            summary.index.display(),
            summary.sql.display()
        );
        for warning in &warnings {
            text.push_str("Warning: ");
            text.push_str(warning);
            text.push('\n');
        }
        text
    };
    print(&text)?;
    Ok(ExitCode::SUCCESS)
}

fn run_status(args: &BackupStatusArgs) -> Result<ExitCode> {
    let dest = std::fs::canonicalize(&args.dest).with_context(|| format!("cannot resolve {}", args.dest.display()))?;
    let mut report = status::status(&dest, args.source_label.as_deref())?;
    report.warnings.extend(status::mdutil_warning(&dest));
    let text = if args.json {
        let mut text = serde_json::to_string_pretty(&status::json(&report))?;
        text.push('\n');
        text
    } else {
        status::text(&report)
    };
    print(&text)?;
    Ok(ExitCode::SUCCESS)
}

fn run_plan(args: &BackupPlanArgs) -> Result<ExitCode> {
    if let Some(path) = &args.profile {
        return profile::run_plan(path, args);
    }
    let (Some(label), Some(dest)) = (&args.label, &args.dest) else {
        bail!("--label and --dest are required without --profile");
    };
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
        label: label.clone(),
        dest: dest.clone(),
        floor: args.floor,
        snapshot: args.snapshot.clone(),
        rules,
        out: args.out.clone(),
        catalog,
        app_catalog: catalog_dir(None).ok(),
        progress: crate::wants_progress(args.progress),
        rollup: !args.no_rollup,
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
        app_catalog: catalog_dir(None).ok(),
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

/// Refuses `path` (`what` names it: "the catalog", "the plan file") when it
/// resolves to a place under `source`, unless it is inside `app_catalog`, the
/// app's own data directory (the documented carve-out in this module's docs).
///
/// Paths are resolved through their deepest existing ancestor, so a catalog
/// that does not exist yet, or one reached through a symlink, is judged by
/// where it would really be written.
///
/// # Errors
///
/// The refusal, naming both paths.
pub(crate) fn refuse_write_under_source(
    source: &Path,
    what: &str,
    path: &Path,
    app_catalog: Option<&Path>,
) -> Result<()> {
    let resolved = resolve(path);
    if !resolved.starts_with(resolve(source)) {
        return Ok(());
    }
    if app_catalog.is_some_and(|app| resolved.starts_with(resolve(app))) {
        return Ok(());
    }
    bail!(
        "{what} {} is inside the source {}; the source is only read. Pass a {} outside it",
        path.display(),
        source.display(),
        if what.contains("plan") { "--out" } else { "--catalog" }
    )
}

/// `path` made absolute, with its deepest existing ancestor canonicalized and
/// the rest appended (`..` popped, `.` dropped).
fn resolve(path: &Path) -> PathBuf {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    let mut base = loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            break canonical;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                existing = parent;
            }
            _ => break existing.to_path_buf(),
        }
    };
    for name in tail.iter().rev() {
        match Path::new(name).components().next() {
            Some(Component::ParentDir) => {
                base.pop();
            }
            Some(Component::CurDir) | None => {}
            Some(_) => base.push(name),
        }
    }
    base
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
