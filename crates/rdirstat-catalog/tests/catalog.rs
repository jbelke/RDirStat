//! Round trips, naming, and the entries walk, all in temp dirs.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a test that cannot build its fixture has already failed"
)]

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, BooleanArray, StringArray, UInt64Array};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rdirstat_catalog::{
    Catalog, CatalogError, HashRow, ReceiptRow, dest_catalog_dir, read_hashes, read_manifests, read_receipts,
    write_entries, write_hashes, write_receipts,
};
use rdirstat_core::{
    CategoryId, CompletedScan, ConfigHash, DirTotals, DisplayPath, Kind, Node, ScanCounts, ScanId, ScanOptions,
    ScanTotals, TreeBuilder, TreeGeneration, VolumeId, flags,
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn hash(path: &str, ino: u64, sha: &str) -> HashRow {
    HashRow {
        path: path.to_owned(),
        dev: 16_777_232,
        ino,
        size: 1 << 40,
        mtime_ns: -1_500_000_000,
        sha256: sha.to_owned(),
    }
}

fn receipt(method: &str) -> ReceiptRow {
    ReceiptRow {
        run_id: "run1".to_owned(),
        source_label: "mac-data".to_owned(),
        source_path: "/Users/me/naïve ☕.mov".to_owned(),
        source_dev: 1,
        source_ino: u64::MAX,
        size: 123,
        mtime_ns: i64::MAX,
        dest_path: "/Volumes/dest/Backups/mac-data/Users/me/naïve ☕.mov".to_owned(),
        method: method.to_owned(),
        sha256: SHA_B.to_owned(),
        verified_at_ms: 1_764_000_000_000,
    }
}

/// `(path_bytes, path, path_lossy, kind, logical, ext)`.
type EntryRow = (Vec<u8>, String, bool, String, u64, Option<String>);

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_str().unwrap().to_owned()
}

fn batches(path: &Path) -> Vec<RecordBatch> {
    ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn hashes_round_trip_across_parts() {
    let dir = tempfile::tempdir().unwrap();
    let first = vec![hash("/a", 1, SHA_A), hash("/b", 2, SHA_B)];
    let second = vec![hash("/c", 3, SHA_A)];
    write_hashes(&first, dir.path(), "run1").unwrap();
    write_hashes(&second, dir.path(), "run2").unwrap();

    let back = read_hashes(dir.path()).unwrap();
    let mut expected = first;
    expected.extend(second);
    assert_eq!(back, expected);
}

#[test]
fn receipts_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let rows = vec![receipt("copy"), receipt("clone")];
    let path = write_receipts(&rows, dir.path(), "run1").unwrap();
    assert_eq!(file_name(&path), "receipts-run1-0000.parquet");
    assert_eq!(read_receipts(dir.path()).unwrap(), rows);
    // Kinds do not bleed into each other.
    assert!(read_hashes(dir.path()).unwrap().is_empty());
}

#[test]
fn missing_directory_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_hashes(&dir.path().join("absent")).unwrap().is_empty());
    assert!(read_receipts(&dir.path().join("absent")).unwrap().is_empty());
}

#[test]
fn bad_rows_are_refused_before_any_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cat");
    assert!(matches!(
        write_hashes(&[hash("/a", 1, "ABC")], &out, "r"),
        Err(CatalogError::InvalidRow { index: 0, .. })
    ));
    assert!(matches!(
        write_receipts(&[receipt("copy"), receipt("move")], &out, "r"),
        Err(CatalogError::InvalidRow { index: 1, .. })
    ));
    assert!(matches!(
        write_hashes(&[], &out, "../x"),
        Err(CatalogError::InvalidRunId(_))
    ));
    assert!(!out.exists());
}

#[test]
fn writes_never_overwrite_and_take_the_next_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_hashes(&[hash("/a", 1, SHA_A)], dir.path(), "run1").unwrap();
    let b = write_hashes(&[hash("/b", 2, SHA_A)], dir.path(), "run1").unwrap();
    let other = write_hashes(&[hash("/c", 3, SHA_A)], dir.path(), "run2").unwrap();
    assert_eq!(file_name(&a), "hashes-run1-0000.parquet");
    assert_eq!(file_name(&b), "hashes-run1-0001.parquet");
    assert_eq!(file_name(&other), "hashes-run2-0000.parquet");

    // A gap in the sequence still moves forward past the highest number.
    std::fs::rename(&b, dir.path().join("hashes-run1-0005.parquet")).unwrap();
    let c = write_hashes(&[hash("/d", 4, SHA_A)], dir.path(), "run1").unwrap();
    assert_eq!(file_name(&c), "hashes-run1-0006.parquet");

    // The first file still holds exactly what it was written with.
    let first: Vec<_> = read_hashes(dir.path()).unwrap().into_iter().map(|r| r.path).collect();
    assert_eq!(first, ["/a", "/b", "/d", "/c"]);

    // No temp file is left behind.
    let hidden = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(hidden, 0);
}

