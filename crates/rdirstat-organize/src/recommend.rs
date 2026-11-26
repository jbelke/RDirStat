//! The "recommended" preview of a backup profile: what a run would copy,
//! defer, and skip, ranked largest first. This is the JSON document
//! `rdirstat backup recommend --profile <path> --json` prints and the GUI's
//! Backup preview renders; these types are its schema.
//!
//! A recommendation is a dry run. Producing it walks the sources and reads
//! the drive's catalog, and writes nothing anywhere: no plan file, no catalog
//! part, nothing on the destination.
//!
//! Byte fields follow the plan: `logical` is what a copy writes (the
//! destination budget is in logical bytes), `allocated` is what the source
//! spends and what rows are ranked by. The two are never summed together.

use serde::{Deserialize, Serialize};

use crate::fit::{ItemKind, Warning};

/// The recommendation format this build writes.
pub const RECOMMEND_SCHEMA_VERSION: u32 = 1;

/// The whole document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    /// Always [`RECOMMEND_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The profile's name.
    pub profile: String,
    /// Path of the profile file the recommendation was made from.
    pub profile_path: String,
    /// Destination root (canonical).
    pub dest_root: String,
    /// Milliseconds since the Unix epoch when the recommendation was made.
    pub created_at_ms: i64,
    /// Floor and review cap the plans used.
    pub floor_allocated: u64,
    /// Review cap on ranked candidates per source.
    pub review_cap: usize,
    /// Destination capacity and whether everything wanted fits.
    pub fill: DestFill,
    /// Per-source totals, in profile order.
    pub sources: Vec<SourceSummary>,
    /// Sums over every source.
    pub totals: Totals,
    /// Ranked rows over every source: allocated bytes descending, then
    /// source label, then relative path. At most `--limit` of them.
    pub rows: Vec<Row>,
    /// Rows before the limit was applied.
    pub rows_total: u64,
    /// `rows.len() < rows_total`.
    pub rows_truncated: bool,
    /// Plan warnings, each prefixed with its source label in `text`.
    pub warnings: Vec<Warning>,
}

/// The destination and the 75 % fill rule.
///
/// `used = total - available`. A fill fraction is `(used + logical) / total`.
/// The rule: a plan never takes the destination to `reserve_pressure` or
/// beyond.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DestFill {
    /// Destination capacity in bytes.
    pub total_bytes: u64,
    /// Free bytes on the destination now.
    pub available_bytes: u64,
    /// `total_bytes - available_bytes`.
    pub used_bytes: u64,
    /// The fill limit, a fraction (0.75).
    pub reserve_pressure: f64,
    /// Logical bytes of every planned item over all sources.
    pub planned_logical: u64,
    /// Planned plus deferred: everything the profile wants that no rule skips.
    pub wanted_logical: u64,
    /// Fill after the planned items land.
    pub projected_fill_planned: f64,
    /// Fill if everything wanted landed.
    pub projected_fill_wanted: f64,
    /// `projected_fill_wanted < reserve_pressure`: the whole profile fits in
    /// one run. When false, a run lands the planned rows and defers the rest.
    pub fits: bool,
}

/// One source's share of the recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSummary {
    /// Backup label.
    pub label: String,
    /// Source root (canonical).
    pub root: String,
    /// Includes relative to the root; empty means the whole root.
    pub includes: Vec<String>,
    /// The snapshot identity the plan was built from.
    pub source_snapshot: String,
    /// This source's totals.
    pub totals: Totals,
}

/// Item counts and bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Items a run copies.
    pub planned_items: u64,
    /// Their logical bytes.
    pub planned_logical: u64,
    /// Their allocated bytes.
    pub planned_allocated: u64,
    /// Items that do not fit under the fill rule this run.
    pub deferred_items: u64,
    /// Their logical bytes.
    pub deferred_logical: u64,
    /// Their allocated bytes.
    pub deferred_allocated: u64,
    /// Candidates a rule (or a change since the scan) left out.
    pub skipped_items: u64,
    /// Their allocated bytes.
    pub skipped_allocated: u64,
    /// Files no item covers (past the review cap, or a name the plan cannot
    /// carry).
    pub not_considered_items: u64,
    /// Their allocated bytes.
    pub not_considered_allocated: u64,
}

