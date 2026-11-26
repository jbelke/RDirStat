//! Plan and apply against temp dirs. Two temp dirs always share one disk, so
//! these tests hand the code fixed disk facts through [`DiskProbe`]; the real
//! probe's refusal of that pair is covered in `tests/cli.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test code: a failed fixture must abort loudly"
)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use rdirstat_catalog::{ReceiptRow, read_receipts, write_receipts};
use rdirstat_organize::{DiskFacts, PlanFile, Rules, plan_token};

use super::apply::Outcome;
use super::plan::RULE_CHANGED_SINCE_SCAN;
use super::*;

/// Fixed facts: paths under `source` are on `source_disk`, the rest on
/// `dest_disk`.
struct FakeProbe {
    source: PathBuf,
    source_disk: Option<&'static str>,
    dest_disk: Option<&'static str>,
    dest_uuid: Option<&'static str>,
}

const DEST_UUID: &str = "B6C3D4E7-1AA7-3BD1-B725-6033F3101516";

impl DiskProbe for FakeProbe {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let on_source = path.starts_with(&self.source);
        Ok(DiskFacts {
            disk_id: if on_source { self.source_disk } else { self.dest_disk }.map(str::to_owned),
            volume_uuid: if on_source { Some("SOURCE-UUID") } else { self.dest_uuid }.map(str::to_owned),
            st_dev: if on_source { 1 } else { 2 },
            filesystem: "apfs".into(),
            total_bytes: 1 << 40,
            available_bytes: 1 << 40,
        })
    }
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    source: PathBuf,
    dest: PathBuf,
    catalog: PathBuf,
    plan_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with(build_source)
    }

    fn with(build: fn(&Path)) -> Self {
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let cat = tempfile::tempdir().expect("tempdir");
        let source_root = std::fs::canonicalize(src.path()).expect("canonical");
        let dest_root = std::fs::canonicalize(dst.path()).expect("canonical");
        let catalog = std::fs::canonicalize(cat.path()).expect("canonical");
        build(&source_root);
        Self {
            plan_path: catalog.join("plan.json"),
            _dirs: vec![src, dst, cat],
            source: source_root,
            dest: dest_root,
            catalog,
        }
    }

    fn probe(&self) -> FakeProbe {
        FakeProbe {
            source: self.source.clone(),
            source_disk: Some("disk3"),
            dest_disk: Some("disk14"),
            dest_uuid: Some(DEST_UUID),
        }
    }

    fn request(&self) -> PlanRequest {
        PlanRequest {
            source: Some(self.source.clone()),
            label: "mac-data".into(),
            dest: self.dest.clone(),
            floor: 8192,
            snapshot: None,
            rules: Rules::default_backup(),
            out: Some(self.plan_path.clone()),
            catalog: self.catalog.clone(),
            app_catalog: None,
            progress: false,
            rollup: true,
        }
    }

    fn plan(&self) -> PlanOutcome {
        build_plan(&self.request(), &self.probe()).expect("plan")
    }

    fn apply(&self, token: &str) -> Result<ApplyReport> {
        apply(
            &ApplyRequest {
                plan: self.plan_path.clone(),
                token: token.to_owned(),
                catalog: self.catalog.clone(),
                app_catalog: None,
                progress: false,
            },
            &self.probe(),
        )
    }

    fn mirror(&self, relative: &str) -> PathBuf {
        self.dest.join("Backups/mac-data").join(relative)
    }

    fn receipts(&self) -> (Vec<ReceiptRow>, Vec<ReceiptRow>) {
        (
            read_receipts(&self.catalog).expect("primary"),
            read_receipts(&self.dest.join(".rdirstat/catalog")).expect("mirror"),
        )
    }
}

fn bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 31 + usize::from(seed)) % 251).unwrap_or(0))
        .collect()
}

fn put(root: &Path, relative: &str, contents: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// Two large files, a duplicate pair, a package, a skipped `node_modules`, a
/// file under the floor, and a symlink.
fn build_source(root: &Path) {
    put(root, "Movies/big.bin", &bytes(1, 64 * 1024));
    put(root, "Documents/report.pdf", &bytes(2, 32 * 1024));
    put(root, "Documents/dupe-a.bin", &bytes(3, 16 * 1024));
    put(root, "Archive/old/dupe-b.bin", &bytes(3, 16 * 1024));
    put(root, "Applications/Tool.app/Contents/MacOS/tool", &bytes(4, 24 * 1024));
    put(root, "Applications/Tool.app/Contents/Info.plist", &bytes(5, 512));
    put(root, "src/web/node_modules/pkg/big.js", &bytes(6, 40 * 1024));
    put(root, "notes.txt", b"small");
    std::os::unix::fs::symlink("Movies/big.bin", root.join("link-to-big")).expect("symlink");
}

/// Everything observable about a tree: per entry, its kind, size, inode,
/// mtime, and contents (or link target). Directory mtimes catch any entry
/// created, renamed, or removed inside.
type Fingerprint = BTreeMap<PathBuf, (char, u64, u64, i64, i64, Vec<u8>)>;

fn fingerprint(root: &Path) -> Fingerprint {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let meta = std::fs::symlink_metadata(&dir).expect("lstat");
        out.insert(
            dir.clone(),
            ('d', 0, meta.ino(), meta.mtime(), meta.mtime_nsec(), Vec::new()),
        );
        for entry in std::fs::read_dir(&dir).expect("readdir") {
            let path = entry.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            let kind = meta.file_type();
            if kind.is_dir() {
                stack.push(path);
            } else {
                let contents = if kind.is_symlink() {
                    std::fs::read_link(&path)
                        .expect("readlink")
                        .into_os_string()
                        .into_encoded_bytes()
                } else {
                    std::fs::read(&path).expect("read")
                };
                let tag = if kind.is_symlink() { 'l' } else { 'f' };
                out.insert(
                    path,
                    (tag, meta.size(), meta.ino(), meta.mtime(), meta.mtime_nsec(), contents),
                );
            }
        }
    }
    out
}

