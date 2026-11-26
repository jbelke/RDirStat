//! `rdirstat backup index` and `rdirstat backup status`: what the drive holds,
//! from the drive's own Parquet. Neither walks the drive or the source.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rdirstat_catalog::{
    IndexRow, PLAN_STATUS_PLANNED, PLAN_STATUS_SKIPPED, PlanRow, build_index, dest_catalog_dir, read_plan_rows,
    visit_entries,
};
use rdirstat_core::format_iec;
use serde::Serialize;
use serde_json::{Value, json};

/// Largest not-yet-backed-up files listed per source.
const TOP_MISSING: usize = 10;

/// Count and logical bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Tally {
    pub(crate) items: u64,
    pub(crate) bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.items += 1;
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

/// One source label's state on the drive.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SourceStatus {
    pub(crate) source_label: String,
    /// Landed destination paths from this source (logical bytes).
    pub(crate) backed_up: Tally,
    /// Of those, landed as `clonefile(2)` clones: logical bytes that took no
    /// new blocks.
    pub(crate) cloned: Tally,
    /// The most recent plan the drive has rows for.
    pub(crate) last_plan: Option<String>,
    /// Planned items of the last plan with no receipt (logical bytes).
    pub(crate) pending: Tally,
    /// Candidates the last plan skipped, by rule (allocated bytes).
    pub(crate) skipped: Tally,
    /// The last plan's floor on allocated bytes.
    pub(crate) floor_allocated: Option<u64>,
    /// Files in the last plan's scan at or over the floor that are neither
    /// backed up (themselves or inside a backed-up package) nor skipped by a
    /// rule (allocated bytes). `None` when the drive has no scan entries for
    /// the last plan.
    pub(crate) not_backed_up: Option<Tally>,
    /// The largest of those, `(path, allocated)`.
    pub(crate) largest_not_backed_up: Vec<(String, u64)>,
}

/// The whole drive.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusReport {
    pub(crate) dest: PathBuf,
    pub(crate) catalog: PathBuf,
    pub(crate) sources: Vec<SourceStatus>,
    /// Spotlight caveats for the drive, if any.
    pub(crate) warnings: Vec<String>,
}

/// Builds the status of `dest` from `<dest>/.rdirstat/catalog`, optionally
/// for one source label.
///
/// # Errors
///
/// No catalog on the drive, or an unreadable part.
pub(crate) fn status(dest: &Path, only: Option<&str>) -> Result<StatusReport> {
    let catalog = dest_catalog_dir(dest);
    if !catalog.is_dir() {
        bail!(
            "{} has no catalog at {}; nothing has been backed up to it",
            dest.display(),
            catalog.display()
        );
    }
    let index = build_index(&catalog).with_context(|| format!("reading receipts in {}", catalog.display()))?;
    let plans = read_plan_rows(&catalog).with_context(|| format!("reading plans in {}", catalog.display()))?;

    let labels: BTreeSet<&str> = index
        .iter()
        .map(|r| r.source_label.as_str())
        .chain(plans.iter().map(|r| r.source_label.as_str()))
        .collect();
    let mut sources = Vec::new();
    for label in labels {
        if only.is_some_and(|wanted| wanted != label) {
            continue;
        }
        sources.push(source_status(&catalog, label, &index, &plans)?);
    }
    if let Some(wanted) = only
        && sources.is_empty()
    {
        bail!("the drive has no receipts or plans for source label {wanted:?}");
    }
    Ok(StatusReport {
        dest: dest.to_path_buf(),
        catalog,
        sources,
        warnings: spotlight_marker_warning(dest).into_iter().collect(),
    })
}