#[test]
fn files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = write_hashes(&[hash("/a", 1, SHA_A)], dir.path(), "r").unwrap();
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// `/Volumes/src` with `media/clip.mkv`, a non-UTF-8 file, a symlink, and
/// `readme.TXT`. The root is interned as a bare name, as core's own tests do,
/// to prove absolute paths come from `root_path`.
fn scan() -> CompletedScan {
    let mut builder = TreeBuilder::new();
    let root_name = builder.intern(b"root").unwrap();
    let root = builder.push_node(Node::directory(root_name, 10)).unwrap();
    builder.register_directory(root, DirTotals::EMPTY).unwrap();

    let media_name = builder.intern(b"media").unwrap();
    let media = builder.push_child(root, Node::directory(media_name, 20)).unwrap();
    builder.register_directory(media, DirTotals::EMPTY).unwrap();

    let leaf = |builder: &mut TreeBuilder, parent, name: &[u8], node: fn(rdirstat_core::NameRef) -> Node| {
        let interned = builder.intern(name).unwrap();
        let node = node(interned);
        builder.push_child(parent, node).unwrap();
        if node.kind() == Kind::File
            && let Some(totals) = builder.dir_totals_mut(parent)
        {
            totals.absorb_direct_file(node.contributed_size(), node.contributed_alloc(), node.mtime);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }
    };
    leaf(&mut builder, media, b"clip.MKV", |n| {
        Node::leaf(n, Kind::File, 4096, 8192, 30)
            .with_flags(flags::HARD_LINK)
            .with_category(CategoryId::from_raw(3))
    });
    leaf(&mut builder, media, b"bad\xff.bin", |n| {
        Node::leaf(n, Kind::File, 7, 4096, -5)
    });
    leaf(&mut builder, media, b"link", |n| Node::leaf(n, Kind::Symlink, 0, 0, 1));
    leaf(&mut builder, root, b"readme.TXT", |n| {
        Node::leaf(n, Kind::File, 100, 4096, 40)
    });

    builder.rollup().unwrap();
    let tree = builder.finish().unwrap();
    CompletedScan {
        scan_id: ScanId::from_raw(7),
        generation: TreeGeneration::from_raw(3),
        root_path: PathBuf::from("/Volumes/src"),
        root,
        volume: VolumeId {
            device: 16_777_232,
            fs_type: "apfs".to_owned(),
            volume_uuid: None,
            mount_point: DisplayPath::from_bytes(b"/Volumes/src"),
            case_preserving: true,
            case_sensitive: false,
        },
        started_unix_ms: 1_700_000_000_000,
        finished_unix_ms: 1_700_000_191_000,
        options: ScanOptions::default(),
        exclusion_hash: ConfigHash::from_hex("abc".to_owned()),
        category_config_hash: ConfigHash::from_hex("def".to_owned()),
        tool_version: "test".to_owned(),
        counts: ScanCounts::default(),
        totals: ScanTotals {
            logical: 4203,
            allocated: 16_384,
        },
        mutations: 0,
        errors: Vec::new(),
        error_counts: Vec::new(),
        excluded_roots: Vec::new(),
        tree,
    }
}

