//! End-to-end tests: the real binary, real fixtures, real `du`.
//!
//! Every fixture is built inside a `TempDir` and nothing is ever written
//! outside it. The `verify` cases are the valuable ones — each is a specific
//! accounting bug that would otherwise only surface as a wrong number in the
//! UI months later.

#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot be built must abort the test loudly; these helpers are test code even though they sit outside a #[test] fn"
)]

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

/// The binary under test, as Cargo built it.
const BINARY: &str = env!("CARGO_BIN_EXE_rdirstat-cli");

fn run(args: &[&str]) -> Output {
    Command::new(BINARY).args(args).output().expect("the binary runs")
}

fn write(path: &Path, bytes: &[u8]) {
    let mut file = fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

/// A fixture with nesting, a large file, a small file, and a symlink.
fn fixture(root: &Path) {
    fs::create_dir_all(root.join("project/src")).expect("mkdir");
    fs::create_dir_all(root.join("project/target/debug")).expect("mkdir");
    write(&root.join("project/src/main.rs"), &vec![b'a'; 4_096]);
    write(&root.join("project/target/debug/binary"), &vec![b'b'; 262_144]);
    write(&root.join("project/README.md"), &vec![b'c'; 512]);
    std::os::unix::fs::symlink(root.join("project/src"), root.join("project/link-to-src")).expect("symlink");
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn scan_stats_json_reports_every_number_the_measurement_gate_needs() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());

    let output = run(&[
        "scan",
        &dir.path().to_string_lossy(),
        "--stats",
        "--format",
        "json",
        "--progress",
        "never",
    ]);
    assert!(output.status.success(), "scan exits zero: {output:?}");

    let document: Value = serde_json::from_str(&stdout_of(&output)).expect("stdout is one JSON document");
    let stats = document.get("stats").expect("--stats emits a stats object");

    for key in [
        "observed_entries",
        "retained_nodes",
        "arena_bytes",
        "name_blob_bytes",
        "name_blob_capacity_bytes",
        "dir_index_bytes",
        "wall_ms",
        "threads",
        "entries_per_second",
        "mean_name_bytes",
        "directory_ratio_bp",
        "mutations",
    ] {
        assert!(stats.get(key).is_some(), "stats.{key} is present");
    }
    assert!(
        stats.get("peak_rss_bytes").is_some(),
        "peak RSS is reported, even as null"
    );
    assert!(stats.get("errors").and_then(|errors| errors.get("by_class")).is_some());

    let totals = stats.get("totals").expect("totals");
    let logical = totals.get("logical").and_then(Value::as_u64).expect("logical");
    let allocated = totals.get("allocated").and_then(Value::as_u64).expect("allocated");
    assert_eq!(logical, 4_096 + 262_144 + 512 + symlink_target_length(dir.path()));
    assert!(allocated >= 4_096 + 262_144 + 512);

    // The arena line the whole memory budget rests on: 48 bytes a node.
    let nodes = stats.get("retained_nodes").and_then(Value::as_u64).expect("nodes");
    let arena = stats.get("arena_bytes").and_then(Value::as_u64).expect("arena");
    assert_eq!(arena, nodes * 48);
    assert_eq!(stats.get("node_bytes").and_then(Value::as_u64), Some(48));
}

/// `st_size` of a symlink is the length of its target, and it lands in the
/// logical total. Allocated is unaffected, which is why `verify` compares
/// allocated.
fn symlink_target_length(root: &Path) -> u64 {
    let link = root.join("project/link-to-src");
    let metadata = fs::symlink_metadata(link).expect("symlink metadata");
    metadata.len()
}

#[test]
fn scan_text_is_root_last_by_default_and_root_first_with_top_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let path = dir.path().to_string_lossy().into_owned();

    let bottom_up = stdout_of(&run(&["scan", &path, "--progress", "never"]));
    let top_down = stdout_of(&run(&["scan", &path, "--top-down", "--progress", "never"]));

    let root_name = dir.path().to_string_lossy().into_owned();
    let first_line_of = |text: &str| text.lines().next().unwrap_or_default().to_owned();
    assert!(
        first_line_of(&top_down).contains(&root_name),
        "--top-down puts the root first"
    );
    assert!(
        !first_line_of(&bottom_up).contains(&root_name),
        "the default puts the root last, like pdu"
    );
    assert!(bottom_up.contains("logical"));
    assert!(bottom_up.contains("allocated"));
}

#[test]
fn quantity_selects_which_number_the_rows_are_measured_by() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A sparse file: logical 1 MiB, allocated ~0. The two quantities must
    // disagree, and the flag must be what chooses between them.
    let sparse = dir.path().join("sparse");
    let file = fs::File::create(&sparse).expect("create");
    file.set_len(1_048_576).expect("truncate");
    drop(file);

    let path = dir.path().to_string_lossy().into_owned();
    let logical: Value = serde_json::from_str(&stdout_of(&run(&[
        "scan",
        &path,
        "--format",
        "json",
        "--quantity",
        "logical",
        "--progress",
        "never",
    ])))
    .expect("json");
    let allocated: Value = serde_json::from_str(&stdout_of(&run(&[
        "scan",
        &path,
        "--format",
        "json",
        "--quantity",
        "allocated",
        "--progress",
        "never",
    ])))
    .expect("json");

    assert_eq!(logical.pointer("/quantity").and_then(Value::as_str), Some("logical"));
    assert_eq!(
        allocated.pointer("/quantity").and_then(Value::as_str),
        Some("allocated")
    );
    assert_eq!(
        logical.pointer("/totals/logical").and_then(Value::as_u64),
        Some(1_048_576)
    );
    assert!(
        logical.pointer("/totals/allocated").and_then(Value::as_u64) < Some(1_048_576),
        "a sparse file allocates less than it claims"
    );
}

