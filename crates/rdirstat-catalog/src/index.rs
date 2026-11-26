//! The drive's own index: `<dest>/index.parquet` and `<dest>/index.sql`
//! (docs/11-ORGANIZE.md, Amendment B).
//!
//! The truth is the append-only part files in `<dest>/.rdirstat/catalog/`.
//! `index.parquet` is derived from them — one row per landed destination
//! path — and rebuilt whole on every run: written to `index.parquet.tmp`,
//! fsynced, renamed over the old one, directory fsynced. A reader sees the
//! old index or the new one, never half of one. `index.sql` holds DuckDB
//! views over the index and the parts, so `duckdb -init <dest>/index.sql`
//! opens a ready shell.
//!
//! Nothing here walks the drive: every number comes from Parquet.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use arrow::array::{Array, StringArray, StringBuilder, UInt8Array, UInt8Builder, UInt64Array};
use arrow::array::{Int64Array, Int64Builder, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};

use crate::catalog::dest_catalog_dir;
use crate::entries::{ENTRIES, ENTRY_SCHEMA, extension};
use crate::plans::{PLAN_SCHEMA, PLANS};
use crate::rows::{Columns, MEMBERS, METHOD_ROLLUP, RECEIPT_SCHEMA, ReceiptRow, read_receipts, visit_members};
use crate::store::{list_parts, part_seq, read_batches};
use crate::{CatalogError, ENTRIES_BATCH_ROWS};

/// The index file at the drive root.
pub const INDEX_FILE: &str = "index.parquet";
/// The DuckDB init script at the drive root.
pub const INDEX_SQL_FILE: &str = "index.sql";

/// One landed destination path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexRow {
    /// Absolute destination path.
    pub dest_path: String,
    /// Logical bytes.
    pub size: u64,
    /// SHA-256 of the content (a tree digest for a package).
    pub sha256: String,
    /// `copy` or `clone`.
    pub method: String,
    /// The source label it was backed up from.
    pub source_label: String,
    /// Absolute source path.
    pub source_path: String,
    /// Source mtime at copy time, nanoseconds since the Unix epoch.
    pub source_mtime_ns: i64,
    /// The backup run whose receipt this is.
    pub run_id: String,
    /// When the copy verified, milliseconds since the Unix epoch.
    pub verified_at_ms: i64,
    /// `rdirstat-classify` category from the source's scan entries, when the
    /// catalog holds an entry for the source path.
    pub category: Option<u8>,
    /// Lowercase extension of the destination file name.
    pub ext: Option<String>,
    /// How many distinct source paths on this drive share `sha256`.
    pub dup_count: u64,
}

static INDEX_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("dest_path", DataType::Utf8, false),
        Field::new("size", DataType::UInt64, false),
        Field::new("sha256", DataType::Utf8, false),
        Field::new("method", DataType::Utf8, false),
        Field::new("source_label", DataType::Utf8, false),
        Field::new("source_path", DataType::Utf8, false),
        Field::new("source_mtime_ns", DataType::Int64, false),
        Field::new("run_id", DataType::Utf8, false),
        Field::new("verified_at_ms", DataType::Int64, false),
        Field::new("category", DataType::UInt8, true),
        Field::new("ext", DataType::Utf8, true),
        Field::new("dup_count", DataType::UInt64, false),
    ]))
});

/// One scan entry, as much of it as the index and status need.
#[derive(Clone, Copy, Debug)]
pub struct EntryView<'a> {
    /// Absolute source path (lossy UTF-8).
    pub path: &'a str,
    /// The scan's source label.
    pub source_label: &'a str,
    /// `file`, `dir`, `symlink`, or `other`.
    pub kind: &'a str,
    /// The node's own allocated bytes.
    pub allocated: u64,
    /// `rdirstat-classify` category index.
    pub category: u8,
}

