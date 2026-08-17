//! Where `*.rdstat` snapshots live, and how they are written safely.
//!
//! `rdirstat_core::snapshot` owns the byte format and nothing else — it never
//! names a path. This module is the other half: it chooses the directory,
//! names the file, makes the write atomic, decides what to keep, and picks
//! which snapshot to restore at launch.
//!
//! ## Why this exists
//!
//! A full-volume scan of this machine is 8.9M entries across 1.2M directories
//! and takes minutes. Without a snapshot the app opens on an empty volume
//! picker every single time and the only way to look at a tree is to pay that
//! cost again. With one, launch is a file read.
//!
//! ## Atomicity
//!
//! A snapshot is written to a staging file in the *same directory* as its
//! destination, fsynced, then renamed over. `rename(2)` within a directory is
//! atomic, so a reader either sees the previous snapshot or the new one and
//! never a partial file. The directory itself is fsynced afterwards, because a
//! rename that is not durable can be lost by a crash even though the file
//! contents were.
//!
//! Staging files are named with a leading `.` and are skipped by the loader, so
//! a crash mid-write leaves litter rather than a candidate that looks loadable.
//! [`SnapshotStore::save`] sweeps stale staging files on its way past.
//!
//! ## Naming
//!
//! ```text
//! <app data>/snapshots/<root-id>/<finished-ms>-<scan-id>.rdstat
//! ```
//!
//! `<root-id>` is a digest of the root path, never the path text itself
//! (docs/06-DATA.md: "Root and scan IDs are generated safe identifiers, never
//! raw user path text"). That keeps a directory listing from disclosing what
//! the user scanned, and keeps a path containing `/` or a newline from becoming
//! a filename. `<finished-ms>` is zero-padded so lexicographic order is
//! chronological order, which is what makes "newest" a sort rather than a stat
//! of every candidate.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use rdirstat_core::CompletedScan;
use rdirstat_core::snapshot::{self, Limits, SnapshotError};

/// How many snapshots to keep per scanned root.
///
/// Small on purpose: each one is the size of the arena, and this is a cache
/// that makes launch fast, not the cross-scan history. History — and therefore
/// diffing — is the Parquet catalog's job (docs/01-ARCHITECTURE.md#persistence),
/// and it stores rows, not arenas.
const KEEP_PER_ROOT: usize = 2;

/// Prefix marking a partially written file. Never a load candidate.
const STAGING_PREFIX: &str = ".staging-";

/// Extension for a complete snapshot.
const EXTENSION: &str = "rdstat";

/// The on-disk snapshot cache.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotStore {
    /// `<app data>/snapshots`.
    root: PathBuf,
}

/// Where a snapshot came from, for the log line and, later, the UI.
///
/// Not `Clone`: it owns a whole arena, and duplicating one would double the
/// largest allocation in the process to no purpose. It is moved into
/// `publish_restored`, which puts it behind the `Arc` everything else shares.
#[derive(Debug)]
pub(crate) struct Restored {
    /// The scan itself, with its original ids still in place. The caller
    /// re-stamps them before publishing.
    pub(crate) scan: Box<CompletedScan>,
    /// The file it was read from.
    pub(crate) path: PathBuf,
    /// Its size on disk.
    pub(crate) bytes: u64,
}

impl SnapshotStore {
    /// Resolves the store under the app's data directory, creating it if needed.
    ///
    /// # Errors
    ///
    /// Any I/O error from resolving or creating the directory. A store that
    /// cannot be created is not fatal: the app still scans, it just cannot
    /// remember.
    pub(crate) fn new(app: &tauri::AppHandle) -> Result<Self, std::io::Error> {
        use tauri::Manager as _;

        let base = app
            .path()
            .app_data_dir()
            .map_err(|error| std::io::Error::other(format!("no app data directory: {error}")))?;
        let root = base.join("snapshots");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// A store rooted at an explicit directory. For tests.
    #[cfg(test)]
    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Writes `scan` and prunes older snapshots of the same root.
    ///
    /// Returns the path written.
    ///
    /// # Errors
    ///
    /// [`SnapshotError`] from encoding, or any I/O error from staging, syncing,
    /// or renaming.
    pub(crate) fn save(&self, scan: &CompletedScan) -> Result<PathBuf, SnapshotError> {
        let dir = self.root.join(root_id(scan));
        fs::create_dir_all(&dir)?;

        // Staged in the destination directory, never in a temp dir: a rename
        // across filesystems is not atomic, and `/tmp` is frequently a
        // different volume than Application Support.
        let staging = dir.join(format!("{STAGING_PREFIX}{}.{EXTENSION}", std::process::id()));
        let final_path = dir.join(format!(
            "{:013}-{:08}.{EXTENSION}",
            scan.finished_unix_ms.max(0),
            scan.scan_id.get()
        ));

        {
            let file = File::create(&staging)?;
            let mut writer = BufWriter::new(file);
            snapshot::write(scan, &mut writer)?;
            // Flush the userspace buffer, then force the bytes to the device.
            // A rename is only as durable as the data it points at.
            let file = writer
                .into_inner()
                .map_err(|error| SnapshotError::Io(std::io::Error::other(error.to_string())))?;
            file.sync_all()?;
        }

        fs::rename(&staging, &final_path)?;
        sync_dir(&dir);

        Self::prune(&dir, &final_path);
        Ok(final_path)
    }

    /// Loads the most recent snapshot across every root, newest first.
    ///
    /// Returns `None` when there is nothing to restore. A snapshot that fails
    /// to load is **skipped, not fatal**: a stale format, a truncated file from
    /// a full disk, or an arena from a build with a different `Node` layout all
    /// mean "this one is no good, try the next", and having none left simply
    /// means the app opens on the volume picker as it always did.
    pub(crate) fn load_newest(&self) -> Option<Restored> {
        let mut candidates = self.candidates();
        // Newest first. The filename begins with a zero-padded finish time, so
        // this is chronological without stat'ing anything.
        candidates.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));

