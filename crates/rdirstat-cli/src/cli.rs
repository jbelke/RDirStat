//! Command-line surface.
//!
//! Flag names deliberately track `pdu` (parallel-disk-usage) where the two
//! tools answer the same question — `--quantity`, `--top-down`, `--exclude`,
//! `--threads` — so a measurement taken with one is directly comparable with a
//! measurement taken with the other. A flag that means something different from
//! `pdu`'s flag of the same name is a trap, not a convenience.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Scanner diagnostics, profiling, and correctness harness.
#[derive(Debug, Parser)]
#[command(
    name = "rdirstat",
    version,
    about = "RDirStat scanner harness: measure and verify a scan without the webview",
    long_about = None,
    propagate_version = true
)]
pub(crate) struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The supported subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Scan a directory tree and report totals, structure, and measurements.
    Scan(Box<ScanArgs>),
    /// Scan a directory tree and diff the allocated total against `du -sk`.
    Verify(Box<VerifyArgs>),
    /// Plan and apply a Backup onto another physical disk. Backup only: the
    /// source is opened read-only and is never renamed, trashed, or linked.
    #[command(subcommand)]
    Backup(BackupCommand),
}

/// `rdirstat backup …`.
#[derive(Debug, Subcommand)]
pub(crate) enum BackupCommand {
    /// Choose what one Backup wave copies, write the plan file, print its token.
    /// Writes nothing on the destination.
    Plan(Box<BackupPlanArgs>),
    /// Copy a plan's items to the destination and record a receipt for each
    /// item that verified. Refuses unless `--token` matches the plan file.
    Apply(Box<BackupApplyArgs>),
    /// Rebuild `<dest>/index.parquet` and `<dest>/index.sql` from the drive's
    /// own catalog parts. Reads Parquet only; never walks the drive.
    Index(BackupDestArgs),
    /// Print what the drive holds per source: bytes and files backed up,
    /// clone savings, pending items of the last plan, and files over the
    /// floor not yet backed up. Reads Parquet only; never walks the drive.
    Status(BackupStatusArgs),
    /// Dry-run every source of a profile and print what a run would copy,
    /// defer, and skip, largest first, with the skip rule and reason. Writes
    /// nothing and applies nothing.
    Recommend(BackupRecommendArgs),
    /// Resolve and check a profile against the filesystem without planning:
    /// the destination and roots exist, every source is on another disk
    /// than the destination, includes exist under their roots, the rules
    /// file parses. Exits 1 with the first problem.
    CheckProfile(BackupProfileArgs),
    /// Run the backup service for one destination drive: hold its lock, run
    /// jobs (plan + apply rounds until everything lands, waiting for a
    /// dropped drive), and answer the /v1 API on a Unix socket in a per-user
    /// directory (`~/Library/Application Support/rdirstat/run/<volume
    /// uuid>.sock`, else under `$TMPDIR`), never on the drive. Never deletes
    /// anything on the drive.
    Serve(BackupServeArgs),
    /// List the drive's jobs and flag unfinished ones. Asks the service when
    /// one runs, else reads `<dest>/.rdirstat/jobs` directly.
    Jobs(BackupDestArgs),
    /// Hand the service a job (a `NewJob` JSON file) to queue.
    Submit(BackupSubmitArgs),
    /// Pause, resume or cancel a job, or shut the service down. Takes effect
    /// between items: the item being copied always finishes.
    Control(BackupControlArgs),
    /// Run one backup job in-process and print wall time per phase as JSON
    /// (`scripts/bench-backup.sh`). `phase-timing` builds only.
    #[cfg(feature = "phase-timing")]
    #[command(hide = true)]
    Bench(BackupBenchArgs),
    /// Print this binary's version and the job schema it speaks as one JSON
    /// line. The desktop app asks this before it launches the binary.
    Version,
}

/// `rdirstat backup serve` options.
#[derive(Debug, Args)]
pub(crate) struct BackupServeArgs {
    /// Destination root (the backup volume's mount point).
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// Primary catalog for jobs that name none. Default: the app data dir's
    /// `catalog`.
    #[arg(long, value_name = "DIR")]
    pub(crate) catalog: Option<PathBuf>,

