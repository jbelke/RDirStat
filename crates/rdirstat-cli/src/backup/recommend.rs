//! `backup recommend --profile`: every source of a profile planned as a dry
//! run, flattened into one ranked list. Writes nothing; applies nothing. The
//! document's schema is [`rdirstat_organize::recommend`].

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use rdirstat_core::format_iec;
use rdirstat_organize::recommend::{DestFill, Row, SourceSummary, Totals};
use rdirstat_organize::{
    DEFAULT_RESERVE_PRESSURE, PlanItem, RECOMMEND_SCHEMA_VERSION, Recommendation, RowStatus, Warning,
};

use super::DiskProbe;
use super::plan::PlanOutcome;
use super::profile::{PlanAll, Resolved, plan_all};

/// Default number of rows a recommendation lists.
pub(crate) const DEFAULT_LIMIT: usize = 1000;

/// Plans `profile` as a dry run and ranks the result.
///
/// # Errors
///
/// A source whose plan fails, or a destination whose facts cannot be read.
pub(crate) fn recommend(
    profile: &Resolved,
    limit: usize,
    progress: bool,
    probe: &dyn DiskProbe,
) -> Result<Recommendation> {
    let dest_facts = probe.facts(&profile.dest)?;
    let outcomes = plan_all(
        profile,
        &PlanAll {
            // Nothing is written on a dry run; these only satisfy the
            // "never under the source" checks the plan makes.
            catalog: super::catalog_dir(None).unwrap_or_else(|_| std::env::temp_dir().join("rdirstat-catalog")),
            app_catalog: super::catalog_dir(None).ok(),
            progress,
            rollup: true,
            dry_run: true,
            probe,
        },
    )?;
    Ok(build(
        profile,
        &outcomes,
        dest_facts.total_bytes,
        dest_facts.available_bytes,
        limit,
    ))
}

