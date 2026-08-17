//! What the app has stored on disk, and how to get it out.
//!
//! Users asked to "see the database and the details for it" and to "backup the
//! data-snapshot and/or export it". Worth being precise about what that is,
//! because the obvious answer is wrong:
//!
//! **There is no DuckDB in this build.** `docs/06-DATA.md` describes a
//! DuckDB/Parquet catalog as an explicitly optional later phase, and nothing in
//! the workspace depends on it — no catalog crate, no `duckdb` in any manifest.
//! Presenting an empty "database" panel implying otherwise would be worse than
//! saying so, so [`StorageReport::catalog_present`] exists to let the UI state
//! it plainly rather than leave a blank.
//!
//! What the app *does* persist is the snapshot store — `*.rdstat` files, one
//! directory per scanned root, written after every completed scan. From the
//! user's point of view that IS the database: it is where their scans live, it
//! survives a relaunch, and it is what a backup would be of.
//!
//! This module only ever READS that directory. [`crate::snapshot_store`] owns
//! writing, pruning and restoring, and duplicating any of that here would be
//! two implementations of the same invariant. The one exception is
//! [`export_snapshot`], which copies a file out — and copying out cannot
//! corrupt what stays behind.

// Everything below is reached through the Tauri commands that expose it. The
// command layer is held by another session while it lands its own work, so for
// the moment these are used only by the tests in this file — which is what
// `dead_code` is reporting. Remove this the moment the commands land; it is a
// scaffold, not a policy.
#![allow(dead_code)]

use std::fs;
use std::path::{Component, Path, PathBuf};

use rdirstat_core::snapshot;
use serde::{Deserialize, Serialize};

/// Cap on how many snapshot files a report will describe.
///
/// The store keeps two per root and prunes, so a realistic install has a
/// handful. This exists so a directory that has somehow accumulated thousands
/// — a pruning bug, a user restoring an old backup wholesale — produces a
/// bounded response instead of an unbounded one, and says it truncated.
const MAX_REPORTED: usize = 500;

/// One stored snapshot, described without decoding its arena.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct StoredSnapshot {
    /// Absolute path to the `.rdstat` file, so the user can find it in Finder.
    pub path: String,
    /// The root that was scanned.
    pub root_path: String,
    /// `st_dev` of the volume it was scanned from.
    pub device: u64,
    /// When the scan behind it finished, Unix milliseconds.
    pub taken_unix_ms: i64,
    pub nodes: u64,
    pub directories: u64,
    /// Size of the snapshot file itself — what it costs to keep.
    pub bytes: u64,
    /// Logical bytes the scan measured. What the snapshot is *about*, as
    /// opposed to what it costs.
    pub logical: u64,
    pub allocated: u64,
    /// The build that wrote it, so an unreadable one has a suspect.
    pub tool_version: String,
}

/// A snapshot file that could not be read.
///
/// Reported rather than skipped. A file sitting in the store consuming disk
/// that the app cannot open is exactly the thing a storage panel exists to
/// surface — silently omitting it would make the panel's total disagree with
/// Finder's for no visible reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct UnreadableSnapshot {
    pub path: String,
    pub bytes: u64,
    pub reason: String,
}

/// Everything the app has on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct StorageReport {
    /// `<app data>/snapshots`. Shown so the user can open it themselves.
    pub directory: String,
    /// Whether that directory exists yet — it does not until the first scan.
    pub directory_exists: bool,
    pub snapshots: Vec<StoredSnapshot>,
    pub unreadable: Vec<UnreadableSnapshot>,
    /// Total bytes of every snapshot file, readable or not. This is the number
    /// that answers "what is this app costing me".
    pub total_bytes: u64,
    /// True when more files were found than [`MAX_REPORTED`], so the UI can say
    /// the list is partial rather than implying it is complete.
    pub truncated: bool,
    /// **False in every current build.** See the module docs: the DuckDB
    /// catalog is a documented future phase, not a missing feature, and the UI
    /// says so rather than showing an empty database panel.
    pub catalog_present: bool,
}

