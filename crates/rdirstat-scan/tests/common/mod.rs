//! One quiescent fixture, shared by the policy and differential tests.
//!
//! Nothing here writes outside a [`TempDir`](tempfile::TempDir).

#![allow(dead_code, unreachable_pub, clippy::expect_used, clippy::panic)]

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rdirstat_core::{CompletedScan, Kind, NodeId, Tree};
use rdirstat_scan::{Engine, ScanOutcome, Scanner};

/// What the fixture managed to create. Some objects need privileges or a short
/// path, so their tests skip rather than fail on a machine that cannot make
/// them.
#[derive(Debug, Default)]
pub struct Fixture {
    pub socket: bool,
    pub unreadable: Option<PathBuf>,
}

/// Sizes the assertions quote.
pub const ONE_TXT: u64 = 100;
pub const TWO_BIN: u64 = 2_000;
pub const DEEP_TXT: u64 = 10;
pub const HARD_BYTES: u64 = 5_000;
pub const SPARSE_BYTES: u64 = 4 * 1024 * 1024;
pub const PACKAGE_BYTES: u64 = 300;
pub const EXCLUDED_BYTES: u64 = 777;

/// Builds the tree every test in this crate shares.
///
/// ```text
/// root/
///   a/one.txt          100 B
///   a/two.bin        2 000 B
///   a/nested/deep.txt   10 B
///   b/filelink   -> ../a/one.txt     symlink, a leaf
///   b/dirlink    -> ../a             symlink to a directory, still a leaf
///   b/broken     -> ./nowhere        dangling symlink
///   b/sock                           unix socket, when the path is short enough
///   empty/                           no children
///   Excluded/skipped.bin           777 B
///   Some.app/Contents.bin          300 B
///   hard1.dat / hard2.dat        5 000 B of content, two names
///   sparse.img                4 194 304 B logical, far less allocated
///   locked/                          mode 000, when not running as root
/// ```
pub fn build(root: &Path) -> Fixture {
    let mut fixture = Fixture::default();

    let a = root.join("a");
    fs::create_dir(&a).expect("a");
    write_bytes(&a.join("one.txt"), ONE_TXT);
    write_bytes(&a.join("two.bin"), TWO_BIN);
    let nested = a.join("nested");
    fs::create_dir(&nested).expect("nested");
    write_bytes(&nested.join("deep.txt"), DEEP_TXT);

    let b = root.join("b");
    fs::create_dir(&b).expect("b");
    std::os::unix::fs::symlink("../a/one.txt", b.join("filelink")).expect("file symlink");
    std::os::unix::fs::symlink("../a", b.join("dirlink")).expect("dir symlink");
    std::os::unix::fs::symlink("./nowhere", b.join("broken")).expect("broken symlink");
    fixture.socket = std::os::unix::net::UnixListener::bind(b.join("sock")).is_ok();

    fs::create_dir(root.join("empty")).expect("empty");

    let excluded = root.join("Excluded");
    fs::create_dir(&excluded).expect("Excluded");
    write_bytes(&excluded.join("skipped.bin"), EXCLUDED_BYTES);

    let package = root.join("Some.app");
    fs::create_dir(&package).expect("Some.app");
    write_bytes(&package.join("Contents.bin"), PACKAGE_BYTES);

    write_bytes(&root.join("hard1.dat"), HARD_BYTES);
    fs::hard_link(root.join("hard1.dat"), root.join("hard2.dat")).expect("hard link");

    let sparse = File::create(root.join("sparse.img")).expect("sparse");
    sparse.set_len(SPARSE_BYTES).expect("set_len");
    drop(sparse);

    let locked = root.join("locked");
    fs::create_dir(&locked).expect("locked");
    write_bytes(&locked.join("secret.bin"), 42);
    if set_unreadable(&locked) {
        fixture.unreadable = Some(locked);
    }

    fixture
}

/// Restores permissions so the `TempDir` can be removed.
pub fn restore(fixture: &Fixture) {
    if let Some(locked) = &fixture.unreadable {
        let _ignored = fs::set_permissions(locked, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    }
}

fn set_unreadable(path: &Path) -> bool {
    if fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o000)).is_err() {
        return false;
    }
    // Running as root ignores the mode bits, so the "unreadable" assertions
    // would be testing the wrong thing. Detect that and skip instead.
    fs::read_dir(path).is_err()
}

fn write_bytes(path: &Path, len: u64) {
    let mut file = File::create(path).expect("create");
    let block = vec![b'x'; 4096];
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(4096);
        let take = usize::try_from(chunk).expect("at most 4096");
        file.write_all(&block[..take]).expect("write");
        remaining -= chunk;
    }
    file.flush().expect("flush");
}

/// Builds a chain of `depth` nested directories with a file at the bottom.
pub fn build_deep_chain(root: &Path, depth: usize) -> PathBuf {
    let mut current = root.to_path_buf();
    for step in 0..depth {
        current = current.join(format!("d{step}"));
        fs::create_dir(&current).expect("chain level");
    }
    write_bytes(&current.join("bottom.txt"), 7);
    current
}

/// Runs a scan with the given engine and default options, returning the
/// completed scan.
pub fn scan_with(root: &Path, engine: Engine) -> Box<CompletedScan> {
    scan_with_scanner(&Scanner::new().with_engine(engine), root)
}

/// Runs a configured scanner and insists it completed.
pub fn scan_with_scanner(scanner: &Scanner, root: &Path) -> Box<CompletedScan> {
    match scanner
        .scan(root)
        .expect("the scan succeeds even with unreadable paths")
    {
        ScanOutcome::Completed(scan) => scan,
        other => panic!("expected a completed scan, got {other:?}"),
    }
}

/// The first node with this name, anywhere in the tree.
pub fn find(tree: &Tree, name: &[u8]) -> Option<NodeId> {
    (0..tree.len())
        .filter_map(|index| NodeId::from_index(u32::try_from(index).ok()?))
        .find(|id| tree.name_bytes(*id) == Some(name))
}

/// Every node as `(path, kind, size, alloc, mtime, flags)`, sorted by path.
///
/// This is the normalization the differential test compares: child insertion
/// order is not a contract, so the sets are ordered by path before comparison.
pub fn normalize(tree: &Tree) -> Vec<(String, Kind, u64, u64, i64, u16)> {
    let mut rows = Vec::with_capacity(tree.len());
    let mut path = Vec::with_capacity(256);
    for index in 0..tree.len() {
        let Some(id) = u32::try_from(index).ok().and_then(NodeId::from_index) else {
            continue;
        };
        let Some(node) = tree.node(id) else { continue };
        path.clear();
        tree.path_bytes(id, &mut path).expect("every node has a path");
        rows.push((
            String::from_utf8_lossy(&path).into_owned(),
            node.kind(),
            node.size,
            node.alloc,
            node.mtime,
            node.flags,
        ));
    }
    rows.sort();
    rows
}

/// Sums every leaf's contributed bytes with a plain iteration over the arena —
/// the naive answer the iterative rollup has to reproduce.
pub fn naive_totals(tree: &Tree) -> (u64, u64) {
    tree.nodes().iter().fold((0_u64, 0_u64), |(logical, allocated), node| {
        (
            logical.saturating_add(node.contributed_size()),
            allocated.saturating_add(node.contributed_alloc()),
        )
    })
}
