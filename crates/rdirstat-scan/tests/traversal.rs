#![allow(clippy::expect_used, clippy::panic)]

//! One test per traversal rule in docs/02-SCANNER.md#traversal-rules.
//!
//! Every one of them runs against a quiescent `TempDir` fixture. None of them
//! writes outside it.

mod common;

use rdirstat_core::{Kind, ScanOptions, flags};
use rdirstat_scan::{Engine, ExclusionSet, Scanner, default_exclusions, path_rule};

#[test]
fn a_symlink_is_a_leaf_and_its_target_is_never_walked_through_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let dirlink = common::find(&scan.tree, b"dirlink").expect("the symlink to a directory is retained");
    let node = scan.tree.node(dirlink).expect("live node");
    assert_eq!(
        node.kind(),
        Kind::Symlink,
        "a symlink to a directory is still a symlink"
    );
    assert_eq!(scan.tree.child_count(dirlink), 0, "a symlink never has children");
    assert!(
        scan.tree.dir_totals(dirlink).is_none(),
        "a symlink gets no directory row"
    );

    // `a/one.txt` exists exactly once: reached through `a`, never through
    // `b/dirlink`.
    let paths = common::normalize(&scan.tree);
    let hits = paths.iter().filter(|(path, ..)| path.ends_with("/one.txt")).count();
    assert_eq!(hits, 1, "the target is counted once, under its real parent");

    let broken = common::find(&scan.tree, b"broken").expect("dangling symlink retained");
    let broken_node = scan.tree.node(broken).expect("live node");
    assert!(
        broken_node.has_flags(flags::BROKEN_SYMLINK),
        "a dangling symlink is flagged, not resolved"
    );
}

#[test]
fn hard_linked_content_is_counted_once_and_the_repeat_stays_visible() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let first = common::find(&scan.tree, b"hard1.dat").expect("first name");
    let second = common::find(&scan.tree, b"hard2.dat").expect("second name");
    let a = scan.tree.node(first).expect("live");
    let b = scan.tree.node(second).expect("live");

    assert!(a.has_flags(flags::HARD_LINK) && b.has_flags(flags::HARD_LINK));
    let repeats = u32::from(a.has_flags(flags::HARD_LINK_REPEAT)) + u32::from(b.has_flags(flags::HARD_LINK_REPEAT));
    assert_eq!(repeats, 1, "exactly one of the two names is the repeat");
    assert_eq!(scan.counts.hard_link_repeats, 1);

    let contributed = a.contributed_size() + b.contributed_size();
    assert_eq!(
        contributed,
        common::HARD_BYTES,
        "the content is counted once, not twice"
    );
    assert_eq!(a.size, common::HARD_BYTES, "both entries still report their real size");
    assert_eq!(b.size, common::HARD_BYTES);
}

#[test]
fn counting_hard_links_twice_is_an_option_not_a_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());

    let mut options = ScanOptions::default();
    options.count_hard_links_once = false;
    let scan = common::scan_with_scanner(
        &Scanner::new().with_engine(Engine::Sequential).with_options(options),
        temp.path(),
    );
    common::restore(&fixture);

    assert_eq!(scan.counts.hard_link_repeats, 0);
    let first = common::find(&scan.tree, b"hard1.dat").expect("first");
    let second = common::find(&scan.tree, b"hard2.dat").expect("second");
    let total = scan.tree.node(first).expect("live").contributed_size()
        + scan.tree.node(second).expect("live").contributed_size();
    assert_eq!(total, common::HARD_BYTES * 2);
}

#[test]
fn logical_and_allocated_stay_separate_and_sparseness_is_a_flag() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let sparse = common::find(&scan.tree, b"sparse.img").expect("sparse file");
    let node = scan.tree.node(sparse).expect("live");
    assert_eq!(node.size, common::SPARSE_BYTES, "logical size is what the file says");
    assert!(node.alloc <= node.size);
    if node.alloc < node.size {
        assert!(node.has_flags(flags::SPARSE), "alloc < size is the sparse signal");
    }
    assert_ne!(
        scan.totals.logical, scan.totals.allocated,
        "the two totals are different numbers and are never reconciled"
    );
}