fn outcome_of(report: &ApplyReport, relative: &str) -> Outcome {
    report
        .results
        .iter()
        .find(|r| r.relative == relative)
        .unwrap_or_else(|| panic!("{relative} not in the report"))
        .outcome
}

#[test]
fn plan_then_apply_backs_up_without_touching_the_source() {
    let fx = Fixture::new();
    let before = fingerprint(&fx.source);

    let planned = fx.plan();
    let relatives: Vec<String> = planned
        .plan
        .items
        .iter()
        .map(|item| item.relative.to_string_lossy().into_owned())
        .collect();
    assert_eq!(relatives[0], "Movies/big.bin", "largest first: {relatives:?}");
    for expected in [
        "Documents/report.pdf",
        "Applications/Tool.app",
        "Documents/dupe-a.bin",
        "Archive/old/dupe-b.bin",
    ] {
        assert!(
            relatives.iter().any(|r| r == expected),
            "{expected} planned: {relatives:?}"
        );
    }
    assert!(!relatives.iter().any(|r| r.contains("node_modules") || r == "notes.txt"));
    assert!(
        planned.plan.skipped.iter().any(|s| s.rule_id == "node-modules"),
        "skip logged with its rule: {:?}",
        planned.plan.skipped
    );
    assert_eq!(planned.token, plan_token(&planned.plan));
    assert!(
        !fx.dest.join("Backups").exists(),
        "planning writes nothing on the destination"
    );
    let text = plan::text(&planned, 3);
    assert!(text.contains("Token: ") && text.contains("Skipped by rule:") && text.contains("node-modules"));
    let doc = plan::json(&planned, 3);
    assert_eq!(doc["token"], planned.token.as_str());
    assert_eq!(doc["top_items"].as_array().map(Vec::len), Some(3));

    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
    assert_eq!(before, fingerprint(&fx.source), "the source is unmodified");

    assert_eq!(
        std::fs::read(fx.mirror("Movies/big.bin")).expect("read"),
        bytes(1, 64 * 1024)
    );
    assert_eq!(
        std::fs::read(fx.mirror("Applications/Tool.app/Contents/MacOS/tool")).expect("read"),
        bytes(4, 24 * 1024)
    );
    assert_eq!(
        std::fs::read(fx.mirror("Archive/old/dupe-b.bin")).expect("read"),
        bytes(3, 16 * 1024)
    );
    // The second of the duplicate pair clones the first (APFS temp dirs).
    let dupes = [
        outcome_of(&report, "Documents/dupe-a.bin"),
        outcome_of(&report, "Archive/old/dupe-b.bin"),
    ];
    assert!(dupes.contains(&Outcome::Cloned), "{dupes:?}");

    let (primary, mirror) = fx.receipts();
    assert_eq!(primary.len(), planned.plan.items.len());
    assert_eq!(primary.len(), report.receipts_written);
    let mut mirrored = mirror.clone();
    mirrored.sort_by(|a, b| a.dest_path.cmp(&b.dest_path));
    let mut primaries = primary.clone();
    primaries.sort_by(|a, b| a.dest_path.cmp(&b.dest_path));
    assert_eq!(primaries, mirrored, "receipts are mirrored to the destination");
    let big = primary
        .iter()
        .find(|r| r.dest_path.ends_with("Movies/big.bin"))
        .expect("receipt");
    assert_eq!(
        big.sha256,
        rdirstat_copy::sha256_hex(&rdirstat_copy::sha256_file(&fx.source.join("Movies/big.bin")).expect("hash"))
    );
    assert!(
        std::fs::read_dir(fx.dest.join(".rdirstat/catalog"))
            .expect("mirror dir")
            .any(|e| e.expect("entry").file_name().to_string_lossy().starts_with("manifest-")),
        "the run writes a manifest"
    );

    // A second apply of the same plan copies nothing and receipts nothing.
    let again = fx.apply(&planned.token).expect("re-apply");
    assert!(again.complete());
    assert!(again.results.iter().all(|r| r.outcome == Outcome::AlreadyLanded));
    assert_eq!(again.receipts_written, 0);
    assert_eq!(before, fingerprint(&fx.source), "still unmodified");
}

#[test]
fn a_wrong_token_refuses_the_whole_plan_and_writes_nothing() {
    let fx = Fixture::new();
    let planned = fx.plan();
    let mut wrong = planned.token.clone();
    wrong.replace_range(0..1, if wrong.starts_with('0') { "1" } else { "0" });
    let error = fx.apply(&wrong).expect_err("refused");
    assert!(error.to_string().contains("token"), "{error:#}");
    assert!(!fx.dest.join("Backups").exists());
    assert!(!fx.dest.join(".rdirstat").exists());

    // A plan edited after the token was printed is refused with that token.
    let mut plan: PlanFile = serde_json::from_slice(&std::fs::read(&fx.plan_path).expect("read")).expect("parse");
    plan.items.truncate(1);
    std::fs::write(&fx.plan_path, serde_json::to_vec(&plan).expect("encode")).expect("write");
    assert!(fx.apply(&planned.token).is_err());
    assert!(!fx.dest.join("Backups").exists());
}

#[test]
fn only_a_backup_plan_is_applied() {
    let fx = Fixture::new();
    fx.plan();
    let mut plan: PlanFile = serde_json::from_slice(&std::fs::read(&fx.plan_path).expect("read")).expect("parse");
    plan.intent = "migrate".into();
    std::fs::write(&fx.plan_path, serde_json::to_vec(&plan).expect("encode")).expect("write");
    let error = fx.apply(&plan_token(&plan)).expect_err("refused");
    assert!(error.to_string().contains("Backup plans only"), "{error:#}");
    assert!(!fx.dest.join("Backups").exists());
}

