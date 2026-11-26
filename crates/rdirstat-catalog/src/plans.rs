//! `plans`: what a Backup plan chose and what it skipped, one row per item.
//!
//! `backup apply` writes the rows of the plan it ran as part of its run, so
//! the drive's own catalog says what was meant to land (pending = planned
//! rows without a receipt) and what was left out on purpose, by which rule.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use arrow::array::{ArrayRef, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use serde::{Deserialize, Serialize};

use crate::CatalogError;
use crate::rows::Columns;
use crate::store::{list_parts, read_batches, write_part};

/// Part-file kind for plan rows.
pub(crate) const PLANS: &str = "plans";

/// [`PlanRow::status`] of an item the plan copies.
pub const PLAN_STATUS_PLANNED: &str = "planned";
/// [`PlanRow::status`] of a candidate a rule (or a change since the scan)
/// left out.
pub const PLAN_STATUS_SKIPPED: &str = "skipped";

/// One planned or skipped item of one plan.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanRow {
    /// `plan-<label>-<created_at_ms>`; scan entries the plan was made from
    /// are written under the same run id.
    pub plan_id: String,
    /// The plan's source label.
    pub source_label: String,
    /// [`PLAN_STATUS_PLANNED`] or [`PLAN_STATUS_SKIPPED`].
    pub status: String,
    /// Path relative to the source volume root (lossy UTF-8).
    pub relative: String,
    /// Absolute source path (lossy UTF-8).
    pub source_path: String,
    /// `file` or `package` for a planned item; empty for a skipped one.
    pub kind: String,
    /// Logical bytes (zero for a skipped item: the plan records allocated only).
    pub logical: u64,
    /// Allocated bytes on the source.
    pub allocated: u64,
    /// The skip rule id; empty for a planned item.
    pub rule_id: String,
    /// The skip rule's reason; empty for a planned item.
    pub reason: String,
    /// The plan's floor on allocated bytes.
    pub floor_allocated: u64,
    /// When the plan was made, milliseconds since the Unix epoch.
    pub created_at_ms: i64,
}

pub(crate) static PLAN_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("plan_id", DataType::Utf8, false),
        Field::new("source_label", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("relative", DataType::Utf8, false),
        Field::new("source_path", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("logical", DataType::UInt64, false),
        Field::new("allocated", DataType::UInt64, false),
        Field::new("rule_id", DataType::Utf8, false),
        Field::new("reason", DataType::Utf8, false),
        Field::new("floor_allocated", DataType::UInt64, false),
        Field::new("created_at_ms", DataType::Int64, false),
    ]))
});

fn strings<'a>(values: impl Iterator<Item = &'a str>) -> ArrayRef {
    Arc::new(StringArray::from_iter_values(values))
}

/// Writes `rows` as a new `plans-<run_id>-<seq>.parquet` in `out_dir`.
///
/// # Errors
///
/// [`CatalogError::InvalidRow`] if a `status` is neither planned nor skipped
/// (checked before anything is written), [`CatalogError::InvalidRunId`], or
/// an I/O or encoder failure.
pub fn write_plan_rows(rows: &[PlanRow], out_dir: &Path, run_id: &str) -> Result<PathBuf, CatalogError> {
    for (index, row) in rows.iter().enumerate() {
        if !matches!(row.status.as_str(), PLAN_STATUS_PLANNED | PLAN_STATUS_SKIPPED) {
            return Err(CatalogError::InvalidRow {
                index,
                detail: format!("status {:?} is not \"planned\" or \"skipped\"", row.status),
            });
        }
    }
    let batch = RecordBatch::try_new(
        Arc::clone(&PLAN_SCHEMA),
        vec![
            strings(rows.iter().map(|r| r.plan_id.as_str())),
            strings(rows.iter().map(|r| r.source_label.as_str())),
            strings(rows.iter().map(|r| r.status.as_str())),
            strings(rows.iter().map(|r| r.relative.as_str())),
            strings(rows.iter().map(|r| r.source_path.as_str())),
            strings(rows.iter().map(|r| r.kind.as_str())),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.logical))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.allocated))),
            strings(rows.iter().map(|r| r.rule_id.as_str())),
            strings(rows.iter().map(|r| r.reason.as_str())),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.floor_allocated))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.created_at_ms))),
        ],
    )?;
    write_part(out_dir, PLANS, run_id, &PLAN_SCHEMA, |writer| Ok(writer.write(&batch)?))
}

/// Every row of every `plans-*.parquet` in `dir`, in file-name order.
///
/// A missing `dir` reads as no rows.
///
/// # Errors
///
/// [`CatalogError::Schema`] if a part file lacks a column or holds a null,
/// or an I/O or decoder failure.
pub fn read_plan_rows(dir: &Path) -> Result<Vec<PlanRow>, CatalogError> {
    let mut rows = Vec::new();
    for part in list_parts(dir, PLANS)? {
        for batch in read_batches(&part)? {
            let c = Columns::new(&batch, &part);
            let plan_id = c.utf8("plan_id")?;
            let source_label = c.utf8("source_label")?;
            let status = c.utf8("status")?;
            let relative = c.utf8("relative")?;
            let source_path = c.utf8("source_path")?;
            let kind = c.utf8("kind")?;
            let logical: &UInt64Array = c.u64("logical")?;
            let allocated = c.u64("allocated")?;
            let rule_id = c.utf8("rule_id")?;
            let reason = c.utf8("reason")?;
            let floor = c.u64("floor_allocated")?;
            let created = c.i64("created_at_ms")?;
            rows.extend((0..batch.num_rows()).map(|i| PlanRow {
                plan_id: plan_id.value(i).to_owned(),
                source_label: source_label.value(i).to_owned(),
                status: status.value(i).to_owned(),
                relative: relative.value(i).to_owned(),
                source_path: source_path.value(i).to_owned(),
                kind: kind.value(i).to_owned(),
                logical: logical.value(i),
                allocated: allocated.value(i),
                rule_id: rule_id.value(i).to_owned(),
                reason: reason.value(i).to_owned(),
                floor_allocated: floor.value(i),
                created_at_ms: created.value(i),
            }));
        }
    }
    Ok(rows)
}