        for path in candidates {
            match Self::load(&path) {
                Ok(restored) => return Some(restored),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "discarding an unloadable snapshot");
                    // A file this build can never read is litter. Removing it
                    // keeps a permanently broken snapshot from being retried on
                    // every launch, and it is only ever a cache.
                    let _ = fs::remove_file(&path);
                }
            }
        }
        None
    }

    /// Reads one snapshot file.
    fn load(path: &Path) -> Result<Restored, SnapshotError> {
        let file = File::open(path)?;
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or_default();
        let mut reader = BufReader::new(file);
        let scan = snapshot::read(&mut reader, Limits::DESIGN_PROFILE)?;
        Ok(Restored {
            scan: Box::new(scan),
            path: path.to_path_buf(),
            bytes,
        })
    }

    /// Every complete snapshot under the store, across all roots.
    fn candidates(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(roots) = fs::read_dir(&self.root) else {
            return found;
        };
        for root in roots.flatten() {
            let Ok(entries) = fs::read_dir(root.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if is_snapshot(&path) {
                    found.push(path);
                }
            }
        }
        found
    }

    /// Keeps the newest [`KEEP_PER_ROOT`] snapshots in `dir` and removes the
    /// rest, along with any staging file left by a crashed write.
    fn prune(dir: &Path, just_written: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut snapshots = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if is_staging(&path) {
                // Someone else's in-flight write, or our own crash. Only sweep
                // it if it is not being written right now; an mtime in the past
                // is the cheap, portable version of that test.
                if is_stale(&path) {
                    let _ = fs::remove_file(&path);
                }
            } else if is_snapshot(&path) {
                snapshots.push(path);
            }
        }
        snapshots.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
        for old in snapshots.into_iter().skip(KEEP_PER_ROOT) {
            if old == just_written {
                continue;
            }
            let _ = fs::remove_file(old);
        }
        sync_dir(dir);
    }
}

/// Whether `path` is a complete snapshot rather than a staging file.
fn is_snapshot(path: &Path) -> bool {
    if is_staging(path) {
        return false;
    }
    path.extension().is_some_and(|ext| ext == EXTENSION) && path.is_file()
}

fn is_staging(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(STAGING_PREFIX))
}

/// Whether a staging file is old enough to be abandoned rather than in flight.
///
/// A write of a multi-gigabyte arena can take a while, so the threshold is
/// generous: the cost of sweeping too eagerly is corrupting a live write, and
/// the cost of sweeping too late is one stale file.
fn is_stale(path: &Path) -> bool {
    const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age > ABANDONED_AFTER)
}