/// Reads the store directory and describes every snapshot in it.
///
/// Never decodes an arena: [`snapshot::peek`] reads the header and metadata and
/// stops, so this stays a few kilobytes per file rather than the ~960 MB a
/// 12-million-node snapshot occupies. That matters because this runs whenever
/// the panel is opened.
pub(crate) fn describe(store_root: &Path) -> StorageReport {
    let mut report = StorageReport {
        directory: store_root.display().to_string(),
        directory_exists: store_root.is_dir(),
        snapshots: Vec::new(),
        unreadable: Vec::new(),
        total_bytes: 0,
        truncated: false,
        catalog_present: false,
    };
    if !report.directory_exists {
        return report;
    }

    for file in snapshot_files(store_root) {
        let bytes = fs::metadata(&file).map_or(0, |meta| meta.len());
        report.total_bytes = report.total_bytes.saturating_add(bytes);

        if report.snapshots.len() + report.unreadable.len() >= MAX_REPORTED {
            report.truncated = true;
            continue;
        }

        match peek_file(&file) {
            Ok(peek) => report.snapshots.push(StoredSnapshot {
                path: file.display().to_string(),
                root_path: peek.root_path.display().to_string(),
                device: peek.volume.device,
                taken_unix_ms: peek.finished_unix_ms,
                nodes: peek.nodes,
                directories: peek.directories,
                bytes,
                logical: peek.totals.logical,
                allocated: peek.totals.allocated,
                tool_version: peek.tool_version,
            }),
            Err(reason) => report.unreadable.push(UnreadableSnapshot {
                path: file.display().to_string(),
                bytes,
                reason,
            }),
        }
    }

    // Newest first: the most recent scan is the one a user is looking for, and
    // an unsorted directory listing is whatever order the filesystem felt like.
    report.snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.taken_unix_ms));
    report
}

fn peek_file(path: &Path) -> Result<snapshot::Peek, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut reader = std::io::BufReader::new(file);
    snapshot::peek(&mut reader, snapshot::Limits::default()).map_err(|error| error.to_string())
}

/// Every `*.rdstat` under the store, one directory deep.
///
/// The layout is `<store>/<root-id>/<file>.rdstat`, so this deliberately does
/// not recurse further: a deep walk of a directory the app does not own is how
/// a storage panel turns into an accidental disk scan.
fn snapshot_files(store_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(roots) = fs::read_dir(store_root) else {
        return files;
    };
    for root in roots.flatten() {
        let Ok(entries) = fs::read_dir(root.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rdstat") {
                files.push(path);
            }
        }
    }
    files
}

/// Copies one snapshot out of the store to a path the user chose.
///
/// A plain copy, deliberately. The alternative — re-serializing the arena —
/// would produce a file this build can read and give no guarantee about the
/// one that wrote it; copying the bytes preserves the original exactly,
/// including its checksum, so a restore later verifies against the same digest
/// the writer computed.
///
/// # Errors
///
/// A source outside the store, a destination that is not an absolute `..`-free
/// path, or any I/O failure. The source check is not paranoia: the path comes
/// from the frontend, and without it this is an arbitrary-file-read primitive
/// dressed up as an export.
pub(crate) fn export_snapshot(store_root: &Path, source: &Path, destination: &Path) -> Result<u64, String> {
    if !source.starts_with(store_root) {
        return Err("that file is not in this app's snapshot store".to_owned());
    }
    if !acceptable(source) || !acceptable(destination) {
        return Err("paths must be absolute and free of `..` segments".to_owned());
    }
    if source.extension().is_none_or(|ext| ext != "rdstat") {
        return Err("only .rdstat snapshots can be exported".to_owned());
    }
    // Refuse to clobber. An export that silently overwrote a previous export
    // would destroy the very thing the user asked to keep.
    if destination.exists() {
        return Err(format!("{} already exists", destination.display()));
    }
    fs::copy(source, destination).map_err(|error| error.to_string())
}

