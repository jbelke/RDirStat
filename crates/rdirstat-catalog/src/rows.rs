//! `hashes` and `receipts`: small row types with a write and a read.

use std::path::Path;
use std::sync::{Arc, LazyLock};

use arrow::array::{Array, ArrayRef, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use serde::{Deserialize, Serialize};

use crate::CatalogError;
use crate::store::{list_parts, read_batches, write_part};

/// Part-file kind for content hashes.
const HASHES: &str = "hashes";
/// Part-file kind for receipts.
const RECEIPTS: &str = "receipts";

/// One content hash observed for a file at a known identity.
///
/// `(dev, ino, size, mtime_ns)` is the identity the file had when it was
/// hashed; a later stat that disagrees means the hash no longer describes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HashRow {
    /// Absolute path when hashed (lossy UTF-8 if the name was not UTF-8).
    pub path: String,
    /// `st_dev` when hashed.
    pub dev: u64,
    /// `st_ino` when hashed.
    pub ino: u64,
    /// Logical size in bytes.
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// SHA-256 of the content, 64 lowercase hex digits.
    pub sha256: String,
}

/// Proof that one source file landed at one destination path and verified.
///
/// Written only after the destination compared equal to the source; a
/// receipt for a destination that failed compare is a defect.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReceiptRow {
    /// The backup run that wrote this receipt.
    pub run_id: String,
    /// The plan's source label, e.g. `mac-data`.
    pub source_label: String,
    /// Absolute source path.
    pub source_path: String,
    /// Source `st_dev` at copy time.
    pub source_dev: u64,
    /// Source `st_ino` at copy time.
    pub source_ino: u64,
    /// Logical size in bytes.
    pub size: u64,
    /// Source modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Absolute destination path.
    pub dest_path: String,
    /// `"copy"` (ditto, then compare) or `"clone"` (`clonefile(2)` of an
    /// already landed file with the same size and SHA-256).
    pub method: String,
    /// SHA-256 of the content, 64 lowercase hex digits.
    pub sha256: String,
    /// When verification finished, milliseconds since the Unix epoch.
    pub verified_at_ms: i64,
}

pub(crate) static HASH_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("dev", DataType::UInt64, false),
        Field::new("ino", DataType::UInt64, false),
        Field::new("size", DataType::UInt64, false),
        Field::new("mtime_ns", DataType::Int64, false),
        Field::new("sha256", DataType::Utf8, false),
    ]))
});

pub(crate) static RECEIPT_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::Utf8, false),
        Field::new("source_label", DataType::Utf8, false),
        Field::new("source_path", DataType::Utf8, false),
        Field::new("source_dev", DataType::UInt64, false),
        Field::new("source_ino", DataType::UInt64, false),
        Field::new("size", DataType::UInt64, false),
        Field::new("mtime_ns", DataType::Int64, false),
        Field::new("dest_path", DataType::Utf8, false),
        Field::new("method", DataType::Utf8, false),
        Field::new("sha256", DataType::Utf8, false),
        Field::new("verified_at_ms", DataType::Int64, false),
    ]))
});

fn check_sha(index: usize, sha256: &str) -> Result<(), CatalogError> {
    if sha256.len() == 64 && sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(CatalogError::InvalidRow {
            index,
            detail: format!("sha256 {sha256:?} is not 64 lowercase hex digits"),
        })
    }
}

fn strings<'a>(values: impl Iterator<Item = &'a str>) -> ArrayRef {
    Arc::new(StringArray::from_iter_values(values))
}

/// Writes `rows` as a new `hashes-<run_id>-<seq>.parquet` in `out_dir`.
///
/// # Errors
///
/// [`CatalogError::InvalidRow`] if a `sha256` is not 64 lowercase hex digits
/// (checked before anything is written), [`CatalogError::InvalidRunId`], or an
/// I/O or encoder failure.
pub fn write_hashes(rows: &[HashRow], out_dir: &Path, run_id: &str) -> Result<std::path::PathBuf, CatalogError> {
    for (index, row) in rows.iter().enumerate() {
        check_sha(index, &row.sha256)?;
    }
    let batch = RecordBatch::try_new(
        Arc::clone(&HASH_SCHEMA),
        vec![
            strings(rows.iter().map(|r| r.path.as_str())),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.dev))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.ino))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.size))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.mtime_ns))),
            strings(rows.iter().map(|r| r.sha256.as_str())),
        ],
    )?;
    write_part(
        out_dir,
        HASHES,
        run_id,
        &HASH_SCHEMA,
        |writer| Ok(writer.write(&batch)?),
    )
}

/// Writes `rows` as a new `receipts-<run_id>-<seq>.parquet` in `out_dir`.
///
/// # Errors
///
/// [`CatalogError::InvalidRow`] if a `sha256` is malformed or `method` is not
/// `"copy"` or `"clone"` (checked before anything is written),
/// [`CatalogError::InvalidRunId`], or an I/O or encoder failure.
pub fn write_receipts(rows: &[ReceiptRow], out_dir: &Path, run_id: &str) -> Result<std::path::PathBuf, CatalogError> {
    for (index, row) in rows.iter().enumerate() {
        check_sha(index, &row.sha256)?;
        if !matches!(row.method.as_str(), "copy" | "clone") {
            return Err(CatalogError::InvalidRow {
                index,
                detail: format!("method {:?} is not \"copy\" or \"clone\"", row.method),
            });
        }
    }
    let batch = RecordBatch::try_new(
        Arc::clone(&RECEIPT_SCHEMA),
        vec![
            strings(rows.iter().map(|r| r.run_id.as_str())),
            strings(rows.iter().map(|r| r.source_label.as_str())),
            strings(rows.iter().map(|r| r.source_path.as_str())),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.source_dev))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.source_ino))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.size))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.mtime_ns))),
            strings(rows.iter().map(|r| r.dest_path.as_str())),
            strings(rows.iter().map(|r| r.method.as_str())),
            strings(rows.iter().map(|r| r.sha256.as_str())),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.verified_at_ms))),
        ],
    )?;
    write_part(out_dir, RECEIPTS, run_id, &RECEIPT_SCHEMA, |writer| {
        Ok(writer.write(&batch)?)
    })
}