/// Fsyncs a directory so a rename within it survives a crash.
///
/// Best effort: some filesystems refuse to open a directory for this, and a
/// snapshot that is merely *probably* durable is still worth having. It is a
/// cache, not a system of record.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// A stable, safe directory name for a scan root.
///
/// Deliberately a digest of the path bytes rather than the path itself:
/// docs/06-DATA.md requires generated identifiers, and a real macOS path can
/// contain `/`, a newline, or bytes that are not UTF-8 — none of which belong
/// in a filename. The digest is FNV-1a, chosen because it is five lines and
/// fixed forever; a hasher whose output can change between Rust releases would
/// silently orphan every existing snapshot on a toolchain bump.
fn root_id(scan: &CompletedScan) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let bytes = path_bytes(&scan.root_path);
    let mut hash = OFFSET;
    for byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    // The device number joins the digest so the same mount point on two
    // different volumes does not share a directory.
    hash = (hash ^ scan.volume.device).wrapping_mul(PRIME);
    format!("{hash:016x}")
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use std::sync::Arc;

    use rdirstat_core::{ScanId, ScanOptions, TreeGeneration};

    use super::*;
    use crate::engine::{ScanOutcome, ScanRequest};
    use crate::progress::ProgressCounters;
    use crate::state::CancelToken;

    /// Scans a real directory, because the point of the store is round-tripping
    /// an arena the scanner actually produced — not one hand-built to be easy.
    fn scan_of(root: &Path, scan_id: u64, finished_ms: i64) -> Arc<CompletedScan> {
        let outcome = crate::engine::run(ScanRequest {
            root: root.to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::from_raw(scan_id),
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        });
        match outcome {
            ScanOutcome::Completed(mut scan) => {
                // Pinned so the filename, and therefore the newest-first sort,
                // is deterministic instead of depending on how fast the test ran.
                scan.finished_unix_ms = finished_ms;
                Arc::from(scan)
            }
            other => panic!("expected a completed scan, got {other:?}"),
        }
    }

    fn tree_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        for index in 0..8_u32 {
            std::fs::write(dir.path().join(format!("f{index:02}.bin")), vec![b'x'; 100]).expect("write");
        }
        std::fs::write(dir.path().join("sub/deep.bin"), vec![b'y'; 5_000]).expect("write");
        dir
    }

    fn store_in(home: &tempfile::TempDir) -> SnapshotStore {
        SnapshotStore::at(home.path().join("snapshots"))
    }

    #[test]
    fn a_saved_scan_comes_back_with_the_same_tree() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);
        let scan = scan_of(source.path(), 1, 1_700_000_000_000);

        store.save(&scan).expect("saves");
        let restored = store.load_newest().expect("restores");

        assert_eq!(restored.scan.tree, scan.tree, "the arena changed on the way through");
        assert_eq!(restored.scan.root_path, scan.root_path);
        assert_eq!(restored.scan.totals, scan.totals);
        assert!(restored.bytes > 0, "a restored snapshot reported zero bytes");
    }

    #[test]
    fn nothing_saved_means_nothing_restored() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(store_in(&home).load_newest().is_none());
    }

    #[test]
    fn the_newest_snapshot_wins() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);

        store
            .save(&scan_of(source.path(), 1, 1_700_000_000_000))
            .expect("saves");
        let newer = scan_of(source.path(), 2, 1_700_000_500_000);
        store.save(&newer).expect("saves");

        let restored = store.load_newest().expect("restores");
        assert_eq!(
            restored.scan.finished_unix_ms, newer.finished_unix_ms,
            "an older snapshot was restored in preference to a newer one"
        );
    }

    #[test]
    fn retention_keeps_only_the_newest_few() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);

        for index in 0..(KEEP_PER_ROOT as u64 + 3) {
            let base = 1_700_000_000_000 + i64::try_from(index).expect("fits") * 1000;
            store.save(&scan_of(source.path(), index, base)).expect("saves");
        }

        let kept = store.candidates();
        assert_eq!(
            kept.len(),
            KEEP_PER_ROOT,
            "retention kept {} snapshots, expected {KEEP_PER_ROOT}",
            kept.len()
        );
    }

    #[test]
    fn two_roots_do_not_share_a_directory() {
        let first = tree_fixture();
        let second = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);

        store.save(&scan_of(first.path(), 1, 1_700_000_000_000)).expect("saves");
        store
            .save(&scan_of(second.path(), 2, 1_700_000_001_000))
            .expect("saves");

        // Both survive: retention is per root, so scanning a second volume must
        // not evict the first one's history.
        assert_eq!(store.candidates().len(), 2);
    }

    #[test]
    fn a_root_id_never_contains_the_path_text() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);
        let scan = scan_of(source.path(), 1, 1_700_000_000_000);
        let path = store.save(&scan).expect("saves");

        let as_text = path.to_string_lossy();
        let leaf = source
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a temp dir has a name");
        assert!(
            !as_text.contains(leaf),
            "the scanned path leaked into the snapshot filename: {as_text}"
        );
    }

    #[test]
    fn a_corrupt_snapshot_is_discarded_rather_than_retried_forever() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);
        let path = store
            .save(&scan_of(source.path(), 1, 1_700_000_000_000))
            .expect("saves");

        let mut bytes = fs::read(&path).expect("reads");
        let target = bytes.len() - 12;
        bytes[target] ^= 0xff;
        fs::write(&path, &bytes).expect("writes");

        assert!(store.load_newest().is_none(), "a corrupt snapshot was restored");
        assert!(
            !path.exists(),
            "a permanently unreadable snapshot was left to fail again"
        );
    }

    #[test]
    fn a_staging_file_is_never_a_load_candidate() {
        let source = tree_fixture();
        let home = tempfile::tempdir().expect("tempdir");
        let store = store_in(&home);
        let path = store
            .save(&scan_of(source.path(), 1, 1_700_000_000_000))
            .expect("saves");

        // A crash mid-write leaves exactly this: a complete-looking file with a
        // staging name. It must not be picked up, whatever its timestamp.
        let dir = path.parent().expect("has a parent");
        let staging = dir.join(format!("{STAGING_PREFIX}99999.{EXTENSION}"));
        fs::copy(&path, &staging).expect("copies");
        fs::remove_file(&path).expect("removes");

        assert!(store.load_newest().is_none(), "a staging file was restored");
    }
}