/// Calls `visit` for every row of the `entries-*.parquet` parts in `dir`,
/// or only those of run `run_id` when given, reading only the columns
/// [`EntryView`] holds, one batch at a time.
///
/// A missing `dir` visits nothing.
///
/// # Errors
///
/// [`CatalogError::Schema`] if a part lacks a column, or an I/O or decoder
/// failure.
pub fn visit_entries(
    dir: &Path,
    run_id: Option<&str>,
    mut visit: impl FnMut(EntryView<'_>),
) -> Result<(), CatalogError> {
    const COLUMNS: [&str; 5] = ["path", "source_label", "kind", "allocated", "category"];
    for part in list_parts(dir, ENTRIES)? {
        if let Some(run_id) = run_id {
            let name = part.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if part_seq(name, ENTRIES, run_id).is_none() {
                continue;
            }
        }
        let file = File::open(&part).map_err(|source| CatalogError::Io {
            operation: "open",
            path: part.clone(),
            source,
        })?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let mask = ProjectionMask::columns(builder.parquet_schema(), COLUMNS);
        let reader = builder
            .with_projection(mask)
            .with_batch_size(ENTRIES_BATCH_ROWS)
            .build()?;
        for batch in reader {
            let batch = batch?;
            let c = Columns::new(&batch, &part);
            let path = c.utf8("path")?;
            let label = c.utf8("source_label")?;
            let kind = c.utf8("kind")?;
            let allocated = c.u64("allocated")?;
            let category = c.nullable::<UInt8Array>("category", "UInt8")?;
            for i in 0..batch.num_rows() {
                visit(EntryView {
                    path: path.value(i),
                    source_label: label.value(i),
                    kind: kind.value(i),
                    allocated: allocated.value(i),
                    category: category.value(i),
                });
            }
        }
    }
    Ok(())
}

/// The index rows the parts in `catalog_dir` describe, sorted by
/// `dest_path`.
///
/// One row per destination file: every `copy`/`clone` receipt, and every
/// `members` row a directory roll-up wrote. A `rollup` receipt names a
/// directory, not a file, so it is not a row itself; its files are. When two
/// rows name the same path (a re-landed copy), the later `verified_at_ms`
/// wins. `dup_count` counts the
/// distinct `(source_label, source_path)` pairs per `sha256`. `category` is
/// looked up in the scan entries of the receipt's source label.
///
/// # Errors
///
/// A part that cannot be read.
pub fn build_index(catalog_dir: &Path) -> Result<Vec<IndexRow>, CatalogError> {
    // Keyed by a 128-bit digest of the path, not a second copy of it: with a
    // member row per roll-up file, this map holds every file on the drive.
    let mut by_dest: HashMap<u128, ReceiptRow> = HashMap::new();
    let mut keep = |receipt: ReceiptRow| match by_dest.entry(key128(&[receipt.dest_path.as_bytes()])) {
        std::collections::hash_map::Entry::Occupied(mut kept) => {
            if kept.get().verified_at_ms <= receipt.verified_at_ms {
                kept.insert(receipt);
            }
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(receipt);
        }
    };
    for receipt in read_receipts(catalog_dir)? {
        if receipt.method != METHOD_ROLLUP {
            keep(receipt);
        }
    }
    visit_members(catalog_dir, |member| keep(member.to_owned_row()))?;

    let source_key = |label: &str, path: &str| key128(&[label.as_bytes(), path.as_bytes()]);
    let mut sources: HashMap<u128, HashSet<u128>> = HashMap::new();
    for receipt in by_dest.values() {
        sources
            .entry(key128(&[receipt.sha256.as_bytes()]))
            .or_default()
            .insert(source_key(&receipt.source_label, &receipt.source_path));
    }
    let dup_counts: HashMap<u128, u64> = sources.into_iter().map(|(sha, set)| (sha, set.len() as u64)).collect();

    let wanted: HashSet<u128> = by_dest
        .values()
        .map(|r| source_key(&r.source_label, &r.source_path))
        .collect();
    let mut categories: HashMap<u128, u8> = HashMap::new();
    visit_entries(catalog_dir, None, |entry| {
        let key = source_key(entry.source_label, entry.path);
        if wanted.contains(&key) {
            categories.insert(key, entry.category);
        }
    })?;
    drop(wanted);

    let mut rows: Vec<IndexRow> = by_dest
        .into_values()
        .map(|r| {
            let name = Path::new(&r.dest_path)
                .file_name()
                .map(|n| n.as_encoded_bytes().to_vec())
                .unwrap_or_default();
            let category = categories.get(&source_key(&r.source_label, &r.source_path)).copied();
            let dup_count = dup_counts.get(&key128(&[r.sha256.as_bytes()])).copied().unwrap_or(1);
            IndexRow {
                dest_path: r.dest_path,
                size: r.size,
                sha256: r.sha256,
                method: r.method,
                source_label: r.source_label,
                source_path: r.source_path,
                source_mtime_ns: r.mtime_ns,
                run_id: r.run_id,
                verified_at_ms: r.verified_at_ms,
                category,
                ext: extension(&name),
                dup_count,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.dest_path.cmp(&b.dest_path));
    Ok(rows)
}

/// A 128-bit digest of length-prefixed parts: a compact map key for the tens
/// of millions of paths a drive can hold. Collisions are not a concern at
/// this width.
fn key128(parts: &[&[u8]]) -> u128 {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    u128::from_le_bytes(out)
}

fn io(operation: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> CatalogError {
    let path = path.to_path_buf();
    move |source| CatalogError::Io {
        operation,
        path,
        source,
    }
}

/// Replaces `<dir>/<name>` through `<name>.tmp`: create (mode `0600`),
/// `fill`, fsync, rename, fsync the directory.
fn replace_file(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(File) -> Result<File, CatalogError>,
) -> Result<PathBuf, CatalogError> {
    let target = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    // A temp left by a crashed rebuild is ours to replace: nothing reads it.
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io("remove", &temp)(error)),
    }
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(io("create", &temp))?;
    let result = fill(created).and_then(|file| file.sync_all().map_err(io("fsync", &temp)));
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, &target).map_err(io("rename", &target))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io("fsync directory", dir))?;
    Ok(target)
}