    /// Start the service in a new session (`setsid`) with stdout and stderr
    /// appended to `<dest>/.rdirstat/run/serve.log`, wait until it answers,
    /// print its pid, and return.
    #[arg(long)]
    pub(crate) detach: bool,
}

/// `rdirstat backup submit` options.
#[derive(Debug, Args)]
pub(crate) struct BackupSubmitArgs {
    /// Destination root whose service gets the job.
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// A JSON file holding the job (`sources`, optional `id`, `rules`,
    /// `floor`, `max_rounds`, ...).
    #[arg(long, value_name = "FILE")]
    pub(crate) job: PathBuf,

    /// Print the service's JSON answer.
    #[arg(long)]
    pub(crate) json: bool,
}

/// What `backup control` asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ControlAction {
    /// Hold the job between items.
    Pause,
    /// Continue a paused job, or re-queue a stopped, cancelled, incomplete
    /// or failed one.
    Resume,
    /// Stop the job between items.
    Cancel,
    /// Finish the current item, record the job as stopped, and exit.
    Shutdown,
}

/// `rdirstat backup control` options.
#[derive(Debug, Args)]
pub(crate) struct BackupControlArgs {
    /// Destination root whose service to ask.
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// What to do.
    #[arg(value_enum)]
    pub(crate) action: ControlAction,

    /// The job (not for `shutdown`).
    #[arg(value_name = "JOB_ID", required_if_eq_any = [("action", "pause"), ("action", "resume"), ("action", "cancel")])]
    pub(crate) job: Option<String>,
}

/// `rdirstat backup check-profile` options.
#[derive(Debug, Args)]
pub(crate) struct BackupProfileArgs {
    /// The profile file (`<dest>/.rdirstat/profiles/<name>.toml`).
    #[arg(long, value_name = "FILE")]
    pub(crate) profile: PathBuf,

    /// Print one JSON document (`rdirstat_organize::ProfileCheck`) instead of text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// `rdirstat backup recommend` options.
#[derive(Debug, Args)]
pub(crate) struct BackupRecommendArgs {
    /// The profile file (`<dest>/.rdirstat/profiles/<name>.toml`).
    #[arg(long, value_name = "FILE")]
    pub(crate) profile: PathBuf,

    /// Most rows to list; totals always cover every row.
    #[arg(long, value_name = "N", default_value_t = crate::backup::RECOMMEND_LIMIT)]
    pub(crate) limit: usize,

    /// Print one JSON document (`rdirstat_organize::Recommendation`) instead of text.
    #[arg(long)]
    pub(crate) json: bool,

    /// When to draw the scan progress line on stderr.
    #[arg(long, value_enum, default_value_t = ProgressWhen::Auto)]
    pub(crate) progress: ProgressWhen,
}

/// `rdirstat backup bench` options.
#[cfg(feature = "phase-timing")]
#[derive(Debug, Args)]
pub(crate) struct BackupBenchArgs {
    /// Source root (the corpus).
    #[arg(long, value_name = "DIR")]
    pub(crate) source: PathBuf,

    /// Destination root. The job, catalog mirror and copies go under it.
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// Primary catalog directory; not under the source.
    #[arg(long, value_name = "DIR")]
    pub(crate) catalog: PathBuf,

    /// Source label.
    #[arg(long, default_value = "bench")]
    pub(crate) label: String,

    /// Smallest allocated size planned as its own item; smaller files land
    /// in directory roll-ups. Default: a real job's.
    #[arg(long, value_name = "BYTES")]
    pub(crate) floor: Option<u64>,

    /// Write the JSON report here as well as to stdout.
    #[arg(long, value_name = "FILE")]
    pub(crate) out: Option<PathBuf>,
}

/// `rdirstat backup index` options.
#[derive(Debug, Args)]
pub(crate) struct BackupDestArgs {
    /// Destination root (the backup volume's mount point).
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// Print one JSON document instead of text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// `rdirstat backup status` options.
#[derive(Debug, Args)]
pub(crate) struct BackupStatusArgs {
    /// Destination root (the backup volume's mount point).
    #[arg(long, value_name = "DIR")]
    pub(crate) dest: PathBuf,

