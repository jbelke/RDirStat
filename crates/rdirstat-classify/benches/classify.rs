//! Throughput of the classify path.
//!
//! docs/04-CLASSIFICATION.md#performance-and-tests sets the phase-2 target at
//! **more than 20M representative names/second**, single-threaded, release
//! build. "Representative" is the load-bearing word: one short extension
//! repeated a million times is not evidence, so the corpus below mixes hit and
//! miss shapes in roughly the proportions a real home directory produces, and
//! the harness prints its own name-length and suffix-hit distribution before
//! measuring.
//!
//! Run with:
//!
//! ```text
//! CARGO_TARGET_DIR=target/agent-1 cargo bench -p rdirstat-classify --bench classify
//! ```
//!
//! # Recorded result
//!
//! This is **not** the "recorded target Mac" of docs/00; it is the machine the
//! crate was written on, and the number is stated so a regression has something
//! to fail against. Apple M3 Max, macOS 15.5, rustc 1.97.1, `bench` profile
//! (release + `lto = "thin"` + `codegen-units = 1`), single thread,
//! 2026-08-17:
//!
//! | benchmark | median | throughput |
//! | --- | --- | --- |
//! | `classify/representative_corpus` (50k names) | 2.23 ms | **22.4 M names/s** |
//! | `classify/single_hit_short` (`IMG_4821.jpg`) | 16.5 ns | 60.7 M/s |
//! | `classify/single_hit_multipart` (`backup.tar.gz`) | 18.7 ns | 53.4 M/s |
//! | `classify/single_miss_glob_scan` | 16.5 ns | 60.5 M/s |
//! | `classify/single_symlink` | 0.65 ns | 1.53 G/s |
//! | `context_tags/component_hit` | ~31 ns | ~32 M/s |
//!
//! The corpus number is the one that counts, and it clears the >20M/s target
//! with ~12% headroom on *this* machine. It is not a claim about the target Mac
//! in docs/00, which this workflow never measured.
//!
//! Two changes got it there from a first-cut 8.5 M/s, both recorded because
//! the naive shape is the one a rewrite would fall back into: bucketing the
//! basename globs by first byte (a linear walk over ~60 patterns cost 148 ns on
//! every unmatched name) and replacing `memchr::memrchr` with a scalar reverse
//! scan for names under 64 bytes.
//!
//! Allocation: zero on this path, by construction rather than by counter — a
//! counting `GlobalAlloc` needs `unsafe impl`, which this workspace forbids
//! outside one audited module. Suffix slices borrow the input, folding uses a
//! stack buffer, and every table is built at compile time.

#![allow(
    clippy::expect_used,
    reason = "benchmark setup: a broken fixture must fail loudly, and clippy.toml only\n              exempts `expect` inside #[test] functions, not the helpers they call"
)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rdirstat_classify::{Categorizer, CategoryId};
use rdirstat_core::Kind;

/// One corpus entry, in the shape the builder holds it: bytes, kind, mode.
struct Entry {
    name: Vec<u8>,
    kind: Kind,
    mode: u32,
}

/// A deterministic xorshift, so the corpus is identical between runs and
/// between machines. No `rand` dependency for a benchmark fixture.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap_or(1)).unwrap_or(0)
    }
}

/// Suffixes weighted the way a home directory actually looks: a long tail of
/// source and media, a solid block of generated files, some archives.
const HOT_SUFFIXES: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "mp4", "mov", "mp3", "m4a", "pdf", "txt", "md", "json", "plist", "js", "ts", "tsx",
    "rs", "py", "go", "swift", "h", "c", "cpp", "o", "d", "log", "lock", "css", "html", "yaml", "xml", "so", "dylib",
    "a", "ttf", "sqlite", "db-wal", "zip", "tar.gz", "tar.bz2", "tar.zst", "gz", "dmg", "qcow2", "cr3", "psd", "wasm",
    "pyc", "class", "woff2",
];

/// Extensions that match nothing. Real trees are full of them and they are the
/// *expensive* case: every rung of the ladder runs.
const COLD_SUFFIXES: &[&str] = &["qqq", "dat", "bin", "idx", "tbl", "state", "v2", "backup"];

/// Whole names with no extension at all.
const BARE_NAMES: &[&str] = &[
    "README",
    "LICENSE",
    "configure",
    "Makefile",
    "Dockerfile",
    "install",
    "COPYING",
    "CHANGELOG",
];

/// Leading-dot names.
const DOT_NAMES: &[&str] = &[
    ".DS_Store",
    ".gitignore",
    ".env",
    ".zshrc",
    "._sidecar",
    ".localized",
    ".npmrc",
];

/// Directory names, including bundles.
const DIR_NAMES: &[&str] = &[
    "node_modules",
    "Caches",
    "DerivedData",
    "Safari.app",
    "MyLib.framework",
    "Photos.photoslibrary",
    "src",
    "Documents",
    "build",
    "MyApp.xcarchive",
];

const STEMS: &[&str] = &[
    "a",
    "img",
    "IMG_4821",
    "report",
    "Screenshot 2026-02-11 at 09.14.22",
    "index",
    "com.apple.some.long.reverse.dns.identifier",
    "vacation photo (1)",
    "libnativeruntimesupport",
    "x",
];

