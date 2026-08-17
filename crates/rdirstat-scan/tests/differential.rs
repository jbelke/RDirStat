#![allow(clippy::expect_used, clippy::panic)]

//! The differential test: the two schedulers must agree.
//!
//! This is the test that makes the parallel engine trustworthy. The sequential
//! walker is deliberately boring — one stack, no threads, no channels — so when
//! the two disagree the bug is in the scheduler, not in the policy: both drive
//! the *same* [`StdReader`](rdirstat_scan::StdReader) and the same builder.
//!
//! Enumeration order is unspecified, so the comparison normalizes to a
//! path-sorted set first. One flag is deliberately excluded from the strict
//! comparison and checked separately: which of two hard-linked names is the
//! contributor is decided by traversal order, and the *invariant* is that
//! exactly one is, which is what the totals assert.

mod common;

use rdirstat_core::flags;
use rdirstat_scan::{Engine, ScanOutcome, Scanner, StdReader};
use std::sync::Arc;

fn strip_order_dependent(rows: Vec<(String, rdirstat_core::Kind, u64, u64, i64, u16)>) -> Vec<(String, u64, u64, u16)> {
    rows.into_iter()
        .map(|(path, _kind, size, alloc, _mtime, bits)| (path, size, alloc, bits & !flags::HARD_LINK_REPEAT))
        .collect()
}

#[test]
fn both_schedulers_produce_the_same_normalized_entry_set() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    common::build_deep_chain(&temp.path().join("a"), 40);

    let sequential = common::scan_with(temp.path(), Engine::Sequential);
    let parallel = common::scan_with(temp.path(), Engine::parallel(8));
    let single_worker = common::scan_with(temp.path(), Engine::parallel(1));
    common::restore(&fixture);

    let a = common::normalize(&sequential.tree);
    let b = common::normalize(&parallel.tree);
    let c = common::normalize(&single_worker.tree);

    assert_eq!(a.len(), b.len(), "the same nodes are retained");
    assert_eq!(
        strip_order_dependent(a.clone()),
        strip_order_dependent(b.clone()),
        "sequential and 8-worker parallel see the same tree"
    );
    assert_eq!(
        strip_order_dependent(a),
        strip_order_dependent(c),
        "one worker is still the parallel engine, and still agrees"
    );

    // Kinds and mtimes are order-independent, so they compare exactly.
    let kinds_a: Vec<_> = common::normalize(&sequential.tree)
        .into_iter()
        .map(|(path, kind, _, _, mtime, _)| (path, kind, mtime))
        .collect();
    let kinds_b: Vec<_> = common::normalize(&parallel.tree)
        .into_iter()
        .map(|(path, kind, _, _, mtime, _)| (path, kind, mtime))
        .collect();
    assert_eq!(kinds_a, kinds_b);
}

#[test]
fn both_schedulers_produce_the_same_totals_and_counts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let sequential = common::scan_with(temp.path(), Engine::Sequential);
    let parallel = common::scan_with(temp.path(), Engine::parallel(6));
    common::restore(&fixture);

    assert_eq!(
        sequential.totals, parallel.totals,
        "hard-link policy makes the total independent of which name pays"
    );
    assert_eq!(sequential.counts.observed_entries, parallel.counts.observed_entries);
    assert_eq!(sequential.counts.retained_nodes, parallel.counts.retained_nodes);
    assert_eq!(sequential.counts.directories, parallel.counts.directories);
    assert_eq!(sequential.counts.files, parallel.counts.files);
    assert_eq!(sequential.counts.symlinks, parallel.counts.symlinks);
    assert_eq!(sequential.counts.special, parallel.counts.special);
    assert_eq!(sequential.counts.unreadable_dirs, parallel.counts.unreadable_dirs);
    assert_eq!(sequential.counts.hard_link_repeats, parallel.counts.hard_link_repeats);
    assert_eq!(sequential.counts.excluded_paths, parallel.counts.excluded_paths);
}

#[test]
fn both_schedulers_produce_the_same_error_classes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    if fixture.unreadable.is_none() {
        common::restore(&fixture);
        return; // running as root: there is no permission error to compare
    }
    let sequential = common::scan_with(temp.path(), Engine::Sequential);
    let parallel = common::scan_with(temp.path(), Engine::parallel(4));
    common::restore(&fixture);

    let mut a = sequential.error_counts.clone();
    let mut b = parallel.error_counts.clone();
    a.sort_by_key(|count| (format!("{:?}", count.class), format!("{:?}", count.operation)));
    b.sort_by_key(|count| (format!("{:?}", count.class), format!("{:?}", count.operation)));
    assert_eq!(a, b, "the same failures, classified the same way");
    assert!(!a.is_empty(), "the fixture does contain an unreadable directory");
}

#[test]
fn chunking_a_large_directory_does_not_change_the_result() {
    // Force the parallel worker to split one directory across many chunks and
    // the reader to emit many small batches, then insist nothing changed.
    let temp = tempfile::tempdir().expect("temp dir");
    let wide = temp.path().join("wide");
    std::fs::create_dir(&wide).expect("wide");
    for index in 0..500_u32 {
        std::fs::write(wide.join(format!("f{index:04}")), b"12345").expect("file");
    }

    let tiny_batches = Arc::new(StdReader::new().with_batch_bounds(7, 64));
    let sequential = common::scan_with_scanner(
        &Scanner::new()
            .with_engine(Engine::Sequential)
            .with_reader(tiny_batches.clone()),
        temp.path(),
    );
    let parallel = common::scan_with_scanner(
        &Scanner::new()
            .with_engine(Engine::parallel(4))
            .with_reader(tiny_batches),
        temp.path(),
    );

    assert_eq!(sequential.counts.files, 500);
    assert_eq!(sequential.totals.logical, 500 * 5);
    assert_eq!(sequential.totals, parallel.totals);
    assert_eq!(common::normalize(&sequential.tree), common::normalize(&parallel.tree));
}

#[test]
fn a_cancelled_scan_never_becomes_a_completed_scan() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());

    for engine in [Engine::Sequential, Engine::parallel(4)] {
        let scanner = Scanner::new().with_engine(engine);
        let cancel = scanner.cancel_token();
        cancel.cancel();
        let outcome = scanner.scan(temp.path()).expect("cancellation is not a failure");
        assert!(
            matches!(outcome, ScanOutcome::Cancelled),
            "{engine:?} should report cancellation"
        );
        assert!(outcome.completed().is_none());
    }
    common::restore(&fixture);
}