#[test]
fn special_files_are_zero_contribution_leaves() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    if !fixture.socket {
        common::restore(&fixture);
        return; // the temp path was too long for a socket; nothing to assert
    }
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let sock = common::find(&scan.tree, b"sock").expect("socket retained");
    let node = scan.tree.node(sock).expect("live");
    assert_eq!(node.kind(), Kind::Socket);
    assert_eq!(node.size, 0, "a socket contributes no data bytes");
    assert_eq!(node.alloc, 0);
    assert_eq!(scan.tree.child_count(sock), 0);
    assert_eq!(scan.counts.special, 1);
}

#[test]
fn direct_files_form_a_virtual_group_with_no_arena_node() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let root = scan.tree.root();
    let totals = scan.tree.dir_totals(root).expect("the root is a directory");
    assert!(totals.has_direct_files(), "the root has files of its own");
    let group = scan.tree.virtual_group(root).expect("group exists");
    assert!(group.is_virtual_group());
    assert_eq!(
        scan.tree.node(group),
        None,
        "the group is a view; no arena node is allocated for it"
    );
    assert_eq!(scan.tree.logical_of(group), totals.direct_logical);

    let empty = common::find(&scan.tree, b"empty").expect("empty dir");
    assert_eq!(
        scan.tree.virtual_group(empty),
        None,
        "a directory with no files of its own has no group"
    );
}

#[test]
fn an_excluded_directory_is_a_flagged_marker_that_is_never_opened() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());

    let mut options = ScanOptions::default();
    options.apply_default_exclusions = false;
    options.exclusions = vec![path_rule("Excluded")];
    let scan = common::scan_with_scanner(
        &Scanner::new().with_engine(Engine::Sequential).with_options(options),
        temp.path(),
    );
    common::restore(&fixture);

    let excluded = common::find(&scan.tree, b"Excluded").expect("the marker node is retained");
    let node = scan.tree.node(excluded).expect("live");
    assert!(node.has_flags(flags::EXCLUDED));
    assert_eq!(scan.tree.child_count(excluded), 0, "it was never opened");
    assert_eq!(scan.counts.excluded_paths, 1);
    assert_eq!(scan.excluded_roots.len(), 1);
    assert!(scan.excluded_roots[0].as_str().ends_with("/Excluded"));
    assert!(
        common::find(&scan.tree, b"skipped.bin").is_none(),
        "nothing inside an excluded directory is retained"
    );
    assert_eq!(
        scan.counts.unreadable_dirs,
        u64::from(fixture.unreadable.is_some()),
        "excluded is counted separately from unreadable"
    );
}

#[test]
fn a_permission_error_marks_the_node_and_the_scan_still_succeeds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    if fixture.unreadable.is_none() {
        common::restore(&fixture);
        return; // running as root: the mode bits would not be enforced
    }
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let locked = common::find(&scan.tree, b"locked").expect("the node is retained");
    let node = scan.tree.node(locked).expect("live");
    assert!(node.has_flags(flags::UNREADABLE));
    assert_eq!(scan.counts.unreadable_dirs, 1);
    assert!(scan.is_partial(), "an unreadable path makes the totals a floor");
    assert!(
        scan.errors
            .iter()
            .any(|error| matches!(error, rdirstat_core::ScanError::PermissionDenied { .. })),
        "the refusal is recorded, not propagated"
    );
    let denied = scan
        .error_counts
        .iter()
        .find(|count| count.class == rdirstat_core::ErrorClass::PermissionDenied)
        .expect("counted by class");
    assert_eq!(denied.count, 1);
}

#[test]
fn a_package_directory_is_flagged_and_still_descended() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let package = common::find(&scan.tree, b"Some.app").expect("package");
    let node = scan.tree.node(package).expect("live");
    assert!(node.has_flags(flags::PACKAGE));
    assert_eq!(
        scan.tree.child_count(package),
        1,
        "the scanner still accounts for the contents; only the UI collapses it"
    );
    let totals = scan.tree.dir_totals(package).expect("directory row");
    assert_eq!(totals.logical, common::PACKAGE_BYTES);
}

#[test]
fn the_rollup_reproduces_a_naive_sum_over_every_leaf() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::Sequential);
    common::restore(&fixture);

    let (logical, allocated) = common::naive_totals(&scan.tree);
    assert_eq!(scan.totals.logical, logical, "root logical == sum of contributions");
    assert_eq!(scan.totals.allocated, allocated);

    let root_totals = scan.tree.dir_totals(scan.tree.root()).expect("root row");
    assert_eq!(root_totals.logical, logical);
    assert_eq!(
        u64::from(root_totals.retained_nodes) + 1,
        scan.counts.retained_nodes,
        "the root's own node is the only one its subtree total does not count"
    );
}

