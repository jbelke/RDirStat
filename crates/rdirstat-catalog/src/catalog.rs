//! [`Catalog`]: one primary directory plus an optional mirror, and the
//! per-run `manifest` that inventories the parts a run wrote.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use arrow::array::{Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::file::reader::{FileReader, SerializedFileReader};
use rdirstat_core::CompletedScan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::plans::{PlanRow, write_plan_rows};
use crate::rows::{Columns, HashRow, ReceiptRow, write_hashes, write_members, write_receipts};
use crate::store::{list_parts, mirror_part, part_seq, read_batches, validate_run_id, write_part};
use crate::{CatalogError, write_entries};

/// Part-file kind for run manifests.
const MANIFEST: &str = "manifest";

/// Part kinds a manifest inventories, in the order it lists them.
const INVENTORIED: [&str; 5] = ["entries", "hashes", "members", "plans", "receipts"];

/// Where a destination drive keeps its own copy of the catalog, relative to
/// its mount point (docs/11-ORGANIZE.md, Amendment A).
pub const DEST_CATALOG_DIR: &str = ".rdirstat/catalog";

/// `<dest_mount>/.rdirstat/catalog`.
#[must_use]
pub fn dest_catalog_dir(dest_mount: &Path) -> PathBuf {
    dest_mount.join(DEST_CATALOG_DIR)
}

/// One part file a run published, as recorded in its manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ManifestRow {
    /// The run that wrote the part.
    pub run_id: String,
    /// `entries`, `hashes`, `members`, `plans`, or `receipts`.
    pub kind: String,
    /// The part's file name, e.g. `hashes-run1-0000.parquet`.
    pub file_name: String,
    /// Rows in the part, from its Parquet footer.
    pub rows: u64,
    /// The part's size in bytes.
    pub bytes: u64,
    /// SHA-256 of the part file's bytes, 64 lowercase hex digits.
    pub sha256: String,
    /// When the manifest was written, milliseconds since the Unix epoch.
    pub recorded_at_ms: i64,
}

static MANIFEST_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("file_name", DataType::Utf8, false),
        Field::new("rows", DataType::UInt64, false),
        Field::new("bytes", DataType::UInt64, false),
        Field::new("sha256", DataType::Utf8, false),
        Field::new("recorded_at_ms", DataType::Int64, false),
    ]))
});

/// Where a part landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The part in the primary (app data) catalog.
    pub primary: PathBuf,
    /// The same bytes in the mirror, when the catalog has one.
    pub mirror: Option<PathBuf>,
}

/// A catalog written to a primary directory (the app data dir) and, when
/// given, mirrored file-for-file to a second one (normally
/// [`dest_catalog_dir`] on the backup drive) so the drive describes itself.
///
/// Every write publishes to the primary first, then copies the published
/// bytes to the mirror under the same file name. A mirror failure leaves the
/// primary part in place and reports [`CatalogError::Mirror`];
/// [`Catalog::sync_mirror`] copies whatever of a run the mirror is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    primary: PathBuf,
    mirror: Option<PathBuf>,
}

impl Catalog {
    /// A catalog with no mirror.
    #[must_use]
    pub fn new(primary: impl Into<PathBuf>) -> Self {
        Self {
            primary: primary.into(),
            mirror: None,
        }
    }

    /// A catalog in `primary` mirrored to `<dest_mount>/.rdirstat/catalog`.
    #[must_use]
    pub fn for_destination(primary: impl Into<PathBuf>, dest_mount: &Path) -> Self {
        Self {
            primary: primary.into(),
            mirror: Some(dest_catalog_dir(dest_mount)),
        }
    }

    /// A catalog in `primary` mirrored to the directory `mirror`.
    #[must_use]
    pub fn with_mirror(primary: impl Into<PathBuf>, mirror: impl Into<PathBuf>) -> Self {
        Self {
            primary: primary.into(),
            mirror: Some(mirror.into()),
        }
    }