#[test]
fn an_item_changed_after_the_plan_is_skipped_and_never_receipted() {
    let fx = Fixture::new();
    let planned = fx.plan();
    // Same size, new bytes and mtime.
    std::fs::write(fx.source.join("Documents/report.pdf"), bytes(9, 32 * 1024)).expect("rewrite");
    let before = fingerprint(&fx.source);

    let report = fx.apply(&planned.token).expect("apply");
    assert!(!report.complete());
    assert_eq!(outcome_of(&report, "Documents/report.pdf"), Outcome::ChangedSinceScan);
    assert_eq!(outcome_of(&report, "Movies/big.bin"), Outcome::Copied);
    assert!(
        !fx.mirror("Documents/report.pdf").exists(),
        "never copied in its new state"
    );
    let (primary, mirror) = fx.receipts();
    for rows in [&primary, &mirror] {
        assert!(!rows.iter().any(|r| r.dest_path.ends_with("report.pdf")));
        assert_eq!(rows.len(), planned.plan.items.len() - 1);
    }
    assert_eq!(before, fingerprint(&fx.source));
}

#[test]
fn a_stale_clone_source_is_copied_around_not_left_as_a_partial() {
    let fx = Fixture::new();
    let planned = fx.plan();
    let before = fingerprint(&fx.source);

    // A stale catalog row on the drive: it claims `stale.bin` holds big.bin's
    // bytes, but it was edited in place since (same length, other bytes).
    // Apply must not clone it; it copies big.bin from the source instead.
    let source_big = fx.source.join("Movies/big.bin");
    let stale = fx.dest.join("stale.bin");
    std::fs::write(&stale, bytes(7, 64 * 1024)).expect("write");
    let row = ReceiptRow {
        run_id: "earlier".into(),
        source_label: "other".into(),
        source_path: "/elsewhere/big.bin".into(),
        source_dev: 9,
        source_ino: 9,
        size: 64 * 1024,
        mtime_ns: 0,
        dest_path: stale.to_string_lossy().into_owned(),
        method: "copy".into(),
        sha256: rdirstat_copy::sha256_hex(&rdirstat_copy::sha256_file(&source_big).expect("hash")),
        verified_at_ms: 0,
    };
    write_receipts(&[row], &fx.dest.join(".rdirstat/catalog"), "earlier").expect("seed");

    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
    assert_eq!(outcome_of(&report, "Movies/big.bin"), Outcome::Copied);
    assert_eq!(
        std::fs::read(fx.mirror("Movies/big.bin")).expect("read"),
        bytes(1, 64 * 1024)
    );
    assert_eq!(std::fs::read(&stale).expect("read"), bytes(7, 64 * 1024), "untouched");
    assert_eq!(before, fingerprint(&fx.source));
}

#[test]
fn same_or_unknown_disks_are_refused_at_plan_and_at_apply() {
    let fx = Fixture::new();
    for (source_disk, dest_disk) in [
        (Some("disk3"), Some("disk3")),
        (None, Some("disk14")),
        (Some("disk3"), None),
    ] {
        let probe = FakeProbe {
            source: fx.source.clone(),
            source_disk,
            dest_disk,
            dest_uuid: Some(DEST_UUID),
        };
        let error = build_plan(&fx.request(), &probe).expect_err("plan refused");
        assert!(format!("{error:#}").contains("refusing"), "{error:#}");
        assert!(!fx.plan_path.exists());
    }

    let planned = fx.plan();
    for (source_disk, dest_disk, dest_uuid) in [
        (Some("disk14"), Some("disk14"), Some(DEST_UUID)),
        (None, Some("disk14"), Some(DEST_UUID)),
        // Another drive mounted at the same path, even one that took the
        // planned drive's BSD name.
        (Some("disk3"), Some("disk14"), Some("OTHER-VOLUME")),
        (Some("disk3"), Some("disk14"), None),
    ] {
        let probe = FakeProbe {
            source: fx.source.clone(),
            source_disk,
            dest_disk,
            dest_uuid,
        };
        let request = ApplyRequest {
            plan: fx.plan_path.clone(),
            token: planned.token.clone(),
            catalog: fx.catalog.clone(),
            app_catalog: None,
            progress: false,
        };
        apply(&request, &probe).expect_err("apply refused");
        assert!(!fx.dest.join("Backups").exists());
    }
}

#[test]
fn a_replugged_destination_with_a_new_disk_name_is_still_the_planned_drive() {
    let fx = Fixture::new();
    let planned = fx.plan();
    let probe = FakeProbe {
        source: fx.source.clone(),
        source_disk: Some("disk3"),
        dest_disk: Some("disk15"),
        dest_uuid: Some(DEST_UUID),
    };
    let request = ApplyRequest {
        plan: fx.plan_path.clone(),
        token: planned.token.clone(),
        catalog: fx.catalog.clone(),
        app_catalog: None,
        progress: false,
    };
    let report = apply(&request, &probe).expect("same volume UUID, new BSD name");
    assert!(report.complete(), "{}", apply::text(&report));
}

#[test]
fn a_plan_without_a_destination_volume_uuid_is_refused() {
    let fx = Fixture::new();
    let probe = FakeProbe {
        dest_uuid: None,
        ..fx.probe()
    };
    let error = build_plan(&fx.request(), &probe).expect_err("refused");
    assert!(format!("{error:#}").contains("volume UUID"), "{error:#}");
    assert!(!fx.plan_path.exists());
}

