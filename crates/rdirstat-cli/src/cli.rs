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