    /// The primary directory.
    #[must_use]
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    /// The mirror directory, if any.
    #[must_use]
    pub fn mirror(&self) -> Option<&Path> {
        self.mirror.as_deref()
    }

    fn mirrored(&self, primary: PathBuf) -> Result<Written, CatalogError> {
        let Some(dir) = &self.mirror else {
            return Ok(Written { primary, mirror: None });
        };
        match mirror_part(&primary, dir) {
            Ok(mirror) => Ok(Written {
                primary,
                mirror: Some(mirror),
            }),
            Err(source) => Err(CatalogError::Mirror {
                primary,
                source: Box::new(source),
            }),
        }
    }

    /// [`write_hashes`] to the primary, then mirror.
    ///
    /// # Errors
    ///
    /// As [`write_hashes`], or [`CatalogError::Mirror`].
    pub fn write_hashes(&self, rows: &[HashRow], run_id: &str) -> Result<Written, CatalogError> {
        self.mirrored(write_hashes(rows, &self.primary, run_id)?)
    }

    /// [`write_members`] to the primary, then mirror.
    ///
    /// # Errors
    ///
    /// As [`write_members`], or [`CatalogError::Mirror`].
    pub fn write_members(&self, rows: &[ReceiptRow], run_id: &str) -> Result<Written, CatalogError> {
        self.mirrored(write_members(rows, &self.primary, run_id)?)
    }

    /// [`write_receipts`] to the primary, then mirror.
    ///
    /// # Errors
    ///
    /// As [`write_receipts`], or [`CatalogError::Mirror`].
    pub fn write_receipts(&self, rows: &[ReceiptRow], run_id: &str) -> Result<Written, CatalogError> {
        self.mirrored(write_receipts(rows, &self.primary, run_id)?)
    }

    /// [`write_plan_rows`] to the primary, then mirror.
    ///
    /// # Errors
    ///
    /// As [`write_plan_rows`], or [`CatalogError::Mirror`].
    pub fn write_plan_rows(&self, rows: &[PlanRow], run_id: &str) -> Result<Written, CatalogError> {
        self.mirrored(write_plan_rows(rows, &self.primary, run_id)?)
    }

    /// [`write_entries`] to the primary, then mirror.
    ///
    /// # Errors
    ///
    /// As [`write_entries`], or [`CatalogError::Mirror`].
    pub fn write_entries(
        &self,
        scan: &CompletedScan,
        source_label: &str,
        run_id: &str,
    ) -> Result<Written, CatalogError> {
        self.mirrored(write_entries(scan, source_label, &self.primary, run_id)?)
    }