#[test]
fn a_catalog_or_plan_file_inside_the_source_is_refused() {
    let fx = Fixture::new();
    let before = fingerprint(&fx.source);

    let mut request = fx.request();
    request.catalog = fx.source.join("Library/rdirstat/catalog");
    let error = build_plan(&request, &fx.probe()).expect_err("catalog refused");
    assert!(format!("{error:#}").contains("inside the source"), "{error:#}");

    let mut request = fx.request();
    request.out = Some(fx.source.join("Documents/report.pdf"));
    let error = build_plan(&request, &fx.probe()).expect_err("plan file refused");
    assert!(format!("{error:#}").contains("--out"), "{error:#}");
    assert_eq!(before, fingerprint(&fx.source), "nothing written under the source");

    // The app's own catalog directory is the documented carve-out.
    let app = fx.source.join("Library/Application Support/app/catalog");
    let mut request = fx.request();
    request.catalog = app.clone();
    request.out = None;
    request.app_catalog = Some(app.clone());
    let planned = build_plan(&request, &fx.probe()).expect("carve-out allowed");
    assert!(planned.plan_path.starts_with(&app));

    // Apply refuses a catalog under the source, before anything is copied.
    let planned = fx.plan();
    let request = ApplyRequest {
        plan: fx.plan_path.clone(),
        token: planned.token.clone(),
        catalog: fx.source.join("cat"),
        app_catalog: None,
        progress: false,
    };
    let error = apply(&request, &fx.probe()).expect_err("apply refused");
    assert!(format!("{error:#}").contains("inside the source"), "{error:#}");
    assert!(!fx.dest.join("Backups").exists());
    assert!(!fx.source.join("cat").exists());
}

#[test]
fn a_receipted_copy_that_changed_on_the_destination_is_not_already_landed() {
    let fx = Fixture::new();
    let planned = fx.plan();
    assert!(fx.apply(&planned.token).expect("apply").complete());

    // Truncated on the destination after its receipt.
    std::fs::write(fx.mirror("Movies/big.bin"), b"short").expect("truncate");
    // A package that lost a file.
    std::fs::remove_file(fx.mirror("Applications/Tool.app/Contents/Info.plist")).expect("rm");

    let again = fx.apply(&planned.token).expect("re-apply");
    assert!(!again.complete(), "{}", apply::text(&again));
    assert_eq!(outcome_of(&again, "Movies/big.bin"), Outcome::DestinationOccupied);
    assert_eq!(
        outcome_of(&again, "Applications/Tool.app"),
        Outcome::DestinationOccupied
    );
    assert_eq!(outcome_of(&again, "Documents/report.pdf"), Outcome::AlreadyLanded);
    assert_eq!(
        std::fs::read(fx.mirror("Movies/big.bin")).expect("read"),
        b"short",
        "left alone"
    );
}

#[test]
fn receipts_still_match_after_the_source_device_number_changes() {
    let fx = Fixture::new();
    let planned = fx.plan();
    assert!(fx.apply(&planned.token).expect("apply").complete());

    // Rewrite the drive's receipts as if the source volume had been mounted
    // under another st_dev when they were taken (a replug or a reboot).
    let mirror = fx.dest.join(".rdirstat/catalog");
    let mut rows = read_receipts(&mirror).expect("read");
    for entry in std::fs::read_dir(&mirror).expect("ls") {
        let path = entry.expect("entry").path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("receipts-"))
        {
            std::fs::remove_file(path).expect("rm");
        }
    }
    for row in &mut rows {
        row.source_dev += 1000;
    }
    write_receipts(&rows, &mirror, "earlier").expect("rewrite");

    let again = fx.apply(&planned.token).expect("re-apply");
    assert!(again.complete(), "{}", apply::text(&again));
    assert!(again.results.iter().all(|r| r.outcome == Outcome::AlreadyLanded));
}

#[test]
fn receipts_for_another_drive_at_the_same_path_are_not_trusted() {
    let fx = Fixture::new();
    let planned = fx.plan();
    assert!(fx.apply(&planned.token).expect("apply").complete());

    // Another drive with the same files, mounted where this one was: its own
    // catalog is empty. The primary still holds this drive's receipts.
    std::fs::remove_dir_all(fx.dest.join(".rdirstat")).expect("rm mirror");
    assert!(!read_receipts(&fx.catalog).expect("primary").is_empty());

    let again = fx.apply(&planned.token).expect("re-apply");
    assert!(!again.complete());
    assert!(
        again
            .results
            .iter()
            .filter(|r| r.files.is_none())
            .all(|r| r.outcome == Outcome::DestinationOccupied),
        "{}",
        apply::text(&again)
    );
    // A roll-up's files are not taken on a receipt either: each was compared
    // with its source before it was adopted.
    assert!(
        again
            .results
            .iter()
            .filter(|r| r.files.is_some())
            .all(|r| r.outcome == Outcome::AlreadyLanded && r.detail.as_deref().is_some_and(|d| d.contains("verified"))),
        "{}",
        apply::text(&again)
    );
}

#[test]
fn only_this_runs_catalog_parts_reach_the_drive() {
    let fx = Fixture::new();
    // Another source's backup to another drive, already in the primary.
    let other = ReceiptRow {
        run_id: "backup-tuf8tb-1".into(),
        source_label: "tuf8tb".into(),
        source_path: "/Volumes/tuf8tb/secret-name.bin".into(),
        source_dev: 5,
        source_ino: 5,
        size: 5,
        mtime_ns: 5,
        dest_path: "/Volumes/OtherDrive/Backups/tuf8tb/secret-name.bin".into(),
        method: "copy".into(),
        sha256: "0".repeat(64),
        verified_at_ms: 0,
    };
    write_receipts(&[other], &fx.catalog, "backup-tuf8tb-1").expect("seed");

    let planned = fx.plan();
    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
    let (_, mirror) = fx.receipts();
    assert_eq!(mirror.len(), planned.plan.items.len());
    assert!(mirror.iter().all(|r| r.source_label == "mac-data"), "{mirror:?}");
    for entry in std::fs::read_dir(fx.dest.join(".rdirstat/catalog")).expect("ls") {
        let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
        assert!(!name.contains("tuf8tb"), "{name} leaked onto the drive");
    }
}