impl Totals {
    /// Adds `other` into `self`, saturating.
    pub fn add(&mut self, other: &Totals) {
        self.planned_items = self.planned_items.saturating_add(other.planned_items);
        self.planned_logical = self.planned_logical.saturating_add(other.planned_logical);
        self.planned_allocated = self.planned_allocated.saturating_add(other.planned_allocated);
        self.deferred_items = self.deferred_items.saturating_add(other.deferred_items);
        self.deferred_logical = self.deferred_logical.saturating_add(other.deferred_logical);
        self.deferred_allocated = self.deferred_allocated.saturating_add(other.deferred_allocated);
        self.skipped_items = self.skipped_items.saturating_add(other.skipped_items);
        self.skipped_allocated = self.skipped_allocated.saturating_add(other.skipped_allocated);
        self.not_considered_items = self.not_considered_items.saturating_add(other.not_considered_items);
        self.not_considered_allocated = self
            .not_considered_allocated
            .saturating_add(other.not_considered_allocated);
    }
}

/// What a run would do with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStatus {
    /// Copied by the run.
    Planned,
    /// Wanted, but past the fill rule this run.
    Deferred,
    /// Left out; `rule_id` and `reason` say why.
    Skipped,
}

/// One ranked row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// 1-based rank over every row of every source.
    pub rank: u64,
    /// Which source the row belongs to.
    pub source_label: String,
    /// Path relative to the source root (lossy if not UTF-8).
    pub relative: String,
    /// Planned, deferred, or skipped.
    pub status: RowStatus,
    /// Object kind. Absent on skipped rows: a skip names a path, not a kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ItemKind>,
    /// Logical bytes. Absent on skipped rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical: Option<u64>,
    /// Allocated bytes on the source; the ranking key.
    pub allocated: u64,
    /// Files inside a directory roll-up row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollup_files: Option<u64>,
    /// Skipped rows: the rule id (`builtin:…` for built-in reasons).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Skipped rows: the rule's reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `(used + logical) / total`, or `f64::INFINITY` when `total` is zero.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "a fill fraction for display and a < 0.75 comparison; 2^-52 relative error is irrelevant"
)]
pub fn fill_fraction(used: u64, logical: u64, total: u64) -> f64 {
    if total == 0 {
        return f64::INFINITY;
    }
    (used as f64 + logical as f64) / total as f64
}

impl DestFill {
    /// The fill block for a destination and the planned / wanted bytes.
    #[must_use]
    pub fn new(
        total_bytes: u64,
        available_bytes: u64,
        reserve_pressure: f64,
        planned_logical: u64,
        wanted_logical: u64,
    ) -> DestFill {
        let used_bytes = total_bytes.saturating_sub(available_bytes);
        let projected_fill_planned = fill_fraction(used_bytes, planned_logical, total_bytes);
        let projected_fill_wanted = fill_fraction(used_bytes, wanted_logical, total_bytes);
        DestFill {
            total_bytes,
            available_bytes,
            used_bytes,
            reserve_pressure,
            planned_logical,
            wanted_logical,
            projected_fill_planned,
            projected_fill_wanted,
            fits: projected_fill_wanted < reserve_pressure,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

    use super::*;

    #[test]
    fn fill_follows_the_seventy_five_percent_rule() {
        let fits = DestFill::new(100, 60, 0.75, 10, 30);
        assert_eq!(fits.used_bytes, 40);
        assert!(fits.fits, "40 + 30 = 70 % < 75 %");
        let over = DestFill::new(100, 60, 0.75, 10, 35);
        assert!(!over.fits, "75 % is not below 75 %");
        assert!(!DestFill::new(0, 0, 0.75, 0, 0).fits);
    }

    #[test]
    fn skipped_rows_omit_kind_and_logical() {
        let row = Row {
            rank: 1,
            source_label: "a".into(),
            relative: "x".into(),
            status: RowStatus::Skipped,
            kind: None,
            logical: None,
            allocated: 5,
            rollup_files: None,
            rule_id: Some("builtin:cache".into()),
            reason: Some("rebuildable".into()),
        };
        let json = serde_json::to_value(&row).expect("json");
        assert_eq!(json["status"], "skipped");
        assert!(json.get("kind").is_none());
        assert!(json.get("logical").is_none());
        let back: Row = serde_json::from_value(json).expect("parse");
        assert_eq!(back, row);
    }
}