/// Writes `rows` as `<dest_mount>/index.parquet`, replacing the old index
/// atomically.
///
/// # Errors
///
/// An I/O or encoder failure; the old index, if any, is left as it was.
pub fn write_index(dest_mount: &Path, rows: &[IndexRow]) -> Result<PathBuf, CatalogError> {
    replace_file(dest_mount, INDEX_FILE, |file| {
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
            .set_created_by(format!("rdirstat-catalog {}", env!("CARGO_PKG_VERSION")))
            .build();
        let mut writer = ArrowWriter::try_new(file, Arc::clone(&INDEX_SCHEMA), Some(props))?;
        for chunk in rows.chunks(ENTRIES_BATCH_ROWS) {
            writer.write(&index_batch(chunk)?)?;
        }
        Ok(writer.into_inner()?)
    })
}

fn index_batch(rows: &[IndexRow]) -> Result<RecordBatch, CatalogError> {
    let n = rows.len();
    let mut dest_path = StringBuilder::with_capacity(n, n * 64);
    let mut size = UInt64Builder::with_capacity(n);
    let mut sha256 = StringBuilder::with_capacity(n, n * 64);
    let mut method = StringBuilder::with_capacity(n, n * 5);
    let mut source_label = StringBuilder::with_capacity(n, n * 8);
    let mut source_path = StringBuilder::with_capacity(n, n * 64);
    let mut mtime = Int64Builder::with_capacity(n);
    let mut run_id = StringBuilder::with_capacity(n, n * 24);
    let mut verified = Int64Builder::with_capacity(n);
    let mut category = UInt8Builder::with_capacity(n);
    let mut ext = StringBuilder::with_capacity(n, n * 4);
    let mut dup_count = UInt64Builder::with_capacity(n);
    for row in rows {
        dest_path.append_value(&row.dest_path);
        size.append_value(row.size);
        sha256.append_value(&row.sha256);
        method.append_value(&row.method);
        source_label.append_value(&row.source_label);
        source_path.append_value(&row.source_path);
        mtime.append_value(row.source_mtime_ns);
        run_id.append_value(&row.run_id);
        verified.append_value(row.verified_at_ms);
        category.append_option(row.category);
        ext.append_option(row.ext.as_deref());
        dup_count.append_value(row.dup_count);
    }
    Ok(RecordBatch::try_new(
        Arc::clone(&INDEX_SCHEMA),
        vec![
            Arc::new(dest_path.finish()),
            Arc::new(size.finish()),
            Arc::new(sha256.finish()),
            Arc::new(method.finish()),
            Arc::new(source_label.finish()),
            Arc::new(source_path.finish()),
            Arc::new(mtime.finish()),
            Arc::new(run_id.finish()),
            Arc::new(verified.finish()),
            Arc::new(category.finish()),
            Arc::new(ext.finish()),
            Arc::new(dup_count.finish()),
        ],
    )?)
}