#[test]
fn a_snapshot_plans_like_a_walk_and_catches_changes_since_it() {
    let fx = Fixture::new();
    let mut options = rdirstat_core::ScanOptions::default();
    options.apply_default_exclusions = false;
    let outcome = crate::engine::scan(&crate::engine::Config {
        root: fx.source.clone(),
        options,
        progress: false,
    })
    .expect("scan");
    let snapshot = fx.catalog.join("source.rdstat");
    let mut file = std::fs::File::create(&snapshot).expect("create");
    rdirstat_core::snapshot::write(&outcome.scan, &mut file).expect("write snapshot");
    drop(file);

    let walked = fx.plan();
    // Change one candidate's size after the snapshot was taken.
    put(&fx.source, "Documents/report.pdf", &bytes(2, 33 * 1024));

    let mut request = fx.request();
    request.source = None;
    request.snapshot = Some(snapshot);
    let planned = build_plan(&request, &fx.probe()).expect("plan from snapshot");
    assert!(planned.plan.source_snapshot.starts_with("rdstat:sha256:"));
    assert_eq!(planned.plan.source_root, fx.source);
    let from_snapshot: Vec<_> = planned.plan.items.iter().map(|i| i.relative.clone()).collect();
    let from_walk: Vec<_> = walked
        .plan
        .items
        .iter()
        .map(|i| i.relative.clone())
        .filter(|r| r != Path::new("Documents/report.pdf"))
        .collect();
    assert_eq!(from_snapshot, from_walk);
    assert!(
        planned
            .plan
            .skipped
            .iter()
            .any(|s| s.rule_id == RULE_CHANGED_SINCE_SCAN && s.relative == Path::new("Documents/report.pdf"))
    );
    assert!(planned.plan.items.iter().all(|i| i.ino != 0), "inodes come from lstat");

    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
}

#[test]
fn apply_leaves_an_index_and_status_reads_it_from_parquet_alone() {
    let fx = Fixture::new();
    let planned = fx.plan();
    assert!(planned.entries.is_ok(), "{:?}", planned.entries);
    let plan_id = plan_id(&planned.plan);
    assert!(
        !fx.dest.join(".rdirstat").exists() && !fx.dest.join("index.parquet").exists(),
        "planning still writes nothing on the destination"
    );

    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
    let index = report.index.as_ref().expect("index rebuilt by apply");
    // One row per landed file: each individual item, and each file of each
    // roll-up (the volume root's notes.txt and link-to-big here).
    let files: u64 = planned
        .plan
        .items
        .iter()
        .map(|item| item.rollup.as_ref().map_or(1, |r| r.files))
        .sum();
    assert_eq!(index.rows as u64, files);
    assert!(apply::text(&report).contains("Index: "));

    let rows = rdirstat_catalog::read_index(&fx.dest.join("index.parquet")).expect("index");
    assert_eq!(rows.len() as u64, files);
    let dupe = rows
        .iter()
        .find(|r| r.dest_path.ends_with("dupe-b.bin"))
        .expect("dupe row");
    assert_eq!(dupe.dup_count, 2, "both duplicate source paths counted");
    assert_eq!(dupe.ext.as_deref(), Some("bin"));
    assert!(dupe.category.is_some(), "category joined from the plan's scan entries");
    let sql = std::fs::read_to_string(fx.dest.join("index.sql")).expect("index.sql");
    for view in ["files", "receipts", "entries", "skipped", "dupes"] {
        assert!(sql.contains(&format!("VIEW {view} AS")), "{view}");
    }

    let catalog = fx.dest.join(".rdirstat/catalog");
    let names: Vec<String> = std::fs::read_dir(&catalog)
        .expect("mirror")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with(&format!("entries-{plan_id}-"))),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n.starts_with("plans-")), "{names:?}");

    let report = status::status(&fx.dest, None).expect("status");
    assert_eq!(report.sources.len(), 1);
    let source = &report.sources[0];
    assert_eq!(source.backed_up.items, files);
    assert!(source.cloned.items >= 1, "{source:?}");
    assert_eq!(source.pending.items, 0);
    assert!(source.skipped.items >= 1, "node_modules skipped: {source:?}");
    assert_eq!(source.last_plan.as_deref(), Some(plan_id.as_str()));
    assert_eq!(source.not_backed_up.map(|t| t.items), Some(0), "{source:?}");
    assert!(
        report.warnings.iter().any(|w| w.contains(".metadata_never_index")),
        "{:?}",
        report.warnings
    );
    let text = status::text(&report);
    assert!(text.contains("mac-data") && text.contains("clone savings"), "{text}");
    assert_eq!(status::json(&report)["sources"][0]["pending"]["items"], 0);

    // Parquet only: a landed file removed behind the catalog's back is still
    // reported, and `backup index` rebuilds the same rows without looking.
    std::fs::remove_file(fx.mirror("Movies/big.bin")).expect("remove");
    let again = status::status(&fx.dest, Some("mac-data")).expect("status");
    assert_eq!(again.sources[0].backed_up, source.backed_up);
    let rebuilt = rdirstat_catalog::rebuild_index(&fx.dest).expect("rebuild");
    assert_eq!(rebuilt.rows, rows.len());

    assert!(status::status(&fx.dest, Some("tuf8tb")).is_err(), "unknown label");
}

