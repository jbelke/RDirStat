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
}

impl DiskProbe for FakeProbe {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let on_source = path.starts_with(&self.source);
        Ok(DiskFacts {
            disk_id: if on_source { self.source_disk } else { self.dest_disk }.map(str::to_owned),
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
        let src = tempfile::tempdir().expect("tempdir");
        let dst = tempfile::tempdir().expect("tempdir");
        let cat = tempfile::tempdir().expect("tempdir");
        let source_root = std::fs::canonicalize(src.path()).expect("canonical");
        let dest_root = std::fs::canonicalize(dst.path()).expect("canonical");
        let catalog = std::fs::canonicalize(cat.path()).expect("canonical");
        build_source(&source_root);
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
            progress: false,
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
fn a_failed_compare_leaves_a_named_partial_and_no_receipt() {
    let fx = Fixture::new();
    let planned = fx.plan();
    let before = fingerprint(&fx.source);

    // A stale catalog row: it claims `stale.bin` holds big.bin's bytes, but it
    // holds other bytes of the same length. Apply clones it, the compare
    // against the source fails, and the clone must not be receipted.
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
    assert!(!report.complete());
    let big = report
        .results
        .iter()
        .find(|r| r.relative == "Movies/big.bin")
        .expect("in report");
    assert_eq!(big.outcome, Outcome::Failed, "{big:?}");
    let target = fx.mirror("Movies/big.bin");
    assert_eq!(big.partial.as_deref(), Some(target.to_string_lossy().as_ref()));
    assert!(target.exists(), "the partial is kept, not deleted");
    assert!(apply::text(&report).contains("left on disk at"));

    let (primary, mirror) = fx.receipts();
    let landed = target.to_string_lossy();
    for rows in [&primary, &mirror] {
        assert!(
            !rows.iter().any(|r| r.dest_path == landed),
            "no receipt for a destination that failed compare"
        );
    }
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
        };
        let error = build_plan(&fx.request(), &probe).expect_err("plan refused");
        assert!(format!("{error:#}").contains("refusing"), "{error:#}");
        assert!(!fx.plan_path.exists());
    }

    let planned = fx.plan();
    for (source_disk, dest_disk) in [
        (Some("disk14"), Some("disk14")),
        (None, Some("disk14")),
        (Some("disk3"), Some("disk15")),
    ] {
        let probe = FakeProbe {
            source: fx.source.clone(),
            source_disk,
            dest_disk,
        };
        let request = ApplyRequest {
            plan: fx.plan_path.clone(),
            token: planned.token.clone(),
            catalog: fx.catalog.clone(),
            progress: false,
        };
        apply(&request, &probe).expect_err("apply refused");
        assert!(!fx.dest.join("Backups").exists());
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