/// Every row of an `index.parquet`.
///
/// # Errors
///
/// [`CatalogError::Schema`] if the file lacks a column, or an I/O or decoder
/// failure.
pub fn read_index(path: &Path) -> Result<Vec<IndexRow>, CatalogError> {
    let mut rows = Vec::new();
    for batch in read_batches(path)? {
        let c = Columns::new(&batch, path);
        let dest_path = c.utf8("dest_path")?;
        let size = c.u64("size")?;
        let sha256 = c.utf8("sha256")?;
        let method = c.utf8("method")?;
        let source_label = c.utf8("source_label")?;
        let source_path = c.utf8("source_path")?;
        let mtime: &Int64Array = c.i64("source_mtime_ns")?;
        let run_id = c.utf8("run_id")?;
        let verified = c.i64("verified_at_ms")?;
        let category = c.nullable::<UInt8Array>("category", "UInt8")?;
        let ext = c.nullable::<StringArray>("ext", "Utf8")?;
        let dup_count: &UInt64Array = c.u64("dup_count")?;
        rows.extend((0..batch.num_rows()).map(|i| IndexRow {
            dest_path: dest_path.value(i).to_owned(),
            size: size.value(i),
            sha256: sha256.value(i).to_owned(),
            method: method.value(i).to_owned(),
            source_label: source_label.value(i).to_owned(),
            source_path: source_path.value(i).to_owned(),
            source_mtime_ns: mtime.value(i),
            run_id: run_id.value(i).to_owned(),
            verified_at_ms: verified.value(i),
            category: category.is_valid(i).then(|| category.value(i)),
            ext: ext.is_valid(i).then(|| ext.value(i).to_owned()),
            dup_count: dup_count.value(i),
        }));
    }
    Ok(rows)
}

/// A SQL string literal.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn duckdb_type(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Binary => "BLOB",
        DataType::Boolean => "BOOLEAN",
        DataType::UInt8 => "UTINYINT",
        DataType::UInt16 => "USMALLINT",
        DataType::UInt32 => "UINTEGER",
        DataType::UInt64 => "UBIGINT",
        DataType::Int64 => "BIGINT",
        // Utf8, and anything this build does not write.
        _ => "VARCHAR",
    }
}

/// A view over every part of `kind`, or, when the catalog has none yet, an
/// empty view with the same columns (DuckDB refuses a glob that matches no
/// file, and a broken init script would hide every other view).
fn parts_view(name: &str, catalog_dir: &Path, kind: &str, schema: &Schema) -> Result<String, CatalogError> {
    if list_parts(catalog_dir, kind)?.is_empty() {
        let columns: Vec<String> = schema
            .fields()
            .iter()
            .map(|f| format!("NULL::{} AS \"{}\"", duckdb_type(f.data_type()), f.name()))
            .collect();
        return Ok(format!(
            "-- No {kind} parts yet; run `rdirstat backup index` again after the next run.\n\
             CREATE OR REPLACE VIEW {name} AS SELECT {} WHERE false;\n",
            columns.join(", ")
        ));
    }
    let glob = catalog_dir.join(format!("{kind}-*.parquet"));
    Ok(format!(
        "CREATE OR REPLACE VIEW {name} AS SELECT * FROM read_parquet({}, union_by_name = true);\n",
        quote(&glob.to_string_lossy())
    ))
}