#[test]
fn entries_cover_every_node_with_absolute_paths() {
    let dir = tempfile::tempdir().unwrap();
    let scan = scan();
    let path = write_entries(&scan, "src", dir.path(), "run1").unwrap();
    assert_eq!(file_name(&path), "entries-run1-0000.parquet");

    let batches = batches(&path);
    let mut rows = Vec::new();
    for b in &batches {
        let col = |n: &str| b.column_by_name(n).unwrap();
        let paths = col("path").as_any().downcast_ref::<StringArray>().unwrap();
        let raw = col("path_bytes").as_any().downcast_ref::<BinaryArray>().unwrap();
        let lossy = col("path_lossy").as_any().downcast_ref::<BooleanArray>().unwrap();
        let kind = col("kind").as_any().downcast_ref::<StringArray>().unwrap();
        let logical = col("logical").as_any().downcast_ref::<UInt64Array>().unwrap();
        let ext = col("ext").as_any().downcast_ref::<StringArray>().unwrap();
        let dev = col("dev").as_any().downcast_ref::<UInt64Array>().unwrap();
        let label = col("source_label").as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(col("ino").null_count(), b.num_rows());
        for i in 0..b.num_rows() {
            assert_eq!(dev.value(i), 16_777_232);
            assert_eq!(label.value(i), "src");
            rows.push((
                raw.value(i).to_vec(),
                paths.value(i).to_owned(),
                lossy.value(i),
                kind.value(i).to_owned(),
                logical.value(i),
                ext.is_valid(i).then(|| ext.value(i).to_owned()),
            ));
        }
    }
    rows.sort();
    let expect: Vec<EntryRow> = vec![
        (
            b"/Volumes/src".to_vec(),
            "/Volumes/src".into(),
            false,
            "dir".into(),
            0,
            None,
        ),
        (
            b"/Volumes/src/media".to_vec(),
            "/Volumes/src/media".into(),
            false,
            "dir".into(),
            0,
            None,
        ),
        (
            b"/Volumes/src/media/bad\xff.bin".to_vec(),
            "/Volumes/src/media/bad\u{fffd}.bin".into(),
            true,
            "file".into(),
            7,
            Some("bin".into()),
        ),
        (
            b"/Volumes/src/media/clip.MKV".to_vec(),
            "/Volumes/src/media/clip.MKV".into(),
            false,
            "file".into(),
            4096,
            Some("mkv".into()),
        ),
        (
            b"/Volumes/src/media/link".to_vec(),
            "/Volumes/src/media/link".into(),
            false,
            "symlink".into(),
            0,
            None,
        ),
        (
            b"/Volumes/src/readme.TXT".to_vec(),
            "/Volumes/src/readme.TXT".into(),
            false,
            "file".into(),
            100,
            Some("txt".into()),
        ),
    ];
    assert_eq!(rows, expect);
    assert_eq!(rows.len(), scan.tree.len());
}

#[test]
fn entries_under_a_filesystem_root_have_one_slash() {
    let dir = tempfile::tempdir().unwrap();
    let mut scan = scan();
    scan.root_path = PathBuf::from("/");
    let path = write_entries(&scan, "boot", dir.path(), "r").unwrap();
    let b = &batches(&path)[0];
    let paths = b
        .column_by_name("path")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let mut all: Vec<_> = paths.iter().flatten().collect();
    all.sort_unstable();
    assert_eq!(all.first().copied(), Some("/"));
    assert!(all.contains(&"/media/clip.MKV"));
    assert!(all.iter().all(|p| !p.contains("//")));
}

#[test]
fn catalog_mirrors_every_part_byte_for_byte_under_the_drive() {
    let app = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let catalog = Catalog::for_destination(app.path().join("catalog"), dest.path());
    let mirror_dir = dest.path().join(".rdirstat").join("catalog");
    assert_eq!(catalog.mirror(), Some(mirror_dir.as_path()));
    assert_eq!(dest_catalog_dir(dest.path()), mirror_dir);

    let h = catalog.write_hashes(&[hash("/a", 1, SHA_A)], "run1").unwrap();
    let r = catalog.write_receipts(&[receipt("copy")], "run1").unwrap();
    let e = catalog.write_entries(&scan(), "src", "run1").unwrap();
    for w in [&h, &r, &e] {
        let mirror = w.mirror.as_ref().unwrap();
        assert_eq!(mirror.parent().unwrap(), mirror_dir);
        assert_eq!(file_name(mirror), file_name(&w.primary));
        assert_eq!(std::fs::read(mirror).unwrap(), std::fs::read(&w.primary).unwrap());
    }
    assert_eq!(
        read_hashes(&mirror_dir).unwrap(),
        read_hashes(catalog.primary()).unwrap()
    );
    assert_eq!(read_receipts(&mirror_dir).unwrap(), vec![receipt("copy")]);
    assert!(catalog.sync_mirror("run1").unwrap().is_empty());
}

