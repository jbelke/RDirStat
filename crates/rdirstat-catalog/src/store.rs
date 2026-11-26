//! Part-file naming, atomic publish, and listing.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::CatalogError;

/// Longest run id accepted; keeps file names well under `NAME_MAX`.
const MAX_RUN_ID: usize = 128;

/// zstd level for every part file. Level 3 is zstd's own default: a good
/// ratio for path-heavy columns without making a 70M-row write CPU-bound.
const ZSTD_LEVEL: i32 = 3;

/// How many sequence numbers to try when another writer keeps winning the
/// race for the next name. Reaching this means something is wrong, not busy.
const MAX_PUBLISH_ATTEMPTS: u32 = 1_000;

fn io(operation: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> CatalogError {
    let path = path.to_path_buf();
    move |source| CatalogError::Io {
        operation,
        path,
        source,
    }
}

/// Refuses a run id that could escape the directory, hide the file, or make
/// `<kind>-<run_id>-<seq>` ambiguous to a shell glob.
pub(crate) fn validate_run_id(run_id: &str) -> Result<(), CatalogError> {
    let ok = !run_id.is_empty()
        && run_id.len() <= MAX_RUN_ID
        && run_id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(CatalogError::InvalidRunId(run_id.to_owned()))
    }
}

/// `<kind>-<run_id>-<seq:04>.parquet`.
pub(crate) fn part_name(kind: &str, run_id: &str, seq: u32) -> String {
    format!("{kind}-{run_id}-{seq:04}.parquet")
}

/// The sequence number of `name` if it is a part of `(kind, run_id)`.
pub(crate) fn part_seq(name: &str, kind: &str, run_id: &str) -> Option<u32> {
    let rest = name.strip_prefix(kind)?.strip_prefix('-')?.strip_prefix(run_id)?;
    let digits = rest.strip_prefix('-')?.strip_suffix(".parquet")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One past the highest existing sequence number for `(kind, run_id)`.
fn next_seq(out_dir: &Path, kind: &str, run_id: &str) -> Result<u32, CatalogError> {
    let mut next = 0_u32;
    for entry in fs::read_dir(out_dir).map_err(io("list", out_dir))? {
        let entry = entry.map_err(io("list", out_dir))?;
        if let Some(seq) = entry.file_name().to_str().and_then(|n| part_seq(n, kind, run_id)) {
            next = next.max(seq.saturating_add(1));
        }
    }
    Ok(next)
}

fn sync_dir(dir: &Path) -> Result<(), CatalogError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io("fsync directory", dir))
}

/// Writes one immutable part file and returns its final path.
///
/// `fill` writes every batch; the footer, fsync, and publish happen here. On
/// any failure the hidden temp file — which this function created and nothing
/// else references — is removed, and no final name is ever created.
pub(crate) fn write_part<F>(
    out_dir: &Path,
    kind: &str,
    run_id: &str,
    schema: &Arc<Schema>,
    fill: F,
) -> Result<PathBuf, CatalogError>
where
    F: FnOnce(&mut ArrowWriter<File>) -> Result<(), CatalogError>,
{
    validate_run_id(run_id)?;
    fs::create_dir_all(out_dir).map_err(io("create directory", out_dir))?;

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let temp = out_dir.join(format!(".{kind}-{run_id}.tmp-{}-{nanos}", std::process::id()));

    let result = write_temp(&temp, schema, fill).and_then(|()| publish(out_dir, kind, run_id, &temp));
    // The temp name is ours alone (create_new), and after a successful
    // publish the data lives on under the final name's hard link.
    let _ = fs::remove_file(&temp);
    let path = result?;
    sync_dir(out_dir)?;
    Ok(path)
}

fn write_temp<F>(temp: &Path, schema: &Arc<Schema>, write_batches: F) -> Result<(), CatalogError>
where
    F: FnOnce(&mut ArrowWriter<File>) -> Result<(), CatalogError>,
{
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)
        .map_err(io("create", temp))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(ZSTD_LEVEL)?))
        .set_created_by(format!("rdirstat-catalog {}", env!("CARGO_PKG_VERSION")))
        .build();
    let mut writer = ArrowWriter::try_new(file, Arc::clone(schema), Some(props))?;
    write_batches(&mut writer)?;
    let written = writer.into_inner()?;
    written.sync_all().map_err(io("fsync", temp))?;
    Ok(())
}

