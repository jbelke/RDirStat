#![allow(clippy::expect_used, clippy::panic)]

//! Policy that cannot be created in a `TempDir`.
//!
//! Mounting a filesystem and setting `SF_FIRMLINK` both need privileges, so
//! those two rules are tested through a synthetic [`DirReader`] instead. The
//! reader is the only thing faked: the builder, the schedulers, the exclusion
//! set, and the rollup are the production ones, which is exactly the seam the
//! reader trait exists to provide.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rdirstat_core::{ErrorClass, Kind, Operation, ScanError, ScanOptions, flags};
use rdirstat_scan::{
    CancelToken, DirHandle, DirReader, Engine, RawEntry, RawEntryBatch, ReadDirError, SF_FIRMLINK, ScanOutcome, Scanner,
};

#[derive(Debug, Default)]
struct FakeReader {
    listings: HashMap<PathBuf, Vec<RawEntry>>,
    reads: AtomicUsize,
    cancel_after: Option<(usize, CancelToken)>,
}

impl FakeReader {
    fn new() -> Self {
        Self::default()
    }

    fn listing(mut self, path: PathBuf, entries: Vec<RawEntry>) -> Self {
        self.listings.insert(path, entries);
        self
    }

    fn cancel_after(mut self, reads: usize, cancel: CancelToken) -> Self {
        self.cancel_after = Some((reads, cancel));
        self
    }
}

impl DirReader for FakeReader {
    fn read_batches(
        &self,
        dir: &DirHandle,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(RawEntryBatch<'_>) -> ControlFlow<ScanError>,
    ) -> Result<(), ReadDirError> {
        if cancel.is_cancelled() {
            return Err(ReadDirError::Cancelled);
        }
        let seen = self.reads.fetch_add(1, Ordering::Relaxed);
        if let Some((after, token)) = &self.cancel_after
            && seen + 1 >= *after
        {
            token.cancel();
        }
        let Some(entries) = self.listings.get(dir.path()) else {
            return Err(ReadDirError::Os {
                operation: Operation::OpenDir,
                class: ErrorClass::NotFound,
                os_code: 2,
            });
        };
        match sink(RawEntryBatch::new(entries, &[])) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(error) => Err(ReadDirError::Aborted(error)),
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

fn entry(name: &[u8], kind: Kind, dev: u64, ino: u64) -> RawEntry {
    RawEntry {
        name: rdirstat_scan::SmallName::from_bytes(name),
        kind,
        size: if kind == Kind::File { 100 } else { 0 },
        alloc: if kind == Kind::File { 4_096 } else { 0 },
        mtime: 1_700_000_000,
        dev,
        ino,
        nlink: 1,
        mode: 0o644,
        platform_flags: 0,
    }
}

fn root_device(root: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::dev(&std::fs::metadata(root).expect("stat"))
}

/// The scanner canonicalizes its root, and on macOS `/var/folders/...` resolves
/// through `/private`. The fake listings are keyed by the resolved path.
fn canonical(temp: &tempfile::TempDir) -> PathBuf {
    std::fs::canonicalize(temp.path()).expect("canonical root")
}

/// A root whose listing holds `same` (this volume) and `other` (a mount).
fn mount_fixture(root: &Path, device: u64) -> FakeReader {
    let other_device = device.wrapping_add(1);
    FakeReader::new()
        .listing(
            root.to_path_buf(),
            vec![
                entry(b"same", Kind::Directory, device, 10),
                entry(b"mounted", Kind::Directory, other_device, 11),
            ],
        )
        .listing(root.join("same"), vec![entry(b"here.txt", Kind::File, device, 12)])
        .listing(
            root.join("mounted"),
            vec![entry(b"elsewhere.txt", Kind::File, other_device, 13)],
        )
}

#[test]
fn a_device_boundary_is_marked_and_not_crossed_by_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = canonical(&temp);
    let device = root_device(&root);
    let reader = Arc::new(mount_fixture(&root, device));

    let scan = match Scanner::new()
        .with_engine(Engine::Sequential)
        .with_reader(reader)
        .scan(&root)
        .expect("scan succeeds")
    {
        ScanOutcome::Completed(scan) => scan,
        other => panic!("expected completion, got {other:?}"),
    };

    let mounted = (0..scan.tree.len())
        .filter_map(|index| rdirstat_core::NodeId::from_index(u32::try_from(index).ok()?))
        .find(|id| scan.tree.name_bytes(*id) == Some(b"mounted"))
        .expect("the mount marker is retained");
    let node = scan.tree.node(mounted).expect("live");
    assert!(node.has_flags(flags::MOUNT_POINT));
    assert_eq!(scan.tree.child_count(mounted), 0, "the other volume is not descended");
    assert_eq!(scan.totals.logical, 100, "only this volume's file is counted");
}

#[test]
fn a_device_boundary_is_crossed_only_when_asked() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = canonical(&temp);
    let device = root_device(&root);
    let reader = Arc::new(mount_fixture(&root, device));

    let mut options = ScanOptions::default();
    options.cross_filesystems = true;
    options.apply_default_exclusions = false;

    let scan = Scanner::new()
        .with_engine(Engine::parallel(2))
        .with_reader(reader)
        .with_options(options)
        .scan(&root)
        .expect("scan succeeds")
        .completed()
        .expect("completed");

    assert_eq!(scan.totals.logical, 200, "both files once crossing is enabled");
    assert_eq!(scan.counts.files, 2);
}

#[test]
fn a_firmlink_is_marked_and_never_descended() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = canonical(&temp);
    let device = root_device(&root);
    let mut firmlink = entry(b"Data", Kind::Directory, device, 20);
    firmlink.platform_flags = SF_FIRMLINK;