    /// Only this source label.
    #[arg(long, value_name = "LABEL")]
    pub(crate) source_label: Option<String>,

    /// Print one JSON document instead of text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// `rdirstat backup plan` options.
#[derive(Debug, Args)]
pub(crate) struct BackupPlanArgs {
    /// Plan every source of this profile (`<dest>/.rdirstat/profiles/<name>.toml`),
    /// one plan file each. The profile supplies sources, destination, rules,
    /// floor, and review cap.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["source", "label", "dest", "snapshot", "rules", "out", "floor"])]
    pub(crate) profile: Option<PathBuf>,

    /// Source volume root. Plan paths, and the mirror layout, are relative to
    /// it. With `--snapshot` it defaults to the snapshot's scan root.
    #[arg(long, value_name = "DIR", required_unless_present_any = ["snapshot", "profile"])]
    pub(crate) source: Option<PathBuf>,

    /// Name of this source under `<dest>/Backups/`, e.g. `mac-data`.
    #[arg(long, value_name = "LABEL", required_unless_present = "profile")]
    pub(crate) label: Option<String>,

    /// Destination root (the backup volume's mount point).
    #[arg(long, value_name = "DIR", required_unless_present = "profile")]
    pub(crate) dest: Option<PathBuf>,

    /// Smallest allocated size a candidate may have: `100MiB`, `1GiB`, `5000`.
    #[arg(long, value_name = "SIZE", default_value = "100MiB", value_parser = crate::backup::parse_size)]
    pub(crate) floor: u64,

    /// Plan from this `.rdstat` snapshot instead of walking the source.
    #[arg(long, value_name = "FILE")]
    pub(crate) snapshot: Option<PathBuf>,

    /// Skip/include rules file. Default: the built-in Backup rules.
    #[arg(long, value_name = "FILE")]
    pub(crate) rules: Option<PathBuf>,

    /// Where to write the plan file. Default: `<catalog>/plans/<label>-<ms>.json`.
    #[arg(long, value_name = "FILE")]
    pub(crate) out: Option<PathBuf>,

    /// Primary catalog directory. Default: the app data dir's `catalog`.
    #[arg(long, value_name = "DIR")]
    pub(crate) catalog: Option<PathBuf>,

    /// Largest planned items to print.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub(crate) top: usize,

    /// Print one JSON document instead of text.
    #[arg(long)]
    pub(crate) json: bool,

    /// When to draw the scan progress line on stderr (walks only).
    #[arg(long, value_enum, default_value_t = ProgressWhen::Auto)]
    pub(crate) progress: ProgressWhen,

    /// Plan only files and packages at or over the floor, one item each, as
    /// before directory roll-ups existed. By default everything else that no
    /// rule skips is covered by directory roll-up items.
    #[arg(long)]
    pub(crate) no_rollup: bool,
}

/// `rdirstat backup apply` options.
#[derive(Debug, Args)]
pub(crate) struct BackupApplyArgs {
    /// The plan file `backup plan` wrote.
    #[arg(long, value_name = "FILE")]
    pub(crate) plan: PathBuf,

    /// The token `backup plan` printed for this plan file.
    #[arg(long, value_name = "HEX")]
    pub(crate) token: String,

    /// Primary catalog directory. Default: the app data dir's `catalog`.
    #[arg(long, value_name = "DIR")]
    pub(crate) catalog: Option<PathBuf>,

    /// Print one JSON document instead of text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Which byte quantity the output is about.
///
/// The two are never summed and never reconciled: `logical` is what the file
/// claims, `allocated` is what the filesystem spent. On APFS they disagree for
/// sparse files, compressed files, and clones, and that disagreement is signal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Quantity {
    /// `st_size` — the logical length of the file.
    Logical,
    /// `st_blocks * 512` — the bytes the filesystem allocated.
    #[default]
    Allocated,
}

impl Quantity {
    /// The stable key used in JSON output.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Logical => "logical",
            Self::Allocated => "allocated",
        }
    }
}

/// Output encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// Human-readable text.
    #[default]
    Text,
    /// One JSON document on stdout.
    Json,
}

