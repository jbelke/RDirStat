//! `rdirstat` — the scanner's measurement surface.
//!
//! Everything the desktop app does to a filesystem, this binary can do without
//! a webview: scan a tree, report what it cost, and prove the totals against an
//! independent tool. That matters because a number nobody can reproduce from a
//! terminal is not a measurement, and `docs/00-OVERVIEW.md` gates the whole
//! project on numbers — 5.0 GiB peak RSS, 48 bytes a node, twelve minutes for
//! 69M entries.
//!
//! ```text
//! rdirstat scan <path> --stats --format json
//! rdirstat scan <path> --quantity logical --top-down
//! rdirstat verify <path>
//! ```
//!
//! `verify` is the one to run first. It scans, shells out to `du -skx`, and
//! exits non-zero when the two disagree, which catches symlink following,
//! hard-link double counting, dot-entry accounting, and device-boundary
//! crossing in a single command.
//!
//! Exit codes: `0` success, `1` failure, `2` a `verify` disagreement.

#![forbid(unsafe_code)]

mod cli;
mod engine;
mod exclude;
mod options;
mod report;
mod rss;
mod verify;
mod volume;

use std::io::{self, IsTerminal as _, Write as _};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::cli::{Cli, Command, Format, ProgressWhen, ScanArgs, VerifyArgs};

/// Exit code for a `verify` disagreement, distinct from a failure to run.
const EXIT_DISAGREEMENT: u8 = 2;

fn main() -> ExitCode {
    let arguments = Cli::parse();
    match run(&arguments) {
        Ok(code) => code,
        Err(error) => {
            let _ignored = writeln!(io::stderr(), "rdirstat: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &Cli) -> Result<ExitCode> {
    match &arguments.command {
        Command::Scan(args) => run_scan(args),
        Command::Verify(args) => run_verify(args),
    }
}

fn run_scan(args: &ScanArgs) -> Result<ExitCode> {
    let _guard = init_tracing(args.trace_chrome.as_deref());
    let root = canonical_root(&args.path)?;
    let progress = wants_progress(args.progress);

    let sampler = rss::Sampler::start();
    let span = tracing::info_span!("scan", root = %root.display());
    let entered = span.enter();
    let mut outcome = engine::scan(&engine::Config {
        root: root.clone(),
        options: options::for_scan(args, root == Path::new("/")),
        progress,
    })
    .with_context(|| format!("scanning {}", root.display()))?;
    drop(entered);
    outcome.measurement.peak_rss_bytes = sampler.finish();

    let stdout = io::stdout();
    let mut out = stdout.lock();
    match args.format {
        Format::Text => write!(out, "{}", report::text(&outcome, args))?,
        Format::Json => {
            let document = report::json(&outcome, args);
            serde_json::to_writer_pretty(&mut out, &document)?;
            writeln!(out)?;
        }
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn run_verify(args: &VerifyArgs) -> Result<ExitCode> {
    let root = canonical_root(&args.path)?;
    let normalized = VerifyArgs {
        path: root,
        format: args.format,
        threads: args.threads,
        cross_filesystems: args.cross_filesystems,
        tolerance_bytes: args.tolerance_bytes,
        progress: args.progress,
    };
    let (outcome, verdict) = verify::run(&normalized, wants_progress(args.progress))?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    match args.format {
        Format::Text => write!(out, "{}", verify::text(&outcome, &verdict))?,
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, &verify::json(&outcome, &verdict))?;
            writeln!(out)?;
        }
    }
    out.flush()?;

    if verdict.agrees {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(EXIT_DISAGREEMENT))
    }
}

/// Resolves the scan root once, up front.
///
/// The scan root is the one path in the system that is action authority; every
/// other path is derived from it plus stored components. Resolving symlinks
/// here — and only here — means the traversal never has to.
fn canonical_root(path: &Path) -> Result<std::path::PathBuf> {
    let resolved = std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    let metadata = std::fs::metadata(&resolved).with_context(|| format!("cannot read {}", resolved.display()))?;
    anyhow::ensure!(metadata.is_dir(), "{} is not a directory", resolved.display());
    Ok(resolved)
}

fn wants_progress(when: ProgressWhen) -> bool {
    match when {
        ProgressWhen::Always => true,
        ProgressWhen::Never => false,
        ProgressWhen::Auto => io::stderr().is_terminal(),
    }
}

/// Installs `tracing`, plus a Chrome trace layer when `--trace-chrome` is set.
///
/// The returned guard must stay alive for the whole run: dropping it flushes
/// the trace file.
fn init_tracing(chrome: Option<&Path>) -> Option<tracing_chrome::FlushGuard> {
    // `RUST_LOG` always wins. Without it, a normal run is quiet and a
    // `--trace-chrome` run turns on the per-directory and builder spans, since
    // a trace file with nothing in it is worse than no flag at all.
    let default = if chrome.is_some() { "rdirstat=debug" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let format = tracing_subscriber::fmt::layer().with_writer(io::stderr);

    match chrome {
        None => {
            tracing_subscriber::registry().with(filter).with(format).init();
            None
        }
        Some(path) => {
            let (layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
                .file(path)
                .include_args(true)
                .build();
            tracing_subscriber::registry()
                .with(filter)
                .with(format)
                .with(layer)
                .init();
            Some(guard)
        }
    }
}