fn acceptable(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| matches!(part, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory whose path contains no `..`.
    ///
    /// Canonicalized on purpose: `CARGO_MANIFEST_DIR/../target` literally
    /// contains a `..` component, and `export_snapshot` refuses those. The
    /// first run of these tests failed on exactly that — the fixture, not the
    /// code — which is a decent argument that the guard is doing its job.
    fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/storage-scratch");
        fs::create_dir_all(&base).expect("scratch root");
        let base = base.canonicalize().expect("canonicalize scratch root");
        tempfile::Builder::new().prefix("case-").tempdir_in(&base).expect("tempdir")
    }

    #[test]
    fn a_store_that_does_not_exist_yet_reports_empty_rather_than_failing() {
        // Before the first scan there is no directory. That is the ordinary
        // state on a fresh install, not an error, and the panel has to render.
        let dir = scratch();
        let report = describe(&dir.path().join("never-created"));
        assert!(!report.directory_exists);
        assert!(report.snapshots.is_empty());
        assert_eq!(report.total_bytes, 0);
    }

    #[test]
    fn the_catalog_is_reported_absent_rather_than_implied_present() {
        // The load-bearing honesty: no DuckDB exists in this build, and the UI
        // needs to be able to say so instead of drawing an empty database.
        let dir = scratch();
        assert!(!describe(dir.path()).catalog_present);
    }

    #[test]
    fn an_unreadable_snapshot_is_reported_and_still_counted() {
        // A corrupt file still occupies disk. Omitting it would make the
        // panel's total quietly disagree with Finder's.
        let dir = scratch();
        let root = dir.path().join("abc123");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("1700000000000-00000001.rdstat"), b"not a snapshot").expect("write");

        let report = describe(dir.path());
        assert!(report.snapshots.is_empty());
        assert_eq!(report.unreadable.len(), 1);
        assert_eq!(report.total_bytes, 14);
        assert!(!report.unreadable[0].reason.is_empty(), "a reason must be given");
    }

    #[test]
    fn files_that_are_not_snapshots_are_ignored() {
        let dir = scratch();
        let root = dir.path().join("abc123");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("notes.txt"), b"hello").expect("write");

        let report = describe(dir.path());
        assert!(report.snapshots.is_empty());
        assert!(report.unreadable.is_empty());
        assert_eq!(report.total_bytes, 0, "a stray text file is not the store's cost");
    }

    #[test]
    fn export_refuses_a_source_outside_the_store() {
        // The source path comes from the frontend. Without this check the
        // command is an arbitrary-file-read primitive wearing an export label.
        let dir = scratch();
        let outside = dir.path().join("elsewhere.rdstat");
        fs::write(&outside, b"x").expect("write");
        let store = dir.path().join("store");
        fs::create_dir_all(&store).expect("mkdir");

        let error = export_snapshot(&store, &outside, &dir.path().join("out.rdstat"))
            .expect_err("a source outside the store must be refused");
        assert!(error.contains("not in this app's snapshot store"), "got {error}");
    }

    #[test]
    fn export_refuses_to_overwrite_an_existing_file() {
        // An export that clobbered a previous export would destroy exactly the
        // thing the user asked to keep.
        let dir = scratch();
        let store = dir.path().join("store/abc");
        fs::create_dir_all(&store).expect("mkdir");
        let source = store.join("a.rdstat");
        fs::write(&source, b"payload").expect("write");
        let destination = dir.path().join("taken.rdstat");
        fs::write(&destination, b"do not lose me").expect("write");

        let error = export_snapshot(&dir.path().join("store"), &source, &destination)
            .expect_err("must refuse to clobber");
        assert!(error.contains("already exists"), "got {error}");
        assert_eq!(fs::read(&destination).expect("read"), b"do not lose me");
    }

    #[test]
    fn export_copies_the_bytes_verbatim() {
        // Byte-for-byte, so the original checksum still verifies on restore.
        let dir = scratch();
        let store = dir.path().join("store/abc");
        fs::create_dir_all(&store).expect("mkdir");
        let source = store.join("a.rdstat");
        fs::write(&source, b"exact bytes").expect("write");
        let destination = dir.path().join("copy.rdstat");

        let written = export_snapshot(&dir.path().join("store"), &source, &destination).expect("export");
        assert_eq!(written, 11);
        assert_eq!(fs::read(&destination).expect("read"), b"exact bytes");
    }

    #[test]
    fn export_refuses_a_relative_or_dotdot_destination() {
        let dir = scratch();
        let store = dir.path().join("store/abc");
        fs::create_dir_all(&store).expect("mkdir");
        let source = store.join("a.rdstat");
        fs::write(&source, b"x").expect("write");

        assert!(export_snapshot(&dir.path().join("store"), &source, Path::new("relative.rdstat")).is_err());
        assert!(
            export_snapshot(&dir.path().join("store"), &source, &dir.path().join("../escape.rdstat")).is_err()
        );
    }
}