/// When to draw the progress line on stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProgressWhen {
    /// Only when stderr is a terminal.
    #[default]
    Auto,
    /// Always.
    Always,
    /// Never.
    Never,
}

/// `rdirstat scan` options.
#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a CLI argument struct is a flat description of flags, not a state machine"
)]
pub(crate) struct ScanArgs {
    /// Directory to scan.
    pub(crate) path: PathBuf,

    /// Which byte quantity to report and sort by.
    #[arg(long, value_enum, default_value_t = Quantity::Allocated)]
    pub(crate) quantity: Quantity,

    /// Output encoding.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub(crate) format: Format,

    /// Emit the measurement block: counts, arena bytes, name-blob bytes, peak
    /// RSS, wall time, and error counts by class.
    #[arg(long)]
    pub(crate) stats: bool,

    /// Print the tree root-first instead of `pdu`'s default root-last order.
    #[arg(long)]
    pub(crate) top_down: bool,

    /// Exclude entries matching a glob. Repeatable; first match wins, and
    /// these are evaluated before the shipped defaults.
    ///
    /// A pattern containing `/` is matched against the root-relative path,
    /// otherwise against the entry name. `*` and `?` do not cross `/`; `**`
    /// does.
    #[arg(long, value_name = "GLOB")]
    pub(crate) exclude: Vec<String>,

    /// Do not apply the shipped macOS exclusion defaults.
    #[arg(long)]
    pub(crate) no_default_exclusions: bool,

    /// Descend into directories that live on another filesystem.
    #[arg(long)]
    pub(crate) cross_filesystems: bool,

    /// Reader worker threads. Default is a measured-conservative 8, capped by
    /// the available parallelism.
    #[arg(long, value_name = "N")]
    pub(crate) threads: Option<u16>,

    /// Omit file nodes whose logical size is below BYTES, folding their counts
    /// and bytes into the parent directory. Marks the result aggregated.
    #[arg(long, value_name = "BYTES")]
    pub(crate) aggregate_below: Option<u64>,

    /// Count every hard link, instead of counting each `(dev, ino)` once.
    #[arg(long)]
    pub(crate) count_hard_links_every_time: bool,

    /// End the scan with a `MemoryLimit` error if projected scanner-owned
    /// resident bytes cross this ceiling.
    #[arg(long, value_name = "BYTES")]
    pub(crate) memory_limit: Option<u64>,

    /// Levels of tree to print. 0 prints only the root row.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub(crate) max_depth: u32,

    /// Largest children printed per directory.
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub(crate) max_children: u32,

    /// When to draw the progress line on stderr.
    #[arg(long, value_enum, default_value_t = ProgressWhen::Auto)]
    pub(crate) progress: ProgressWhen,

    /// Write a Chrome trace of the scan to FILE (open it in `chrome://tracing`
    /// or Perfetto). For a phased parallel scanner, the worker timeline is how
    /// you find serialization you did not know you had.
    #[arg(long, value_name = "FILE")]
    pub(crate) trace_chrome: Option<PathBuf>,
}

/// `rdirstat verify` options.
///
/// Verification deliberately runs with the exclusion defaults **off**: `du`
/// has no matching rule set, so any exclusion turns a correctness check into a
/// guaranteed disagreement.
#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    /// Directory to scan and compare.
    pub(crate) path: PathBuf,

    /// Output encoding.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub(crate) format: Format,

    /// Reader worker threads.
    #[arg(long, value_name = "N")]
    pub(crate) threads: Option<u16>,

    /// Descend into other filesystems, and drop `-x` from the `du` invocation
    /// so both tools still see the same namespace.
    #[arg(long)]
    pub(crate) cross_filesystems: bool,

    /// Accept a disagreement up to BYTES. The default is zero: on APFS,
    /// directories report `st_blocks == 0`, so an exact match is the
    /// expectation and any drift is a bug worth reading.
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub(crate) tolerance_bytes: u64,

    /// When to draw the progress line on stderr.
    #[arg(long, value_enum, default_value_t = ProgressWhen::Auto)]
    pub(crate) progress: ProgressWhen,
}