#[test]
fn manifest_lists_the_runs_parts_with_rows_size_and_sha() {
    let app = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let catalog = Catalog::for_destination(app.path(), dest.path());
    catalog
        .write_hashes(&[hash("/a", 1, SHA_A), hash("/b", 2, SHA_B)], "run1")
        .unwrap();
    catalog.write_receipts(&[receipt("clone")], "run1").unwrap();
    catalog.write_hashes(&[hash("/c", 3, SHA_A)], "run2").unwrap();

    let m = catalog.write_manifest("run1", 42).unwrap();
    assert_eq!(file_name(&m.primary), "manifest-run1-0000.parquet");
    let rows = read_manifests(&dest_catalog_dir(dest.path())).unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|r| (r.kind.as_str(), r.file_name.as_str(), r.rows))
        .collect();
    assert_eq!(
        names,
        [
            ("hashes", "hashes-run1-0000.parquet", 2),
            ("receipts", "receipts-run1-0000.parquet", 1)
        ]
    );
    for row in &rows {
        let bytes = std::fs::read(app.path().join(&row.file_name)).unwrap();
        assert_eq!(row.bytes, bytes.len() as u64);
        assert_eq!(row.sha256.len(), 64);
        assert_eq!(row.recorded_at_ms, 42);
        assert_eq!(row.run_id, "run1");
    }
    assert_ne!(rows[0].sha256, rows[1].sha256);
}

#[test]
fn mirror_never_overwrites_and_sync_fills_gaps() {
    let app = tempfile::tempdir().unwrap();
    let mirror = tempfile::tempdir().unwrap();
    let mirror_dir = mirror.path().join("m");

    // Written while the drive was away: primary only.
    let offline = Catalog::new(app.path());
    let first = write_hashes(&[hash("/a", 1, SHA_A)], app.path(), "run1").unwrap();
    assert_eq!(offline.mirror(), None);
    assert!(offline.sync_mirror("run1").unwrap().is_empty());

    let catalog = Catalog::with_mirror(app.path(), &mirror_dir);
    let created = catalog.sync_mirror("run1").unwrap();
    assert_eq!(created, [mirror_dir.join(file_name(&first))]);

    // A different file already under the next name in the mirror.
    std::fs::write(mirror_dir.join("hashes-run1-0001.parquet"), b"not ours").unwrap();
    let err = catalog.write_hashes(&[hash("/b", 2, SHA_A)], "run1").unwrap_err();
    let CatalogError::Mirror { primary, source } = err else {
        panic!("expected Mirror, got {err:?}");
    };
    assert!(primary.exists());
    assert!(matches!(*source, CatalogError::MirrorConflict { .. }));
    assert_eq!(
        std::fs::read(mirror_dir.join("hashes-run1-0001.parquet")).unwrap(),
        b"not ours"
    );
    let hidden = std::fs::read_dir(&mirror_dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(hidden, 0);
}

#[test]
fn sync_mirrors_only_the_named_run() {
    let app = tempfile::tempdir().unwrap();
    let drive_a = tempfile::tempdir().unwrap();
    let drive_b = tempfile::tempdir().unwrap();

    // An earlier backup of another source to another drive.
    let to_a = Catalog::for_destination(app.path(), drive_a.path());
    to_a.write_receipts(&[receipt("copy")], "backup-mac-data-1").unwrap();
    to_a.write_manifest("backup-mac-data-1", 1).unwrap();
    // Drive A's mirror has drifted from the primary under another run's name.
    std::fs::write(dest_catalog_dir(drive_a.path()).join("hashes-old-0000.parquet"), b"x").unwrap();
    write_hashes(&[hash("/old", 1, SHA_A)], app.path(), "old").unwrap();

    // This run, to drive B, wrote its parts while drive B was away.
    write_hashes(&[hash("/b", 2, SHA_B)], app.path(), "backup-tuf8tb-12").unwrap();
    write_hashes(&[hash("/c", 3, SHA_B)], app.path(), "backup-tuf8tb-1").unwrap();
    let to_b = Catalog::for_destination(app.path(), drive_b.path());
    let created = to_b.sync_mirror("backup-tuf8tb-1").unwrap();
    let names: Vec<_> = created.iter().map(|p| file_name(p)).collect();
    assert_eq!(names, ["hashes-backup-tuf8tb-1-0000.parquet"]);
    let on_b: Vec<_> = std::fs::read_dir(dest_catalog_dir(drive_b.path()))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".parquet"))
        .collect();
    assert_eq!(
        on_b,
        ["hashes-backup-tuf8tb-1-0000.parquet"],
        "nothing from mac-data or drive A"
    );

    // An unrelated conflict on drive A does not fail a run that did not write it.
    to_a.sync_mirror("backup-mac-data-1").unwrap();
    assert!(to_a.sync_mirror("old").is_err());
}