/// Gives the temp file its final name without ever replacing an existing one.
fn publish(out_dir: &Path, kind: &str, run_id: &str, temp: &Path) -> Result<PathBuf, CatalogError> {
    let mut seq = next_seq(out_dir, kind, run_id)?;
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let target = out_dir.join(part_name(kind, run_id, seq));
        match fs::hard_link(temp, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                seq = seq.saturating_add(1);
            }
            Err(error) => return Err(io("publish", &target)(error)),
        }
    }
    Err(CatalogError::Io {
        operation: "publish",
        path: out_dir.join(part_name(kind, run_id, seq)),
        source: std::io::Error::new(ErrorKind::AlreadyExists, "no free sequence number"),
    })
}

/// Copies the published part `source` into `mirror_dir` under the same file
/// name, with the same temp, fsync, and `link(2)` publish as a fresh write.
///
/// A mirror file of that name with identical bytes is success (re-mirroring
/// is idempotent). One with different bytes is
/// [`CatalogError::MirrorConflict`]: the mirror is never overwritten.
pub(crate) fn mirror_part(source: &Path, mirror_dir: &Path) -> Result<PathBuf, CatalogError> {
    let name = source.file_name().ok_or_else(|| CatalogError::Io {
        operation: "mirror",
        path: source.to_path_buf(),
        source: std::io::Error::new(ErrorKind::InvalidInput, "not a file path"),
    })?;
    let target = mirror_dir.join(name);
    if target.exists() {
        return same_bytes_or_conflict(source, target);
    }
    fs::create_dir_all(mirror_dir).map_err(io("create directory", mirror_dir))?;

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let temp = mirror_dir.join(format!(
        ".{}.tmp-{}-{nanos}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let linked = copy_to_temp(source, &temp).and_then(|()| match fs::hard_link(&temp, &target) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(io("publish", &target)(error)),
    });
    let _ = fs::remove_file(&temp);
    if !linked? {
        // Another writer published the same name between the check and link.
        return same_bytes_or_conflict(source, target);
    }
    sync_dir(mirror_dir)?;
    Ok(target)
}

fn copy_to_temp(source: &Path, temp: &Path) -> Result<(), CatalogError> {
    let mut input = File::open(source).map_err(io("open", source))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)
        .map_err(io("create", temp))?;
    std::io::copy(&mut input, &mut output).map_err(io("copy", temp))?;
    output.sync_all().map_err(io("fsync", temp))
}

fn same_bytes_or_conflict(source: &Path, target: PathBuf) -> Result<PathBuf, CatalogError> {
    let ours = fs::read(source).map_err(io("read", source))?;
    let theirs = fs::read(&target).map_err(io("read", &target))?;
    if ours == theirs {
        Ok(target)
    } else {
        Err(CatalogError::MirrorConflict { path: target })
    }
}

/// Every published part of `kind` in `dir`, sorted by file name.
///
/// A missing directory is an empty catalog, not an error: the first backup to
/// a fresh destination has nothing to dedupe against.
pub(crate) fn list_parts(dir: &Path, kind: &str) -> Result<Vec<PathBuf>, CatalogError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io("list", dir)(error)),
    };
    let prefix = if kind.is_empty() {
        String::new()
    } else {
        format!("{kind}-")
    };
    let mut parts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io("list", dir))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with('.') && name.starts_with(&prefix) && name.ends_with(".parquet") {
            parts.push(entry.path());
        }
    }
    parts.sort();
    Ok(parts)
}

/// Every record batch in one part file.
pub(crate) fn read_batches(path: &Path) -> Result<Vec<RecordBatch>, CatalogError> {
    let file = File::open(path).map_err(io("open", path))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch?);
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_that_could_escape_or_hide_are_refused() {
        for bad in ["", "../x", "a/b", ".hidden", "-x", "a b", "é"] {
            assert!(validate_run_id(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["20261125T101500Z", "run-1", "a.b_c"] {
            assert!(validate_run_id(good).is_ok(), "{good:?} refused");
        }
        assert!(validate_run_id(&"a".repeat(MAX_RUN_ID + 1)).is_err());
    }

    #[test]
    fn sequence_parsing_ignores_other_runs_and_temps() {
        assert_eq!(part_seq("hashes-r1-0007.parquet", "hashes", "r1"), Some(7));
        assert_eq!(part_seq("hashes-r1-12345.parquet", "hashes", "r1"), Some(12_345));
        assert_eq!(part_seq("hashes-r1-0001-0000.parquet", "hashes", "r1"), None);
        assert_eq!(part_seq("hashes-r1-0001-0000.parquet", "hashes", "r1-0001"), Some(0));
        assert_eq!(part_seq(".hashes-r1.tmp-1-2", "hashes", "r1"), None);
        assert_eq!(part_seq("receipts-r1-0000.parquet", "hashes", "r1"), None);
    }
}