fn source_status(catalog: &Path, label: &str, index: &[IndexRow], plans: &[PlanRow]) -> Result<SourceStatus> {
    let mine: Vec<&IndexRow> = index.iter().filter(|r| r.source_label == label).collect();
    let mut backed_up = Tally::default();
    let mut cloned = Tally::default();
    for row in &mine {
        backed_up.add(row.size);
        if row.method == "clone" {
            cloned.add(row.size);
        }
    }
    let landed: HashSet<&str> = mine.iter().map(|r| r.source_path.as_str()).collect();

    let last_plan = plans
        .iter()
        .filter(|r| r.source_label == label)
        .max_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.plan_id.cmp(&b.plan_id))
        })
        .map(|r| r.plan_id.clone());
    let mut status = SourceStatus {
        source_label: label.to_owned(),
        backed_up,
        cloned,
        last_plan: last_plan.clone(),
        pending: Tally::default(),
        skipped: Tally::default(),
        floor_allocated: None,
        not_backed_up: None,
        largest_not_backed_up: Vec::new(),
    };
    let Some(plan_id) = last_plan else {
        return Ok(status);
    };

    // A plan applied twice wrote its rows twice; count each item once.
    let mut seen = HashSet::new();
    let mut skipped_paths = HashSet::new();
    for row in plans.iter().filter(|r| r.plan_id == plan_id) {
        status.floor_allocated = Some(row.floor_allocated);
        if !seen.insert((row.status.as_str(), row.source_path.as_str())) {
            continue;
        }
        match row.status.as_str() {
            PLAN_STATUS_PLANNED if !landed.contains(row.source_path.as_str()) => status.pending.add(row.logical),
            PLAN_STATUS_SKIPPED => {
                status.skipped.add(row.allocated);
                skipped_paths.insert(row.source_path.as_str());
            }
            _ => {}
        }
    }

    let floor = status.floor_allocated.unwrap_or(0);
    let mut found_entries = false;
    let mut missing = Tally::default();
    let mut largest: Vec<(String, u64)> = Vec::new();
    visit_entries(catalog, Some(&plan_id), |entry| {
        found_entries = true;
        if entry.source_label != label || entry.kind != "file" || entry.allocated < floor {
            return;
        }
        if covered(entry.path, &landed) || covered(entry.path, &skipped_paths) {
            return;
        }
        missing.add(entry.allocated);
        largest.push((entry.path.to_owned(), entry.allocated));
        if largest.len() > TOP_MISSING * 8 {
            keep_largest(&mut largest);
        }
    })
    .with_context(|| format!("reading scan entries of {plan_id}"))?;
    if found_entries {
        keep_largest(&mut largest);
        status.not_backed_up = Some(missing);
        status.largest_not_backed_up = largest;
    }
    Ok(status)
}

fn keep_largest(rows: &mut Vec<(String, u64)>) {
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(TOP_MISSING);
}

/// `path` or one of its ancestors is in `set`.
fn covered(path: &str, set: &HashSet<&str>) -> bool {
    let mut current = Some(Path::new(path));
    while let Some(p) = current {
        if p.to_str().is_some_and(|s| set.contains(s)) {
            return true;
        }
        current = p.parent();
    }
    false
}

/// A warning when the drive root lacks `.metadata_never_index` (one `lstat`;
/// the drive is not walked).
pub(crate) fn spotlight_marker_warning(dest: &Path) -> Option<String> {
    let marker = dest.join(".metadata_never_index");
    (std::fs::symlink_metadata(&marker).is_err()).then(|| {
        format!(
            "{} is missing; Spotlight may index the backup drive. Create it, then run once: sudo mdutil -i off -E {}",
            marker.display(),
            dest.display()
        )
    })
}

/// A warning when `mdutil -s` says indexing is on for `dest`. Never runs
/// sudo; an `mdutil` that cannot answer yields no warning.
pub(crate) fn mdutil_warning(dest: &Path) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/mdutil")
        .arg("-s")
        .arg(dest)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("Indexing enabled").then(|| {
        format!(
            "Spotlight indexing is enabled on {}; run once: sudo mdutil -i off -E {}",
            dest.display(),
            dest.display()
        )
    })
}

/// The status report as text.
pub(crate) fn text(report: &StatusReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Backup status: {} (catalog {})",
        report.dest.display(),
        report.catalog.display()
    );
    if report.sources.is_empty() {
        let _ = writeln!(out, "  nothing backed up yet");
    }
    for source in &report.sources {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", source.source_label);
        let _ = writeln!(
            out,
            "  backed up       {:>9} files  {}",
            source.backed_up.items,
            format_iec(source.backed_up.bytes)
        );
        let _ = writeln!(
            out,
            "  clone savings   {:>9} clones {}",
            source.cloned.items,
            format_iec(source.cloned.bytes)
        );
        match &source.last_plan {
            Some(plan) => {
                let _ = writeln!(out, "  last plan       {plan}");
                let _ = writeln!(
                    out,
                    "  pending         {:>9} items  {}",
                    source.pending.items,
                    format_iec(source.pending.bytes)
                );
                let _ = writeln!(
                    out,
                    "  skipped by rule {:>9} items  {} allocated",
                    source.skipped.items,
                    format_iec(source.skipped.bytes)
                );
            }
            None => {
                let _ = writeln!(out, "  last plan       (none on this drive)");
            }
        }
        match &source.not_backed_up {
            Some(missing) => {
                let _ = writeln!(
                    out,
                    "  not backed up   {:>9} files  {} allocated, each >= {}",
                    missing.items,
                    format_iec(missing.bytes),
                    format_iec(source.floor_allocated.unwrap_or(0))
                );
                for (path, bytes) in &source.largest_not_backed_up {
                    let _ = writeln!(out, "    {:>11}  {path}", format_iec(*bytes));
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "  not backed up   unknown: the drive holds no scan entries for the last plan"
                );
            }
        }
    }
    if !report.warnings.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Warnings:");
        for warning in &report.warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
    out
}

/// The status report as one JSON document.
pub(crate) fn json(report: &StatusReport) -> Value {
    json!({
        "dest": report.dest.to_string_lossy(),
        "catalog": report.catalog.to_string_lossy(),
        "sources": report.sources,
        "warnings": report.warnings,
    })
}