#[test]
fn exclusions_are_reported_and_never_descended() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let path = dir.path().to_string_lossy().into_owned();

    let document: Value = serde_json::from_str(&stdout_of(&run(&[
        "scan",
        &path,
        "--format",
        "json",
        "--exclude",
        "target",
        "--progress",
        "never",
    ])))
    .expect("json");

    assert_eq!(
        document.pointer("/counts/excluded_paths").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(document.pointer("/partial").and_then(Value::as_bool), Some(true));
    let total = document
        .pointer("/totals/logical")
        .and_then(Value::as_u64)
        .expect("total");
    assert!(total < 262_144, "the excluded subtree's bytes are not counted: {total}");
}

#[test]
fn verify_agrees_with_du_on_a_plain_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let output = run(&["verify", &dir.path().to_string_lossy(), "--progress", "never"]);
    let text = stdout_of(&output);
    assert!(output.status.success(), "verify exits zero on agreement:\n{text}");
    assert!(text.contains("AGREE"), "{text}");
}

#[test]
fn verify_agrees_when_hard_links_would_otherwise_double_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("original"), &vec![b'x'; 131_072]);
    for index in 0..8 {
        fs::hard_link(dir.path().join("original"), dir.path().join(format!("link-{index}"))).expect("link");
    }
    let output = run(&[
        "verify",
        &dir.path().to_string_lossy(),
        "--format",
        "json",
        "--progress",
        "never",
    ]);
    let document: Value = serde_json::from_str(&stdout_of(&output)).expect("json");
    assert_eq!(
        document.get("agrees").and_then(Value::as_bool),
        Some(true),
        "hard links are counted once, exactly as du counts them: {document}"
    );
    assert_eq!(
        document.pointer("/counts/hard_link_repeats").and_then(Value::as_u64),
        Some(8)
    );
    assert!(output.status.success());
}

#[test]
fn verify_agrees_when_a_symlink_points_at_a_large_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("payload")).expect("mkdir");
    write(&dir.path().join("payload/big"), &vec![b'y'; 524_288]);
    std::os::unix::fs::symlink(dir.path().join("payload"), dir.path().join("mirror")).expect("symlink");
    std::os::unix::fs::symlink(dir.path().join("payload/big"), dir.path().join("mirror-file")).expect("symlink");

    let output = run(&["verify", &dir.path().to_string_lossy(), "--progress", "never"]);
    let text = stdout_of(&output);
    assert!(
        text.contains("AGREE"),
        "a followed symlink would double the total:\n{text}"
    );
    assert!(output.status.success());
}

#[test]
fn verify_agrees_on_a_deep_tree_where_dot_entries_would_recurse_forever() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut path = dir.path().to_path_buf();
    for level in 0..24 {
        path = path.join(format!("level-{level}"));
        fs::create_dir(&path).expect("mkdir");
        write(&path.join("payload"), &vec![b'z'; 1_024]);
    }
    let output = run(&["verify", &dir.path().to_string_lossy(), "--progress", "never"]);
    let text = stdout_of(&output);
    assert!(text.contains("AGREE"), "{text}");
    assert!(output.status.success());
}

#[test]
fn verify_reports_a_json_verdict_with_the_exact_du_invocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let output = run(&[
        "verify",
        &dir.path().to_string_lossy(),
        "--format",
        "json",
        "--progress",
        "never",
    ]);
    let document: Value = serde_json::from_str(&stdout_of(&output)).expect("json");
    let command = document.get("command").and_then(Value::as_str).expect("command");
    assert!(command.contains("du"), "{command}");
    assert!(command.contains("-skx"), "device boundaries are off, so du gets -x");
    assert!(document.get("delta_bytes").is_some());
    assert!(document.get("scan_allocated_bytes").is_some());
    assert!(document.get("du_kib").is_some());
}

#[test]
fn a_missing_root_fails_with_a_message_and_a_non_zero_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("not-here");
    let output = run(&["scan", &missing.to_string_lossy(), "--progress", "never"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rdirstat:"), "{stderr}");
    assert!(output.stdout.is_empty(), "nothing is printed to stdout on failure");
}

#[test]
fn output_stays_bounded_when_a_directory_has_many_children() {
    let dir = tempfile::tempdir().expect("tempdir");
    for index in 0..200 {
        write(&dir.path().join(format!("file-{index:03}")), &[b'q'; 64]);
    }
    let document: Value = serde_json::from_str(&stdout_of(&run(&[
        "scan",
        &dir.path().to_string_lossy(),
        "--format",
        "json",
        "--max-children",
        "5",
        "--progress",
        "never",
    ])))
    .expect("json");
    let rows = document.get("tree").and_then(Value::as_array).expect("tree rows");
    // Root, five children, one "+N more" marker. Never 201.
    assert_eq!(rows.len(), 7, "{rows:?}");
    let names: Vec<&str> = rows.iter().filter_map(|row| row.get("name")?.as_str()).collect();
    assert!(names.iter().any(|name| name.starts_with("... +195 more")), "{names:?}");
}

#[test]
fn the_help_text_documents_the_pdu_comparable_flags() {
    let output = run(&["scan", "--help"]);
    let text = stdout_of(&output);
    for flag in [
        "--quantity",
        "--top-down",
        "--exclude",
        "--cross-filesystems",
        "--threads",
        "--aggregate-below",
        "--stats",
        "--format",
    ] {
        assert!(text.contains(flag), "`scan --help` documents {flag}");
    }
}