    /// Writes `manifest-<run_id>-<seq>.parquet`: one row per `entries`,
    /// `hashes`, `members`, `plans`, and `receipts` part of `run_id` in the primary, with its row
    /// count, size, and SHA-256, then mirrors it. Call it once a run has
    /// written its last part; a mirror can then be checked against it.
    ///
    /// # Errors
    ///
    /// [`CatalogError::InvalidRunId`], an I/O or decoder failure reading a
    /// part, or [`CatalogError::Mirror`].
    pub fn write_manifest(&self, run_id: &str, recorded_at_ms: i64) -> Result<Written, CatalogError> {
        validate_run_id(run_id)?;
        let mut rows = Vec::new();
        for kind in INVENTORIED {
            for part in list_parts(&self.primary, kind)? {
                let Some(file_name) = part.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if part_seq(file_name, kind, run_id).is_none() {
                    continue;
                }
                let (bytes, sha256) = hash_file(&part)?;
                let reader = SerializedFileReader::new(open(&part)?)?;
                let count = reader.metadata().file_metadata().num_rows();
                rows.push(ManifestRow {
                    run_id: run_id.to_owned(),
                    kind: kind.to_owned(),
                    file_name: file_name.to_owned(),
                    rows: u64::try_from(count).unwrap_or(0),
                    bytes,
                    sha256,
                    recorded_at_ms,
                });
            }
        }
        let batch = RecordBatch::try_new(
            Arc::clone(&MANIFEST_SCHEMA),
            vec![
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.run_id.as_str()))),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.kind.as_str()))),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.file_name.as_str()))),
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.rows))),
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.bytes))),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.sha256.as_str()))),
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.recorded_at_ms))),
            ],
        )?;
        let primary = write_part(&self.primary, MANIFEST, run_id, &MANIFEST_SCHEMA, |writer| {
            Ok(writer.write(&batch)?)
        })?;
        self.mirrored(primary)
    }

    /// Copies every primary part *of `run_id`* the mirror lacks; returns the
    /// mirror paths it created. A catalog without a mirror returns nothing.
    ///
    /// Only the run's own parts travel. The primary holds every source's and
    /// every destination's rows; copying all of it would put other drives'
    /// file names and hashes on this one, and a conflict on some unrelated
    /// old part would fail every later run.
    ///
    /// # Errors
    ///
    /// [`CatalogError::InvalidRunId`], [`CatalogError::MirrorConflict`] if a
    /// mirror part differs from the primary part of the same name, or an I/O
    /// failure.
    pub fn sync_mirror(&self, run_id: &str) -> Result<Vec<PathBuf>, CatalogError> {
        validate_run_id(run_id)?;
        let Some(dir) = &self.mirror else {
            return Ok(Vec::new());
        };
        let mut created = Vec::new();
        for kind in INVENTORIED.iter().chain(std::iter::once(&MANIFEST)) {
            for part in list_parts(&self.primary, kind)? {
                let Some(name) = part.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if part_seq(name, kind, run_id).is_none() {
                    continue;
                }
                let existed = dir.join(name).exists();
                let target = mirror_part(&part, dir)?;
                if !existed {
                    created.push(target);
                }
            }
        }
        Ok(created)
    }
}

fn open(path: &Path) -> Result<File, CatalogError> {
    File::open(path).map_err(|source| CatalogError::Io {
        operation: "open",
        path: path.to_path_buf(),
        source,
    })
}

/// `(size, sha256 hex)` of a file's bytes.
fn hash_file(path: &Path) -> Result<(u64, String), CatalogError> {
    let mut file = open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1 << 16];
    let mut total = 0_u64;
    loop {
        let n = file.read(&mut buf).map_err(|source| CatalogError::Io {
            operation: "read",
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    let digest = hasher.finalize();
    let hex = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    Ok((total, hex))
}

/// Every row of every `manifest-*.parquet` in `dir`, in file-name order.
///
/// A missing `dir` reads as no rows.
///
/// # Errors
///
/// [`CatalogError::Schema`] if a part file lacks a column or holds a null,
/// or an I/O or decoder failure.
pub fn read_manifests(dir: &Path) -> Result<Vec<ManifestRow>, CatalogError> {
    let mut rows = Vec::new();
    for part in list_parts(dir, MANIFEST)? {
        for batch in read_batches(&part)? {
            let c = Columns::new(&batch, &part);
            let run_id = c.utf8("run_id")?;
            let kind = c.utf8("kind")?;
            let file_name = c.utf8("file_name")?;
            let count = c.u64("rows")?;
            let bytes = c.u64("bytes")?;
            let sha = c.utf8("sha256")?;
            let recorded = c.i64("recorded_at_ms")?;
            rows.extend((0..batch.num_rows()).map(|i| ManifestRow {
                run_id: run_id.value(i).to_owned(),
                kind: kind.value(i).to_owned(),
                file_name: file_name.value(i).to_owned(),
                rows: count.value(i),
                bytes: bytes.value(i),
                sha256: sha.value(i).to_owned(),
                recorded_at_ms: recorded.value(i),
            }));
        }
    }
    Ok(rows)
}