/// The DuckDB init script for `dest_mount`: views `files` (the index),
/// `receipts`, `members` (the files of directory roll-ups), `entries`, `plans`, `skipped`, and `dupes`. Paths are
/// absolute, so the script works from any directory; it is regenerated on
/// every rebuild, so a drive mounted elsewhere is fixed by running
/// `rdirstat backup index` again.
///
/// # Errors
///
/// An unreadable catalog directory.
pub fn index_sql(dest_mount: &Path) -> Result<String, CatalogError> {
    let catalog = dest_catalog_dir(dest_mount);
    let mut sql = String::new();
    let _ = writeln!(
        sql,
        "-- Generated by `rdirstat backup index` for {}. Rebuilt on every run; do not edit.\n\
         -- Open with: duckdb -init {}\n\
         -- The truth is the append-only parts in {}; index.parquet is derived from them.",
        dest_mount.display(),
        dest_mount.join(INDEX_SQL_FILE).display(),
        catalog.display()
    );
    let _ = writeln!(
        sql,
        "CREATE OR REPLACE VIEW files AS SELECT * FROM read_parquet({});",
        quote(&dest_mount.join(INDEX_FILE).to_string_lossy())
    );
    sql.push_str(&parts_view("receipts", &catalog, "receipts", &RECEIPT_SCHEMA)?);
    sql.push_str(&parts_view("members", &catalog, MEMBERS, &RECEIPT_SCHEMA)?);
    sql.push_str(&parts_view("entries", &catalog, ENTRIES, &ENTRY_SCHEMA)?);
    sql.push_str(&parts_view("plans", &catalog, PLANS, &PLAN_SCHEMA)?);
    sql.push_str(
        "CREATE OR REPLACE VIEW skipped AS\n  \
         SELECT plan_id, source_label, relative, source_path, rule_id, reason, allocated, created_at_ms\n  \
         FROM plans WHERE status = 'skipped';\n\
         CREATE OR REPLACE VIEW dupes AS\n  \
         SELECT sha256, size, count(*) AS copies, count(*) FILTER (WHERE method = 'clone') AS clones,\n         \
         (count(*) - 1) * size AS logical_saved_by_one_copy, list(dest_path ORDER BY dest_path) AS dest_paths\n  \
         FROM files GROUP BY sha256, size HAVING count(*) > 1;\n",
    );
    Ok(sql)
}

/// What [`rebuild_index`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSummary {
    /// `<dest>/index.parquet`.
    pub index: PathBuf,
    /// `<dest>/index.sql`.
    pub sql: PathBuf,
    /// Rows in the index.
    pub rows: usize,
    /// Sum of `size` over the index.
    pub bytes: u64,
}

/// Rebuilds `<dest_mount>/index.parquet` and `<dest_mount>/index.sql` from
/// the parts in `<dest_mount>/.rdirstat/catalog`.
///
/// # Errors
///
/// A missing catalog directory (nothing has been backed up to this drive),
/// an unreadable part, or a failed write.
pub fn rebuild_index(dest_mount: &Path) -> Result<IndexSummary, CatalogError> {
    let catalog = dest_catalog_dir(dest_mount);
    if !catalog.is_dir() {
        return Err(CatalogError::Io {
            operation: "open catalog",
            path: catalog,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no catalog on this drive"),
        });
    }
    let rows = build_index(&catalog)?;
    let index = write_index(dest_mount, &rows)?;
    let script = index_sql(dest_mount)?;
    let sql = replace_file(dest_mount, INDEX_SQL_FILE, |mut file| {
        file.write_all(script.as_bytes())
            .map_err(io("write", &dest_mount.join(INDEX_SQL_FILE)))?;
        Ok(file)
    })?;
    Ok(IndexSummary {
        index,
        sql,
        rows: rows.len(),
        bytes: rows.iter().map(|r| r.size).sum(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_escape_single_quotes() {
        assert_eq!(quote("/Volumes/a'b"), "'/Volumes/a''b'");
    }

    #[test]
    fn an_empty_kind_gets_a_typed_empty_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let view = parts_view("plans", dir.path(), PLANS, &PLAN_SCHEMA).expect("view");
        assert!(view.contains("WHERE false"), "{view}");
        assert!(view.contains("NULL::UBIGINT AS \"allocated\""), "{view}");
    }
}