#[test]
fn status_counts_what_the_last_plan_left_behind() {
    let fx = Fixture::new();
    let planned = fx.plan();
    std::fs::write(fx.source.join("Documents/report.pdf"), bytes(9, 32 * 1024)).expect("rewrite");
    let report = fx.apply(&planned.token).expect("apply");
    assert_eq!(outcome_of(&report, "Documents/report.pdf"), Outcome::ChangedSinceScan);

    let status = status::status(&fx.dest, None).expect("status");
    let source = &status.sources[0];
    assert_eq!(source.pending.items, 1);
    assert_eq!(source.pending.bytes, 32 * 1024);
    let missing = source.not_backed_up.expect("scan entries on the drive");
    assert_eq!(missing.items, 1, "{source:?}");
    assert!(
        source.largest_not_backed_up[0].0.ends_with("Documents/report.pdf"),
        "{:?}",
        source.largest_not_backed_up
    );
}

#[test]
fn index_and_status_refuse_a_drive_without_a_catalog() {
    let fx = Fixture::new();
    assert!(rdirstat_catalog::rebuild_index(&fx.dest).is_err());
    let error = status::status(&fx.dest, None).expect_err("no catalog");
    assert!(error.to_string().contains("no catalog"), "{error:#}");
    assert!(!fx.dest.join("index.parquet").exists());
}

#[test]
fn sizes_parse() {
    assert_eq!(parse_size("100MiB"), Ok(100 << 20));
    assert_eq!(parse_size("100 mib"), Ok(100 << 20));
    assert_eq!(parse_size("1GiB"), Ok(1 << 30));
    assert_eq!(parse_size("1.5G"), Ok(3 << 29));
    assert_eq!(parse_size("5000"), Ok(5000));
    assert_eq!(parse_size("2MB"), Ok(2_000_000));
    assert!(parse_size("").is_err());
    assert!(parse_size("MiB").is_err());
    assert!(parse_size("10 parsecs").is_err());
    assert!(parse_size("99999999999TiB").is_err());
}

// ---------------------------------------------------------------------------
// Directory roll-ups (rds-fkk.11)
// ---------------------------------------------------------------------------

/// Nested small files, a project with skipped `node_modules` and `target`
/// subtrees, and one large file.
fn build_tree(root: &Path) {
    for i in 0..5_u8 {
        put(
            root,
            &format!("Users/a/Documents/notes/n{i}.txt"),
            &bytes(i, 100 + usize::from(i)),
        );
        put(
            root,
            &format!("Users/a/Documents/notes/deep/d{i}.txt"),
            &bytes(i + 10, 50),
        );
    }
    put(root, "Users/a/Documents/letter.txt", &bytes(20, 300));
    std::os::unix::fs::symlink("letter.txt", root.join("Users/a/Documents/latest")).expect("symlink");
    put(root, "Users/a/src/app/README.md", &bytes(21, 80));
    put(root, "Users/a/src/app/src/main.rs", &bytes(22, 400));
    put(root, "Users/a/src/app/src/lib/mod.rs", &bytes(23, 200));
    put(root, "Users/a/src/app/node_modules/pkg/index.js", &bytes(24, 900));
    put(root, "Users/a/src/app/target/debug/app", &bytes(25, 5000));
    put(root, "Users/a/Movies/big.mov", &bytes(26, 64 * 1024));
}

/// Every regular file and symlink under `root`, relative.
fn files_under(root: &Path) -> Vec<PathBuf> {
    fingerprint(root)
        .into_iter()
        .filter(|(_, v)| v.0 != 'd')
        .map(|(path, _)| path.strip_prefix(root).expect("under root").to_path_buf())
        .collect()
}

fn rollup_items(plan: &PlanFile) -> Vec<(String, rdirstat_organize::RollupScope, u64)> {
    plan.items
        .iter()
        .filter_map(|item| {
            item.rollup
                .as_ref()
                .map(|r| (item.relative.to_string_lossy().into_owned(), r.scope, r.files))
        })
        .collect()
}

fn coverage_total(outcome: &PlanOutcome) -> u64 {
    let c = outcome.rollups.as_ref().expect("roll-ups on").coverage;
    c.individual.files + c.rolled_up.files + c.landed.files + c.skipped.files + c.not_covered.files
}