fn corpus(count: usize) -> Vec<Entry> {
    let mut rng = Rng(0x2026_0817_dead_beef);
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let roll = rng.below(100);
        let stem = STEMS.get(rng.below(STEMS.len())).copied().unwrap_or("x");
        let (name, kind, mode) = if roll < 55 {
            let suffix = HOT_SUFFIXES
                .get(rng.below(HOT_SUFFIXES.len()))
                .copied()
                .unwrap_or("txt");
            (format!("{stem}.{suffix}"), Kind::File, 0o644)
        } else if roll < 68 {
            let suffix = COLD_SUFFIXES
                .get(rng.below(COLD_SUFFIXES.len()))
                .copied()
                .unwrap_or("qqq");
            (format!("{stem}.{suffix}"), Kind::File, 0o644)
        } else if roll < 78 {
            let bare = BARE_NAMES.get(rng.below(BARE_NAMES.len())).copied().unwrap_or("README");
            let mode = if rng.below(2) == 0 { 0o755 } else { 0o644 };
            (bare.to_owned(), Kind::File, mode)
        } else if roll < 85 {
            let dot = DOT_NAMES.get(rng.below(DOT_NAMES.len())).copied().unwrap_or(".env");
            (dot.to_owned(), Kind::File, 0o644)
        } else if roll < 95 {
            let dir = DIR_NAMES.get(rng.below(DIR_NAMES.len())).copied().unwrap_or("src");
            (dir.to_owned(), Kind::Directory, 0o755)
        } else {
            (format!("{stem}.link"), Kind::Symlink, 0o777)
        };
        entries.push(Entry {
            name: name.into_bytes(),
            kind,
            mode,
        });
    }
    entries
}

/// Prints the corpus profile, because a throughput number without its input
/// distribution is not a measurement.
fn describe(categorizer: &Categorizer, entries: &[Entry]) {
    let total = entries.len().max(1);
    let bytes: usize = entries.iter().map(|entry| entry.name.len()).sum();
    let mut lengths: Vec<usize> = entries.iter().map(|entry| entry.name.len()).collect();
    lengths.sort_unstable();
    let percentile = |p: usize| lengths.get(lengths.len() * p / 100).copied().unwrap_or(0);

    let mut hits = 0usize;
    let mut symlinks = 0usize;
    let mut directories = 0usize;
    for entry in entries {
        let id = categorizer.classify(&entry.name, entry.kind, entry.mode);
        if id != CategoryId::UNCATEGORIZED {
            hits += 1;
        }
        if entry.kind == Kind::Symlink {
            symlinks += 1;
        }
        if entry.kind == Kind::Directory {
            directories += 1;
        }
    }

    eprintln!("corpus: {total} names, {bytes} name bytes, mean {} B", bytes / total);
    eprintln!(
        "        name length p50={} p90={} p99={} max={}",
        percentile(50),
        percentile(90),
        percentile(99),
        lengths.last().copied().unwrap_or(0)
    );
    // Integer permille: no float ever touches a measurement in this repo.
    let permille = |part: usize| (part * 1000 / total, (part * 1000 / total) % 10);
    let (hit_p, hit_f) = permille(hits);
    let (sym_p, sym_f) = permille(symlinks);
    let (dir_p, dir_f) = permille(directories);
    eprintln!(
        "        categorized {}.{hit_f}% | symlinks {}.{sym_f}% | directories {}.{dir_f}%",
        hit_p / 10,
        sym_p / 10,
        dir_p / 10
    );
}

fn bench_classify(c: &mut Criterion) {
    let categorizer = Categorizer::defaults().expect("the shipped defaults compile");
    let entries = corpus(50_000);
    describe(&categorizer, &entries);

    let mut group = c.benchmark_group("classify");
    group.throughput(Throughput::Elements(u64::try_from(entries.len()).unwrap_or(0)));
    group.bench_function("representative_corpus", |b| {
        b.iter(|| {
            let mut accumulator = 0u32;
            for entry in &entries {
                let id = categorizer.classify(
                    black_box(entry.name.as_slice()),
                    black_box(entry.kind),
                    black_box(entry.mode),
                );
                accumulator = accumulator.wrapping_add(u32::from(id.get()));
            }
            accumulator
        });
    });

    // The three shapes that dominate, measured on their own so a regression
    // can be attributed instead of guessed at.
    let one = |name: &'static str, kind: Kind, mode: u32| {
        move |b: &mut criterion::Bencher<'_>| {
            let categorizer = Categorizer::defaults().expect("defaults compile");
            b.iter(|| categorizer.classify(black_box(name.as_bytes()), black_box(kind), black_box(mode)));
        }
    };
    group.throughput(Throughput::Elements(1));
    group.bench_function("single_hit_short", one("IMG_4821.jpg", Kind::File, 0o644));
    group.bench_function("single_hit_multipart", one("backup.tar.gz", Kind::File, 0o644));
    group.bench_function(
        "single_miss_glob_scan",
        one("some-unmatched-name.qqq", Kind::File, 0o644),
    );
    group.bench_function("single_symlink", one("whatever.mp4", Kind::Symlink, 0o777));
    group.finish();

    let mut tags = c.benchmark_group("context_tags");
    tags.throughput(Throughput::Elements(1));
    tags.bench_function("component_hit", |b| {
        b.iter(|| categorizer.context_tags(black_box(b"node_modules")));
    });
    tags.bench_function("component_miss", |b| {
        b.iter(|| categorizer.context_tags(black_box(b"Documents")));
    });
    tags.finish();
}

criterion_group!(benches, bench_classify);
criterion_main!(benches);
