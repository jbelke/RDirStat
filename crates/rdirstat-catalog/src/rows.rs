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
/// Part-file kind for the files a directory roll-up landed.
pub(crate) const MEMBERS: &str = "members";
/// [`ReceiptRow::method`] of a directory roll-up's one receipt.
pub const METHOD_ROLLUP: &str = "rollup";

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
    /// already landed file with the same size and SHA-256), or, on a
    /// receipt only, `"rollup"` ([`METHOD_ROLLUP`]): a directory roll-up,
    /// whose files are listed one row each in the `members` parts.
    pub method: String,
    /// SHA-256 of the content, 64 lowercase hex digits. For a `rollup`
    /// receipt, the SHA-256 of the scope's manifest at landing (each entry's
    /// relative path, kind, size, and mtime, sorted by path); the content
    /// proofs are the `members` rows.
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
/// `"copy"`, `"clone"`, or `"rollup"` (checked before anything is written),
/// [`CatalogError::InvalidRunId`], or an I/O or encoder failure.
pub fn write_receipts(rows: &[ReceiptRow], out_dir: &Path, run_id: &str) -> Result<std::path::PathBuf, CatalogError> {
    write_receipt_rows(rows, out_dir, run_id, RECEIPTS, &["copy", "clone", METHOD_ROLLUP])
}

/// Writes `rows` as a new `members-<run_id>-<seq>.parquet` in `out_dir`:
/// one row per file a directory roll-up landed, with the receipt schema.
/// Each row is a proof for that one file: it is written only after the file
/// at `dest_path` compared equal to `source_path`.
///
/// # Errors
///
/// As [`write_receipts`]; a member's `method` must be `"copy"`.
pub fn write_members(rows: &[ReceiptRow], out_dir: &Path, run_id: &str) -> Result<std::path::PathBuf, CatalogError> {
    write_receipt_rows(rows, out_dir, run_id, MEMBERS, &["copy"])
}

fn write_receipt_rows(
    rows: &[ReceiptRow],
    out_dir: &Path,
    run_id: &str,
    kind: &str,
    methods: &[&str],
) -> Result<std::path::PathBuf, CatalogError> {
    for (index, row) in rows.iter().enumerate() {
        check_sha(index, &row.sha256)?;
        if !methods.contains(&row.method.as_str()) {
            return Err(CatalogError::InvalidRow {
                index,
                detail: format!("method {:?} is not one of {methods:?}", row.method),
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
    write_part(out_dir, kind, run_id, &RECEIPT_SCHEMA, |writer| {
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
    visit_receipt_rows(dir, RECEIPTS, |row| rows.push(row.to_owned_row()))?;
    Ok(rows)
}

/// Calls `visit` for every row of every `members-*.parquet` in `dir`, one
/// batch at a time, without collecting them: a roll-up of a large tree
/// writes one member row per file.
///
/// A missing `dir` visits nothing.
///
/// # Errors
///
/// As [`read_receipts`].
pub fn visit_members(dir: &Path, visit: impl FnMut(ReceiptView<'_>)) -> Result<(), CatalogError> {
    visit_receipt_rows(dir, MEMBERS, visit)
}

/// A borrowed [`ReceiptRow`], as [`visit_members`] hands it out.
#[derive(Clone, Copy, Debug)]
pub struct ReceiptView<'a> {
    /// [`ReceiptRow::run_id`].
    pub run_id: &'a str,
    /// [`ReceiptRow::source_label`].
    pub source_label: &'a str,
    /// [`ReceiptRow::source_path`].
    pub source_path: &'a str,
    /// [`ReceiptRow::source_dev`].
    pub source_dev: u64,
    /// [`ReceiptRow::source_ino`].
    pub source_ino: u64,
    /// [`ReceiptRow::size`].
    pub size: u64,
    /// [`ReceiptRow::mtime_ns`].
    pub mtime_ns: i64,
    /// [`ReceiptRow::dest_path`].
    pub dest_path: &'a str,
    /// [`ReceiptRow::method`].
    pub method: &'a str,
    /// [`ReceiptRow::sha256`].
    pub sha256: &'a str,
    /// [`ReceiptRow::verified_at_ms`].
    pub verified_at_ms: i64,
}

impl ReceiptView<'_> {
    /// The owned row.
    #[must_use]
    pub fn to_owned_row(&self) -> ReceiptRow {
        ReceiptRow {
            run_id: self.run_id.to_owned(),
            source_label: self.source_label.to_owned(),
            source_path: self.source_path.to_owned(),
            source_dev: self.source_dev,
            source_ino: self.source_ino,
            size: self.size,
            mtime_ns: self.mtime_ns,
            dest_path: self.dest_path.to_owned(),
            method: self.method.to_owned(),
            sha256: self.sha256.to_owned(),
            verified_at_ms: self.verified_at_ms,
        }
    }
}

fn visit_receipt_rows(dir: &Path, kind: &str, mut visit: impl FnMut(ReceiptView<'_>)) -> Result<(), CatalogError> {
    for part in list_parts(dir, kind)? {
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
            for i in 0..batch.num_rows() {
                visit(ReceiptView {
                    run_id: run_id.value(i),
                    source_label: source_label.value(i),
                    source_path: source_path.value(i),
                    source_dev: source_dev.value(i),
                    source_ino: source_ino.value(i),
                    size: size.value(i),
                    mtime_ns: mtime.value(i),
                    dest_path: dest_path.value(i),
                    method: method.value(i),
                    sha256: sha.value(i),
                    verified_at_ms: verified.value(i),
                });
            }
        }
    }
    Ok(())
}