#[test]
fn rollups_cover_every_small_file_and_never_a_skipped_one() {
    use rdirstat_organize::RollupScope::{Files, Tree};

    let fx = Fixture::with(build_tree);
    let before = fingerprint(&fx.source);
    let planned = fx.plan();

    let rollups = rollup_items(&planned.plan);
    // The highest clean directory is one tree item; a directory that holds a
    // skipped subtree is split: its loose files are a files item, its clean
    // subdirectory a tree item.
    for expected in [
        ("Users/a/Documents".to_owned(), Tree, 12),
        ("Users/a/src/app".to_owned(), Files, 1),
        ("Users/a/src/app/src".to_owned(), Tree, 2),
    ] {
        assert!(rollups.contains(&expected), "{expected:?} in {rollups:?}");
    }
    assert!(
        !rollups
            .iter()
            .any(|(r, _, _)| r.contains("node_modules") || r.contains("target")),
        "{rollups:?}"
    );
    assert!(
        !rollups.iter().any(|(r, _, _)| r == "Users/a/Documents/notes"),
        "only the highest directory: {rollups:?}"
    );
    let app = planned
        .plan
        .items
        .iter()
        .find(|i| i.relative == Path::new("Users/a/src/app"))
        .and_then(|i| i.rollup.as_ref())
        .expect("files roll-up");
    assert!(
        app.exclude.is_empty(),
        "skipped subtrees are directories, not loose files"
    );
    assert!(
        planned
            .plan
            .items
            .iter()
            .any(|i| i.relative == Path::new("Users/a/Movies/big.mov"))
    );

    // Nothing silently dropped: every file is somewhere.
    let all = files_under(&fx.source).len() as u64;
    assert_eq!(coverage_total(&planned), all);
    let summary = planned.rollups.as_ref().expect("roll-ups");
    assert_eq!(summary.coverage.skipped.files, 2, "node_modules and target");
    assert_eq!(summary.coverage.not_covered.files, 0);
    assert_eq!(planned.not_considered, 0);
    let text = plan::text(&planned, 5);
    assert!(text.contains("roll-ups ") && text.contains("whole trees"), "{text}");
    assert!(!text.contains("not ranked"), "{text}");
    let doc = plan::json(&planned, 5);
    assert_eq!(doc["rollups"]["items"], summary.planned_items);
    assert_eq!(doc["rollups"]["files"], summary.planned.files);

    let report = fx.apply(&planned.token).expect("apply");
    assert!(report.complete(), "{}", apply::text(&report));
    assert_eq!(before, fingerprint(&fx.source), "the source is unmodified");

    // Everything but the skipped subtrees is on the drive, byte for byte.
    let mirror_root = fx.mirror("");
    let mut landed = files_under(&mirror_root);
    landed.sort();
    let mut expected: Vec<PathBuf> = files_under(&fx.source)
        .into_iter()
        .filter(|p| {
            let p = p.to_string_lossy();
            !p.contains("node_modules") && !p.contains("/target/")
        })
        .collect();
    expected.sort();
    assert_eq!(landed, expected);
    assert!(!fx.mirror("Users/a/src/app/node_modules").exists());
    assert!(!fx.mirror("Users/a/src/app/target").exists());
    assert_eq!(
        std::fs::read_link(fx.mirror("Users/a/Documents/latest")).expect("link kept"),
        Path::new("letter.txt")
    );

    // One receipt per roll-up, one member row per file; the index and status
    // count files.
    let (_, mirror) = fx.receipts();
    let rollup_receipts = mirror.iter().filter(|r| r.method == "rollup").count();
    assert_eq!(rollup_receipts, rollups.len());
    let rolled_files: u64 = rollups.iter().map(|(_, _, n)| n).sum();
    let mut members = 0_u64;
    rdirstat_catalog::visit_members(&fx.dest.join(".rdirstat/catalog"), |_| members += 1).expect("members");
    assert_eq!(members, rolled_files);
    let index = report.index.as_ref().expect("index");
    assert_eq!(index.rows as u64, expected.len() as u64);
    let status = status::status(&fx.dest, None).expect("status");
    let source = &status.sources[0];
    assert_eq!(source.backed_up.items, expected.len() as u64);
    assert_eq!(source.pending.items, 0, "{source:?}");
    assert_eq!(source.not_backed_up.map(|t| t.items), Some(0), "{source:?}");

    // Re-applying copies and receipts nothing.
    let again = fx.apply(&planned.token).expect("re-apply");
    assert!(again.complete(), "{}", apply::text(&again));
    assert!(again.results.iter().all(|r| r.outcome == Outcome::AlreadyLanded));
    assert_eq!(again.receipts_written, 0);

    // A new plan finds everything on the drive and plans nothing.
    let replanned = fx.plan();
    assert!(replanned.plan.items.is_empty(), "{:?}", rollup_items(&replanned.plan));
    let c = replanned.rollups.as_ref().expect("roll-ups").coverage;
    assert_eq!(c.landed.files, expected.len() as u64);
    assert_eq!(coverage_total(&replanned), all);
    assert_eq!(before, fingerprint(&fx.source), "still unmodified");
}