/// The document for already-built plans. `total` and `available` are the
/// destination's real facts, before any plan.
pub(crate) fn build(
    profile: &Resolved,
    outcomes: &[PlanOutcome],
    total: u64,
    available: u64,
    limit: usize,
) -> Recommendation {
    let mut rows = Vec::new();
    let mut sources = Vec::with_capacity(outcomes.len());
    let mut totals = Totals::default();
    let mut warnings = Vec::new();
    for (outcome, source) in outcomes.iter().zip(&profile.sources) {
        let plan = &outcome.plan;
        let label = &plan.source_label;
        let item_row = |item: &PlanItem, status| Row {
            rank: 0,
            source_label: label.clone(),
            relative: item.relative.to_string_lossy().into_owned(),
            status,
            kind: Some(item.kind),
            logical: Some(item.logical),
            allocated: item.allocated,
            rollup_files: item.rollup.as_ref().map(|r| r.files),
            rule_id: None,
            reason: None,
        };
        rows.extend(plan.items.iter().map(|item| item_row(item, RowStatus::Planned)));
        rows.extend(outcome.deferred.iter().map(|item| item_row(item, RowStatus::Deferred)));
        rows.extend(plan.skipped.iter().map(|item| Row {
            rank: 0,
            source_label: label.clone(),
            relative: item.relative.to_string_lossy().into_owned(),
            status: RowStatus::Skipped,
            kind: None,
            logical: None,
            allocated: item.allocated,
            rollup_files: None,
            rule_id: Some(item.rule_id.clone()),
            reason: Some(item.reason.clone()),
        }));
        let source_totals = Totals {
            planned_items: plan.items.len() as u64,
            planned_logical: outcome.total_logical,
            planned_allocated: outcome.total_allocated,
            deferred_items: outcome.deferred.len() as u64,
            deferred_logical: outcome.deferred.iter().map(|item| item.logical).sum(),
            deferred_allocated: outcome.deferred.iter().map(|item| item.allocated).sum(),
            skipped_items: plan.skipped_count,
            skipped_allocated: outcome.skipped_bytes,
            not_considered_items: outcome.not_considered,
            not_considered_allocated: outcome.not_considered_bytes,
        };
        totals.add(&source_totals);
        warnings.extend(outcome.warnings.iter().map(|warning| Warning {
            code: warning.code.clone(),
            text: format!("{label}: {}", warning.text),
        }));
        sources.push(SourceSummary {
            label: label.clone(),
            root: plan.source_root.to_string_lossy().into_owned(),
            includes: source
                .includes
                .iter()
                .map(|include| include.to_string_lossy().into_owned())
                .collect(),
            source_snapshot: plan.source_snapshot.clone(),
            totals: source_totals,
        });
    }
    rows.sort_by(|a, b| {
        b.allocated
            .cmp(&a.allocated)
            .then_with(|| a.source_label.cmp(&b.source_label))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    let rows_total = rows.len() as u64;
    rows.truncate(limit);
    for (rank, row) in (1_u64..).zip(rows.iter_mut()) {
        row.rank = rank;
    }
    let fill = DestFill::new(
        total,
        available,
        DEFAULT_RESERVE_PRESSURE,
        totals.planned_logical,
        totals.planned_logical.saturating_add(totals.deferred_logical),
    );
    Recommendation {
        schema_version: RECOMMEND_SCHEMA_VERSION,
        profile: profile.name.clone(),
        profile_path: profile.path.to_string_lossy().into_owned(),
        dest_root: profile.dest.to_string_lossy().into_owned(),
        created_at_ms: super::now_ms(),
        floor_allocated: profile.floor,
        review_cap: profile.cap,
        fill,
        sources,
        totals,
        rows_truncated: (rows.len() as u64) < rows_total,
        rows,
        rows_total,
        warnings,
    }
}

/// The recommendation as text.
pub(crate) fn text(doc: &Recommendation) -> String {
    let mut out = String::new();
    let fill = &doc.fill;
    let _ = writeln!(out, "Recommended backup: profile {} -> {}", doc.profile, doc.dest_root);
    let _ = writeln!(
        out,
        "  dest   {} used of {}; after planned {:.1}%, after everything wanted {:.1}% (limit {:.0}%): {}",
        format_iec(fill.used_bytes),
        format_iec(fill.total_bytes),
        fill.projected_fill_planned * 100.0,
        fill.projected_fill_wanted * 100.0,
        fill.reserve_pressure * 100.0,
        if fill.fits { "fits" } else { "does not fit in one run" }
    );
    for source in &doc.sources {
        let t = &source.totals;
        let scope = if source.includes.is_empty() {
            "all".to_owned()
        } else {
            source.includes.join(", ")
        };
        let _ = writeln!(
            out,
            "  {:<12} {} ({scope}): plan {} items {}, defer {} items {}, skip {} items {}",
            source.label,
            source.root,
            t.planned_items,
            format_iec(t.planned_logical),
            t.deferred_items,
            format_iec(t.deferred_logical),
            t.skipped_items,
            format_iec(t.skipped_allocated)
        );
    }
    let _ = writeln!(out);
    for row in &doc.rows {
        let status = match row.status {
            RowStatus::Planned => "plan",
            RowStatus::Deferred => "defer",
            RowStatus::Skipped => "skip",
        };
        let why = match (&row.rule_id, &row.reason) {
            (Some(id), Some(reason)) => format!("  [{id}: {reason}]"),
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "  {:>6}  {:>11}  {status:<5}  {}:{}{why}",
            row.rank,
            format_iec(row.allocated),
            row.source_label,
            row.relative
        );
    }
    if doc.rows_truncated {
        let _ = writeln!(out, "  ... {} more rows", doc.rows_total - doc.rows.len() as u64);
    }
    for warning in &doc.warnings {
        let _ = writeln!(out, "Warning [{}]: {}", warning.code, warning.text);
    }
    let _ = writeln!(
        out,
        "Dry run: nothing was written. Profile: {}",
        Path::new(&doc.profile_path).display()
    );
    out
}