/// Typed, null-free access to one batch's columns by name.
pub(crate) struct Columns<'a> {
    batch: &'a RecordBatch,
    path: &'a Path,
}

impl<'a> Columns<'a> {
    pub(crate) fn new(batch: &'a RecordBatch, path: &'a Path) -> Self {
        Self { batch, path }
    }

    fn get<T: Array + 'static>(&self, name: &str, type_name: &str) -> Result<&'a T, CatalogError> {
        let schema_error = |detail: String| CatalogError::Schema {
            path: self.path.to_path_buf(),
            detail,
        };
        let column = self
            .batch
            .column_by_name(name)
            .ok_or_else(|| schema_error(format!("missing column {name:?}")))?;
        if column.null_count() > 0 {
            return Err(schema_error(format!("column {name:?} has nulls")));
        }
        column.as_any().downcast_ref::<T>().ok_or_else(|| {
            schema_error(format!(
                "column {name:?} is {}, expected {type_name}",
                column.data_type()
            ))
        })
    }

    pub(crate) fn utf8(&self, name: &str) -> Result<&'a StringArray, CatalogError> {
        self.get(name, "Utf8")
    }

    pub(crate) fn u64(&self, name: &str) -> Result<&'a UInt64Array, CatalogError> {
        self.get(name, "UInt64")
    }

    pub(crate) fn i64(&self, name: &str) -> Result<&'a Int64Array, CatalogError> {
        self.get(name, "Int64")
    }

    /// Like [`Columns::get`], but a column that may hold nulls.
    pub(crate) fn nullable<T: Array + 'static>(&self, name: &str, type_name: &str) -> Result<&'a T, CatalogError> {
        let column = self.batch.column_by_name(name).ok_or_else(|| CatalogError::Schema {
            path: self.path.to_path_buf(),
            detail: format!("missing column {name:?}"),
        })?;
        column.as_any().downcast_ref::<T>().ok_or_else(|| CatalogError::Schema {
            path: self.path.to_path_buf(),
            detail: format!("column {name:?} is {}, expected {type_name}", column.data_type()),
        })
    }
}

/// Every row of every `hashes-*.parquet` in `dir`, in file-name order.
///
/// A missing `dir` reads as no rows.
///
/// # Errors
///
/// [`CatalogError::Schema`] if a part file lacks a column or holds a null,
/// or an I/O or decoder failure.
pub fn read_hashes(dir: &Path) -> Result<Vec<HashRow>, CatalogError> {
    let mut rows = Vec::new();
    for part in list_parts(dir, HASHES)? {
        for batch in read_batches(&part)? {
            let c = Columns {
                batch: &batch,
                path: &part,
            };
            let (path, dev, ino, size, mtime, sha) = (
                c.utf8("path")?,
                c.u64("dev")?,
                c.u64("ino")?,
                c.u64("size")?,
                c.i64("mtime_ns")?,
                c.utf8("sha256")?,
            );
            rows.extend((0..batch.num_rows()).map(|i| HashRow {
                path: path.value(i).to_owned(),
                dev: dev.value(i),
                ino: ino.value(i),
                size: size.value(i),
                mtime_ns: mtime.value(i),
                sha256: sha.value(i).to_owned(),
            }));
        }
    }
    Ok(rows)
}

/// Every row of every `receipts-*.parquet` in `dir`, in file-name order.
///
/// A missing `dir` reads as no rows.
///
/// # Errors
///
/// [`CatalogError::Schema`] if a part file lacks a column or holds a null,
/// or an I/O or decoder failure.
pub fn read_receipts(dir: &Path) -> Result<Vec<ReceiptRow>, CatalogError> {
    let mut rows = Vec::new();
    for part in list_parts(dir, RECEIPTS)? {
        for batch in read_batches(&part)? {
            let c = Columns {
                batch: &batch,
                path: &part,
            };
            let run_id = c.utf8("run_id")?;
            let source_label = c.utf8("source_label")?;
            let source_path = c.utf8("source_path")?;
            let source_dev = c.u64("source_dev")?;
            let source_ino = c.u64("source_ino")?;
            let size = c.u64("size")?;
            let mtime = c.i64("mtime_ns")?;
            let dest_path = c.utf8("dest_path")?;
            let method = c.utf8("method")?;
            let sha = c.utf8("sha256")?;
            let verified = c.i64("verified_at_ms")?;
            rows.extend((0..batch.num_rows()).map(|i| ReceiptRow {
                run_id: run_id.value(i).to_owned(),
                source_label: source_label.value(i).to_owned(),
                source_path: source_path.value(i).to_owned(),
                source_dev: source_dev.value(i),
                source_ino: source_ino.value(i),
                size: size.value(i),
                mtime_ns: mtime.value(i),
                dest_path: dest_path.value(i).to_owned(),
                method: method.value(i).to_owned(),
                sha256: sha.value(i).to_owned(),
                verified_at_ms: verified.value(i),
            }));
        }
    }
    Ok(rows)
}