#[test]
#[allow(clippy::too_many_lines, reason = "one scenario across two waves")]
fn a_partly_landed_directory_is_merged_without_overwriting_anything() {
    let fx = Fixture::with(build_tree);

    // Wave 1, without roll-ups: only big.mov, one item.
    let mut first = fx.request();
    first.rollup = false;
    first.floor = 32 * 1024;
    let wave1 = build_plan(&first, &fx.probe()).expect("plan");
    assert_eq!(wave1.plan.items.len(), 1);
    assert!(fx.apply(&wave1.token).expect("apply").complete());
    let landed_ino = std::fs::symlink_metadata(fx.mirror("Users/a/Movies/big.mov"))
        .expect("landed")
        .ino();

    // A new file beside the landed one.
    put(&fx.source, "Users/a/Movies/clip.mov", &bytes(27, 700));
    let before = fingerprint(&fx.source);

    // Leftovers on the drive: an exact copy with no receipt (a crashed run),
    // and a different file where a source file belongs.
    put(
        &fx.dest,
        "Backups/mac-data/Users/a/Documents/notes/n0.txt",
        &bytes(0, 100),
    );
    put(
        &fx.dest,
        "Backups/mac-data/Users/a/Documents/notes/n1.txt",
        b"not the source",
    );
    let occupied = fx.mirror("Users/a/Documents/notes/n1.txt");
    let occupied_before = (
        std::fs::read(&occupied).expect("read"),
        std::fs::symlink_metadata(&occupied).expect("lstat").ino(),
    );

    // Wave 2: everything is under the floor, so all of it rolls up.
    let mut second = fx.request();
    second.floor = 1 << 30;
    let wave2 = build_plan(&second, &fx.probe()).expect("plan");
    let summary = wave2.rollups.as_ref().expect("roll-ups");
    assert_eq!(
        summary.landed_candidates.files, 0,
        "big.mov is under this floor, not a candidate"
    );
    let rollups = rollup_items(&wave2.plan);
    assert!(
        rollups.iter().any(|(r, _, _)| r == "Users/a/Movies"),
        "a directory with a landed file still rolls up: {rollups:?}"
    );

    let report = fx.apply(&wave2.token).expect("apply");
    assert!(!report.complete(), "one file is occupied");
    assert_eq!(before, fingerprint(&fx.source), "the source is unmodified");

    // The landed file is the same inode: never re-copied or overwritten.
    assert_eq!(
        std::fs::symlink_metadata(fx.mirror("Users/a/Movies/big.mov"))
            .expect("still there")
            .ino(),
        landed_ino
    );
    let movies = report
        .results
        .iter()
        .find(|r| r.relative == "Users/a/Movies")
        .expect("movies");
    assert_eq!(movies.outcome, Outcome::Copied, "{movies:?}");
    assert!(
        movies
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("1 already receipted")),
        "{movies:?}"
    );
    assert_eq!(
        std::fs::read(fx.mirror("Users/a/Movies/clip.mov")).expect("landed"),
        bytes(27, 700)
    );

    // The different file is left alone and reported; the rest of the
    // directory landed around it, and the exact leftover was adopted.
    assert_eq!(
        (
            std::fs::read(&occupied).expect("read"),
            std::fs::symlink_metadata(&occupied).expect("lstat").ino()
        ),
        occupied_before
    );
    let documents = report
        .results
        .iter()
        .find(|r| r.relative == "Users/a/Documents")
        .expect("documents");
    assert_eq!(documents.outcome, Outcome::DestinationOccupied, "{documents:?}");
    assert!(
        documents.detail.as_deref().is_some_and(|d| d.contains("n1.txt")),
        "{documents:?}"
    );
    for i in 2..5 {
        assert_eq!(
            std::fs::read(fx.mirror(&format!("Users/a/Documents/notes/n{i}.txt"))).expect("landed"),
            bytes(i, 100 + usize::from(i))
        );
    }
    let (_, mirror) = fx.receipts();
    assert!(
        !mirror
            .iter()
            .any(|r| r.method == "rollup" && r.dest_path.ends_with("Users/a/Documents")),
        "no roll-up receipt for an incomplete directory"
    );
    let mut member_paths = Vec::new();
    rdirstat_catalog::visit_members(&fx.dest.join(".rdirstat/catalog"), |m| {
        member_paths.push(m.dest_path.to_owned());
    })
    .expect("members");
    assert!(
        member_paths.iter().any(|p| p.ends_with("notes/n0.txt")),
        "adopted copy recorded"
    );
    assert!(
        !member_paths.iter().any(|p| p.ends_with("notes/n1.txt")),
        "occupied path not recorded"
    );
    assert!(
        !member_paths.iter().any(|p| p.ends_with("big.mov")),
        "already receipted, not recorded again"
    );
    assert!(member_paths.iter().any(|p| p.ends_with("clip.mov")));
}

#[test]
fn a_file_changed_after_the_plan_skips_its_rollup_and_copies_nothing_of_it() {
    let fx = Fixture::with(build_tree);
    let planned = fx.plan();
    // Same size, new bytes and mtime, deep inside a tree roll-up.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    put(&fx.source, "Users/a/Documents/notes/deep/d3.txt", &bytes(99, 50));
    let before = fingerprint(&fx.source);

    let report = fx.apply(&planned.token).expect("apply");
    assert!(!report.complete());
    let documents = report
        .results
        .iter()
        .find(|r| r.relative == "Users/a/Documents")
        .expect("documents");
    assert_eq!(documents.outcome, Outcome::ChangedSinceScan, "{documents:?}");
    assert!(
        !fx.mirror("Users/a/Documents").exists(),
        "never copied in its new state"
    );
    assert!(
        fx.mirror("Users/a/src/app/src/main.rs").exists(),
        "other roll-ups still land"
    );
    let (_, mirror) = fx.receipts();
    assert!(!mirror.iter().any(|r| r.dest_path.contains("Users/a/Documents")));
    assert_eq!(before, fingerprint(&fx.source), "the source is unmodified");
}

#[test]
fn a_new_file_after_the_plan_is_a_change_not_a_silent_extra() {
    let fx = Fixture::with(build_tree);
    let planned = fx.plan();
    put(&fx.source, "Users/a/src/app/src/new.rs", b"fn main() {}");
    let report = fx.apply(&planned.token).expect("apply");
    let src = report
        .results
        .iter()
        .find(|r| r.relative == "Users/a/src/app/src")
        .expect("src");
    assert_eq!(src.outcome, Outcome::ChangedSinceScan, "{src:?}");
    assert!(!fx.mirror("Users/a/src/app/src").exists());
}

#[test]
fn rollup_rows_are_bound_by_the_token() {
    let fx = Fixture::with(build_tree);
    let planned = fx.plan();
    let mut plan: PlanFile = serde_json::from_slice(&std::fs::read(&fx.plan_path).expect("read")).expect("parse");
    let row = plan
        .items
        .iter_mut()
        .find_map(|i| i.rollup.as_mut())
        .expect("a roll-up row");
    row.files += 1;
    std::fs::write(&fx.plan_path, serde_json::to_vec(&plan).expect("encode")).expect("write");
    let error = fx.apply(&planned.token).expect_err("refused");
    assert!(error.to_string().contains("token"), "{error:#}");
    assert!(!fx.dest.join("Backups").exists());
}

#[test]
fn no_rollup_plans_exactly_as_before() {
    let fx = Fixture::with(build_tree);
    let mut request = fx.request();
    request.rollup = false;
    let planned = build_plan(&request, &fx.probe()).expect("plan");
    assert!(planned.rollups.is_none());
    assert!(
        planned
            .plan
            .items
            .iter()
            .all(|i| i.rollup.is_none() && i.kind != rdirstat_organize::ItemKind::Dir)
    );
    assert_eq!(planned.plan.items.len(), 1, "only big.mov is over the floor");
    let json = std::fs::read_to_string(&fx.plan_path).expect("plan file");
    assert!(!json.contains("\"rollup\""), "no roll-up keys in the plan file");
}