#[test]
fn a_deep_chain_rolls_up_without_recursion() {
    // A real filesystem caps a path at PATH_MAX, so this is as deep as a
    // `TempDir` fixture can go; `synthetic.rs` drives 3 000 levels through the
    // reader trait, where no path limit applies.
    let temp = tempfile::tempdir().expect("temp dir");
    common::build_deep_chain(temp.path(), 120);
    let scan = common::scan_with(temp.path(), Engine::Sequential);

    assert_eq!(scan.counts.directories, 121, "the root plus every level");
    assert_eq!(scan.totals.logical, 7, "the one file at the bottom");
    let (logical, _) = common::naive_totals(&scan.tree);
    assert_eq!(scan.totals.logical, logical);
    assert_eq!(scan.tree.depth(scan.tree.root()).expect("root depth"), 0);
}

#[test]
fn the_shipped_defaults_are_the_ones_docs_02_lists() {
    let set = ExclusionSet::compile(default_exclusions(std::path::Path::new("/"))).expect("compiles");
    let patterns: Vec<&str> = set.rules().iter().map(|rule| rule.pattern.as_str()).collect();
    assert_eq!(
        patterns,
        vec![
            "System/Volumes/Data",
            "System/Volumes/VM",
            "System/Volumes/Preboot",
            "Volumes/*",
            ".Spotlight-V100",
            ".fseventsd",
            ".DocumentRevisions-V100",
            ".TemporaryItems",
        ]
    );
    assert!(
        !patterns.iter().any(|pattern| pattern.contains("Trash")),
        "Trash is included by default because it often explains missing space"
    );
}

#[test]
fn the_result_records_what_actually_ran() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let scan = common::scan_with(temp.path(), Engine::parallel(3));
    common::restore(&fixture);

    assert_eq!(
        scan.options.workers,
        Some(3),
        "the worker count is recorded, not implied"
    );
    assert_eq!(scan.exclusion_hash.as_str().len(), 64);
    assert!(!scan.tool_version.is_empty());
    assert!(scan.finished_unix_ms >= scan.started_unix_ms);
    assert_eq!(scan.volume.device, {
        let metadata = std::fs::metadata(temp.path()).expect("stat");
        std::os::unix::fs::MetadataExt::dev(&metadata)
    });
}

#[test]
fn progress_events_carry_absolute_counters_and_a_terminal_state() {
    use std::sync::{Arc, Mutex};

    use rdirstat_core::{ScanProgress, ScanState};
    use rdirstat_scan::ProgressSink;

    #[derive(Debug, Default)]
    struct Recorder {
        seen: Mutex<Vec<ScanProgress>>,
    }

    impl ProgressSink for Recorder {
        fn publish(&self, progress: &ScanProgress) {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(progress.clone());
            }
        }
    }

    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = common::build(temp.path());
    let recorder = Arc::new(Recorder::default());
    let scan = common::scan_with_scanner(
        &Scanner::new()
            .with_engine(Engine::parallel(2))
            .with_progress(recorder.clone()),
        temp.path(),
    );
    common::restore(&fixture);

    let seen = recorder.seen.lock().expect("not poisoned").clone();
    assert!(seen.len() >= 3, "at least Scanning, Finalizing, and Ready");

    let sequences: Vec<u64> = seen.iter().map(|progress| progress.sequence).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sequences, sorted, "sequence numbers are strictly increasing");

    let first = seen.first().expect("at least one");
    let last = seen.last().expect("at least one");
    assert_eq!(first.state, ScanState::Scanning);
    assert_eq!(last.state, ScanState::Ready, "completion is a state, not a silence");
    assert_eq!(last.pending_dirs, 0, "the exact pending count reaches zero");
    assert_eq!(last.retained_nodes, scan.counts.retained_nodes);
    assert!(
        last.observed_entries >= first.observed_entries,
        "counters are absolute and monotonic"
    );
    assert!(seen.iter().any(|progress| progress.state == ScanState::Finalizing));
}
