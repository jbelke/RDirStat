//! Layout cost at a recorded tile count.
//!
//! docs/05-UI.md: "The 16 ms microbenchmark applies to layout+paint at a
//! recorded tile count", and "candidate node count alone is not evidence that
//! the sub-pixel cutoff kept the rendered set bounded". So each benchmark
//! prints the tiles it drew and the child links it considered before measuring,
//! and the number here is **layout only** — paint belongs to the renderer.

#![allow(
    clippy::expect_used,
    reason = "clippy.toml sets allow-expect-in-tests, which does not cover a bench target; a \
              fixture that fails to build must abort the benchmark, and every expect has a message"
)]

use criterion::{Criterion, criterion_group, criterion_main};
use rdirstat_core::{DirTotals, Kind, LayoutKind, Node, NodeId, Tree, TreeBuilder, TreeGeneration, Viewport};
use rdirstat_treemap::{LayoutOptions, layout, layout_tiles};
use std::hint::black_box;

const MTIME: i64 = 1_700_000_000;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn size(&mut self) -> u64 {
        let magnitude = self.next() % 9;
        let base = self.next() % 1_000;
        (base + 1) * 10_u64.pow(u32::try_from(magnitude).unwrap_or(0))
    }
}

/// `branch^depth` directories, each holding `files` files.
fn synthetic_tree(branch: usize, depth: u32, files: usize) -> Tree {
    let mut rng = Lcg(0x5EED_1234_ABCD_0001);
    let mut builder = TreeBuilder::with_capacity(1 << 20, 32 << 20, 1 << 16);

    let root_name = builder.intern(b"root").expect("root name");
    let root = builder.push_node(Node::directory(root_name, MTIME)).expect("root node");
    builder.register_directory(root, DirTotals::EMPTY).expect("root totals");

    let mut level = vec![root];
    for _ in 0..depth {
        let mut next = Vec::with_capacity(level.len() * branch);
        for parent in &level {
            for index in 0..branch {
                let name = format!("dir-{index:04}");
                let reference = builder.intern(name.as_bytes()).expect("dir name");
                let dir = builder
                    .push_child(*parent, Node::directory(reference, MTIME))
                    .expect("dir node");
                builder.register_directory(dir, DirTotals::EMPTY).expect("dir totals");
                next.push(dir);
            }
        }
        level = next;
    }

    for parent in &level {
        for index in 0..files {
            let name = format!("f-{index:05}.bin");
            let reference = builder.intern(name.as_bytes()).expect("file name");
            let bytes = rng.size();
            builder
                .push_child(*parent, Node::leaf(reference, Kind::File, bytes, bytes, MTIME))
                .expect("file node");
            if let Some(totals) = builder.dir_totals_mut(*parent) {
                totals.absorb_direct_file(bytes, bytes, MTIME);
            }
        }
    }

    builder.rollup().expect("rollup");
    builder.finish().expect("finish")
}

fn viewport() -> Viewport {
    Viewport {
        width: 1_600.0,
        height: 1_000.0,
        device_pixel_ratio: 2.0,
    }
}

fn bench_layouts(criterion: &mut Criterion) {
    // 8 + 64 + 512 directories, 200 files in each of the 512 leaves: ~103k nodes.
    let tree = synthetic_tree(8, 3, 200);
    let root: NodeId = tree.root();
    println!(
        "bench fixture: {} nodes, {} directories",
        tree.len(),
        tree.directory_count()
    );

    for kind in [LayoutKind::Treemap, LayoutKind::Icicle, LayoutKind::Sunburst] {
        let options = LayoutOptions::new(kind, viewport(), 3.0).expect("valid options");
        let stats = layout_tiles(&tree, root, &options).expect("a layout").stats();
        println!(
            "{kind:?}: {} tiles drawn, {} child links considered, max depth {}",
            stats.tiles, stats.considered, stats.max_depth
        );

        let mut group = criterion.benchmark_group("layout");
        group.bench_function(format!("{kind:?}/tiles"), |bencher| {
            bencher.iter(|| layout_tiles(black_box(&tree), black_box(root), black_box(&options)));
        });
        group.bench_function(format!("{kind:?}/arrow"), |bencher| {
            bencher.iter(|| {
                layout(
                    black_box(&tree),
                    TreeGeneration::FIRST,
                    black_box(root),
                    kind,
                    viewport(),
                    3.0,
                )
            });
        });
        group.finish();
    }
}

criterion_group!(benches, bench_layouts);
criterion_main!(benches);
