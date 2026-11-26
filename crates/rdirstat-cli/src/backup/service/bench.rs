//! `rdirstat-cli backup bench` (hidden, `phase-timing` builds only): runs one
//! backup job through the service's own [`Runner`] — plan, apply, journal,
//! catalog, index — and prints wall time split per phase as JSON.
//!
//! The harness is `scripts/bench-backup.sh`; the contract is
//! `.settings/docs/12-THROUGHPUT.md`, slice 0.
//!
//! One deliberate difference from a real job: the disk-pair rule refuses a
//! source and destination on the same physical disk, and an SSD-to-SSD
//! baseline puts both on one. [`BenchProbe`] reports the source's disk as a
//! distinct `bench-source` disk so the run is allowed. And the planner
//! fills a destination to at most 75% of its size; a nearly full scratch
//! disk would plan nothing, so the destination is reported as four times
//! its real size with the same bytes used. That changes what is planned,
//! never how it is copied. Every other fact is the real one from the OS.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use rdirstat_copy::phase;
use rdirstat_jobs::{DEFAULT_FLOOR, JOB_FILE, JobSource, JobSpec, Phase, SCHEMA_VERSION};
use rdirstat_organize::DiskFacts;
use serde_json::{Value, json};

use super::record::{Record, write_atomic};
use super::runner::Runner;
use crate::backup::{DiskProbe, SystemProbe, now_ms};

/// What one bench run is asked.
#[derive(Debug)]
pub(crate) struct BenchRequest {
    pub(crate) source: PathBuf,
    pub(crate) dest: PathBuf,
    pub(crate) catalog: PathBuf,
    pub(crate) label: String,
    pub(crate) floor: u64,
}

/// The real probe, except that paths under `source` sit on their own disk.
struct BenchProbe {
    source: PathBuf,
}

impl DiskProbe for BenchProbe {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let mut facts = SystemProbe.facts(path)?;
        if path.starts_with(&self.source) {
            facts.disk_id = Some("bench-source".to_owned());
            facts.volume_uuid = Some("BENCH-SOURCE".to_owned());
            facts.st_dev = facts.st_dev.wrapping_add(1 << 30);
        } else {
            let used = facts.total_bytes.saturating_sub(facts.available_bytes);
            facts.total_bytes = facts.total_bytes.saturating_mul(4);
            facts.available_bytes = facts.total_bytes.saturating_sub(used);
        }
        Ok(facts)
    }
}

/// Runs the job and returns the report.
///
/// # Errors
///
/// A source or destination that cannot be resolved, or a job directory that
/// cannot be written.
pub(crate) fn bench(request: &BenchRequest) -> Result<Value> {
    let source =
        std::fs::canonicalize(&request.source).with_context(|| format!("cannot resolve {}", request.source.display()))?;
    let dest =
        std::fs::canonicalize(&request.dest).with_context(|| format!("cannot resolve {}", request.dest.display()))?;
    std::fs::create_dir_all(&request.catalog)
        .with_context(|| format!("cannot create {}", request.catalog.display()))?;
    let catalog = std::fs::canonicalize(&request.catalog)?;
    if dest.starts_with(&source) || source.starts_with(&dest) || catalog.starts_with(&source) {
        bail!("source, destination and catalog must not be inside one another");
    }

    let id = format!("bench-{}", now_ms());
    let spec = JobSpec {
        schema_version: SCHEMA_VERSION,
        id: id.clone(),
        created_at_ms: now_ms(),
        dest: dest.clone(),
        sources: vec![JobSource {
            label: request.label.clone(),
            root: source.clone(),
            include: Vec::new(),
        }],
        rules: None,
        floor: request.floor,
        max_rounds: 1,
        review_cap: None,
        rollup: true,
        catalog: Some(catalog),
        mount_poll_ms: 1000,
        mount_timeout_ms: Some(0),
        profile: None,
    };
    let dir = rdirstat_jobs::job_dir(&dest, &id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    write_atomic(&dir, JOB_FILE, &spec).with_context(|| format!("cannot write {}", dir.display()))?;
    let record = Mutex::new(Record::load(&dir, &spec)?);
    let probe = BenchProbe { source };
    let shutdown = AtomicBool::new(false);
    let runner = Runner {
        spec: &spec,
        record: &record,
        probe: &probe,
        shutdown: &shutdown,
        default_catalog: None,
        app_catalog: None,
        on_item: None,
    };

    phase::reset();
    let started = Instant::now();
    let ended = runner.run();
    let wall = started.elapsed().as_secs_f64();
    let totals = phase::snapshot();

    let record = record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = record.state();
    let counts: serde_json::Map<String, Value> = state
        .counts
        .iter()
        .map(|(outcome, tally)| {
            (
                serde_json::to_value(outcome)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                json!({ "items": tally.items, "bytes": tally.bytes }),
            )
        })
        .collect();
    let mut attributed = 0.0;
    let mut phases = Vec::with_capacity(totals.len() + 1);
    for (phase, total) in &totals {
        let seconds = std::time::Duration::from_nanos(total.nanos).as_secs_f64();
        attributed += seconds;
        phases.push(json!({ "phase": phase.key(), "seconds": seconds, "calls": total.calls, "bytes": total.bytes }));
    }
    // The runner between phases: gates, state, round bookkeeping.
    phases.push(json!({ "phase": "unattributed", "seconds": (wall - attributed).max(0.0), "calls": 0, "bytes": 0 }));
    let landed = state.labels.first().map_or((0, 0), |l| (l.landed_items, l.landed_bytes));
    Ok(json!({
        "job": id,
        "job_dir": dir.to_string_lossy(),
        "ended": ended,
        "complete": ended == Phase::Done,
        "last_error": state.last_error,
        "wall_seconds": wall,
        "landed_items": landed.0,
        "landed_bytes": landed.1,
        "counts": counts,
        "phases": phases,
    }))
}

/// The default floor of a real job, so roll-ups cover what a real job's do.
pub(crate) const BENCH_FLOOR: u64 = DEFAULT_FLOOR;