    let reader = Arc::new(
        FakeReader::new()
            .listing(root.clone(), vec![firmlink])
            .listing(root.join("Data"), vec![entry(b"huge.bin", Kind::File, device, 21)]),
    );

    let scan = Scanner::new()
        .with_engine(Engine::Sequential)
        .with_reader(reader)
        .scan(&root)
        .expect("scan succeeds")
        .completed()
        .expect("completed");

    let data = (0..scan.tree.len())
        .filter_map(|index| rdirstat_core::NodeId::from_index(u32::try_from(index).ok()?))
        .find(|id| scan.tree.name_bytes(*id) == Some(b"Data"))
        .expect("the firmlink is retained as a marker");
    let node = scan.tree.node(data).expect("live");
    assert!(node.has_flags(flags::FIRMLINK));
    assert_eq!(
        scan.tree.child_count(data),
        0,
        "descending a firmlink walks the data volume twice"
    );
    assert_eq!(scan.totals.logical, 0);
}

#[test]
fn cancellation_mid_traversal_discards_partial_state() {
    for engine in [Engine::Sequential, Engine::parallel(2)] {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = canonical(&temp);
        let device = root_device(&root);
        let scanner = Scanner::new().with_engine(engine);
        let cancel = scanner.cancel_token();

        let mut reader = FakeReader::new()
            .listing(
                root.clone(),
                vec![
                    entry(b"one", Kind::Directory, device, 30),
                    entry(b"two", Kind::Directory, device, 31),
                ],
            )
            .listing(root.join("one"), vec![entry(b"a.txt", Kind::File, device, 32)])
            .listing(root.join("two"), vec![entry(b"b.txt", Kind::File, device, 33)]);
        reader = reader.cancel_after(1, cancel.clone());

        let outcome = scanner
            .with_reader(Arc::new(reader))
            .with_cancel(cancel)
            .scan(&root)
            .expect("cancellation is not a failure");
        assert!(outcome.is_cancelled(), "{engine:?} should stop on cancel");
    }
}

#[test]
fn an_unknown_directory_is_recorded_and_the_walk_continues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = canonical(&temp);
    let device = root_device(&root);
    // `ghost` has no listing, so the fake reader reports ENOENT for it.
    let reader = Arc::new(
        FakeReader::new()
            .listing(
                root.clone(),
                vec![
                    entry(b"ghost", Kind::Directory, device, 40),
                    entry(b"real", Kind::Directory, device, 41),
                ],
            )
            .listing(root.join("real"), vec![entry(b"kept.txt", Kind::File, device, 42)]),
    );

    let scan = Scanner::new()
        .with_engine(Engine::parallel(2))
        .with_reader(reader)
        .scan(&root)
        .expect("a vanished child never fails the scan")
        .completed()
        .expect("completed");

    assert_eq!(scan.totals.logical, 100, "the readable sibling is still counted");
    assert_eq!(scan.mutations, 1, "the disappearance is counted as a mutation");
    assert!(
        scan.errors
            .iter()
            .any(|error| matches!(error, ScanError::Vanished { .. }))
    );
}

#[test]
fn a_three_thousand_level_chain_rolls_up_iteratively() {
    // No filesystem can hold a path this deep (PATH_MAX), which is exactly why
    // it goes through the reader trait: the point is that neither the traversal
    // nor the rollup recurses. A recursive implementation would blow the stack
    // here; depth is untrusted input.
    const DEPTH: usize = 3_000;

    let temp = tempfile::tempdir().expect("temp dir");
    let root = canonical(&temp);
    let device = root_device(&root);

    let mut reader = FakeReader::new();
    let mut current = root.clone();
    for level in 0..DEPTH {
        let name = format!("level{level}");
        reader = reader.listing(
            current.clone(),
            vec![entry(
                name.as_bytes(),
                Kind::Directory,
                device,
                u64::try_from(level).expect("fits"),
            )],
        );
        current = current.join(name);
    }
    reader = reader.listing(current, vec![entry(b"bottom.bin", Kind::File, device, 999_999)]);

    for engine in [Engine::Sequential, Engine::parallel(3)] {
        let scan = Scanner::new()
            .with_engine(engine)
            .with_reader(Arc::new(FakeReader {
                listings: reader.listings.clone(),
                reads: AtomicUsize::new(0),
                cancel_after: None,
            }))
            .scan(&root)
            .expect("deep is not an error")
            .completed()
            .expect("completed");

        assert_eq!(
            scan.counts.directories,
            u64::try_from(DEPTH + 1).expect("fits"),
            "{engine:?}: the root plus every level"
        );
        assert_eq!(scan.totals.logical, 100, "{engine:?}: the one file at the bottom");
        let deepest = (0..scan.tree.len())
            .filter_map(|index| rdirstat_core::NodeId::from_index(u32::try_from(index).ok()?))
            .find(|id| scan.tree.name_bytes(*id) == Some(b"bottom.bin"))
            .expect("the bottom file is retained");
        assert_eq!(
            scan.tree.depth(deepest).expect("depth is bounded, not recursive"),
            u32::try_from(DEPTH + 1).expect("fits")
        );
    }
}
