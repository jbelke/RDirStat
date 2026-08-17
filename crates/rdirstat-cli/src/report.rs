//! Turning one [`Outcome`] into text or JSON.
//!
//! Both encodings are **bounded**: the tree is cut at `--max-depth` and each
//! directory prints at most `--max-children` rows with an explicit
//! `... +N more` marker. That is the same rule the IPC layer obeys — no output
//! is O(node count) — and it is why `--stats --format json` is safe to run
//! against a 69M-entry volume.

use std::cmp::Reverse;
use std::fmt::Write as _;

use rdirstat_core::{DisplayPath, Kind, Node, NodeId, Tree, flags, format_percent, format_si};
use serde_json::{Value, json};

use crate::cli::{Quantity, ScanArgs};
use crate::engine::Outcome;

/// Bar width, in characters, for the text renderer's `%Bar` column.
const BAR_WIDTH: u64 = 20;

/// One printable row of the tree.
#[derive(Debug)]
pub(crate) struct Row {
    depth: u32,
    name: String,
    logical: u64,
    allocated: u64,
    kind: Kind,
    flags: u16,
    children: u32,
}

impl Row {
    /// The byte count this row is measured by.
    const fn quantity(&self, quantity: Quantity) -> u64 {
        match quantity {
            Quantity::Logical => self.logical,
            Quantity::Allocated => self.allocated,
        }
    }
}

/// Collects the bounded row set, root first.
pub(crate) fn rows(tree: &Tree, max_depth: u32, max_children: u32, quantity: Quantity) -> Vec<Row> {
    let mut out = Vec::new();
    let root = tree.root();
    let Some(node) = tree.node(root) else {
        return out;
    };
    out.push(row_for(tree, root, node, 0));
    collect(tree, root, 0, max_depth, max_children, quantity, &mut out);
    out
}

/// Depth-first, largest-first, bounded on both axes.
fn collect(
    tree: &Tree,
    parent: NodeId,
    depth: u32,
    max_depth: u32,
    max_children: u32,
    quantity: Quantity,
    out: &mut Vec<Row>,
) {
    if depth >= max_depth {
        return;
    }
    let mut children: Vec<(NodeId, u64)> = tree
        .children(parent)
        .map(|id| {
            let size = match quantity {
                Quantity::Logical => tree.logical_of(id),
                Quantity::Allocated => tree.allocated_of(id),
            };
            (id, size)
        })
        .collect();
    // NodeId is the stable tie-breaker, exactly as the query layer sorts.
    children.sort_by_key(|(id, size)| (Reverse(*size), id.raw()));

    let limit = usize::try_from(max_children).unwrap_or(usize::MAX);
    let shown = children.len().min(limit);
    for (id, _) in children.iter().take(shown) {
        let Some(node) = tree.node(*id) else {
            continue;
        };
        out.push(row_for(tree, *id, node, depth + 1));
        if node.kind().is_directory() {
            collect(tree, *id, depth + 1, max_depth, max_children, quantity, out);
        }
    }
    if children.len() > shown {
        let hidden = children.len() - shown;
        let (logical, allocated) = children
            .iter()
            .skip(shown)
            .fold((0_u64, 0_u64), |(logical, allocated), (id, _)| {
                (
                    logical.saturating_add(tree.logical_of(*id)),
                    allocated.saturating_add(tree.allocated_of(*id)),
                )
            });
        out.push(Row {
            depth: depth + 1,
            name: format!("... +{hidden} more"),
            logical,
            allocated,
            kind: Kind::Unknown,
            flags: flags::AGGREGATED,
            children: 0,
        });
    }
}

fn row_for(tree: &Tree, id: NodeId, node: &Node, depth: u32) -> Row {
    let name = tree.name_bytes(id).map_or_else(
        || "?".to_owned(),
        |bytes| DisplayPath::from_bytes(bytes).as_str().to_owned(),
    );
    Row {
        depth,
        name,
        logical: tree.logical_of(id),
        allocated: tree.allocated_of(id),
        kind: node.kind(),
        flags: node.flags,
        children: tree.child_count(id),
    }
}

/// Renders the human-readable report.
///
/// The default order is root-**last**, matching `pdu`; `--top-down` prints
/// root-first. Indentation carries the hierarchy, so reversing the row order is
/// exactly the same tree read from the other end.
pub(crate) fn text(outcome: &Outcome, args: &ScanArgs) -> String {
    let mut out = String::new();
    let scan = &outcome.scan;
    let rows = rows(&scan.tree, args.max_depth, args.max_children, args.quantity);
    let total = match args.quantity {
        Quantity::Logical => scan.totals.logical,
        Quantity::Allocated => scan.totals.allocated,
    };

    let mut lines: Vec<String> = rows
        .iter()
        .map(|row| {
            let value = row.quantity(args.quantity);
            format!(
                "{:>10}  {:>6}  {}  {}{}{}",
                format_si(value),
                format_percent(value, total),
                bar(value, total),
                "  ".repeat(usize::try_from(row.depth).unwrap_or(0)),
                row.name,
                annotation(row)
            )
        })
        .collect();
    if !args.top_down {
        lines.reverse();
    }
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }

    out.push('\n');
    let _ignored = writeln!(
        out,
        "{:>10}  logical      {:>10}  allocated",
        format_si(scan.totals.logical),
        format_si(scan.totals.allocated)
    );
    if scan.is_partial() {
        out.push_str("note: totals are a floor — ");
        let mut reasons = Vec::new();
        if scan.counts.unreadable_dirs > 0 {
            reasons.push(format!("{} unreadable directories", scan.counts.unreadable_dirs));
        }
        if scan.counts.excluded_paths > 0 {
            reasons.push(format!("{} excluded paths", scan.counts.excluded_paths));
        }
        if scan.is_aggregated() {
            reasons.push(format!("{} aggregated nodes", scan.counts.aggregated_nodes));
        }
        if scan.mutations > 0 {
            reasons.push(format!("{} entries changed during the scan", scan.mutations));
        }
        out.push_str(&reasons.join(", "));
        out.push('\n');
    }

    if args.stats {
        out.push('\n');
        out.push_str(&stats_text(outcome));
    }
    out
}

/// The `--stats` block: the measurement surface's whole reason to exist.
pub(crate) fn stats_text(outcome: &Outcome) -> String {
    let scan = &outcome.scan;
    let measured = &outcome.measurement;
    let counts = &scan.counts;
    let mut out = String::new();

    out.push_str("scan\n");
    out.push_str(&pair("root", &scan.root_path.display().to_string()));
    out.push_str(&pair("tool version", &scan.tool_version));
    out.push_str(&pair(
        "volume",
        &format!("{} on {}", scan.volume.fs_type, scan.volume.mount_point),
    ));
    out.push_str(&pair("wall time", &duration(measured.wall_ms)));
    out.push_str(&pair("reader threads", &measured.threads.to_string()));
    out.push_str(&pair(
        "throughput",
        &per_second(counts.observed_entries, measured.wall_ms).map_or_else(
            || "n/a (sub-millisecond)".to_owned(),
            |rate| format!("{rate} entries/s"),
        ),
    ));

    out.push_str("counts\n");
    out.push_str(&pair("observed entries", &counts.observed_entries.to_string()));
    out.push_str(&pair("retained nodes", &counts.retained_nodes.to_string()));
    out.push_str(&pair("directories", &counts.directories.to_string()));
    out.push_str(&pair("files", &counts.files.to_string()));
    out.push_str(&pair("symlinks", &counts.symlinks.to_string()));
    out.push_str(&pair("special", &counts.special.to_string()));
    out.push_str(&pair("hard-link repeats", &counts.hard_link_repeats.to_string()));
    out.push_str(&pair("aggregated nodes", &counts.aggregated_nodes.to_string()));
    out.push_str(&pair("excluded paths", &counts.excluded_paths.to_string()));
    out.push_str(&pair("unreadable dirs", &counts.unreadable_dirs.to_string()));
    out.push_str(&pair("mutations", &scan.mutations.to_string()));

    out.push_str("memory\n");
    out.push_str(&pair(
        "arena (48 B/node)",
        &format!("{} ({} nodes)", format_si(measured.arena_bytes), counts.retained_nodes),
    ));
    out.push_str(&pair(
        "name blob",
        &format!(
            "{} (capacity {}, mean {} B)",
            format_si(measured.name_blob_bytes),
            format_si(measured.name_blob_capacity_bytes),
            measured.mean_name_bytes
        ),
    ));
    out.push_str(&pair(
        "directory index",
        &format!(
            "{} ({} dirs, {} bp of nodes)",
            format_si(measured.dir_index_bytes),
            counts.directories,
            measured.directory_ratio_bp
        ),
    ));
    out.push_str(&pair(
        "peak RSS",
        &measured.peak_rss_bytes.map_or_else(
            || "unmeasured".to_owned(),
            |bytes| format!("{} [{}]", format_si(bytes), crate::rss::RSS_SOURCE),
        ),
    ));

    out.push_str("totals\n");
    out.push_str(&pair("logical", &format_si(scan.totals.logical)));
    out.push_str(&pair("allocated", &format_si(scan.totals.allocated)));

    out.push_str("errors\n");
    if scan.error_counts.is_empty() {
        out.push_str(&pair("none", "0"));
    } else {
        for entry in &scan.error_counts {
            let label = match entry.operation {
                Some(operation) => format!("{:?}/{:?}", entry.class, operation),
                None => format!("{:?}", entry.class),
            };
            out.push_str(&pair(&label, &entry.count.to_string()));
        }
    }
    out
}

/// Renders the JSON document. `--stats` adds the `stats` object.
pub(crate) fn json(outcome: &Outcome, args: &ScanArgs) -> Value {
    let scan = &outcome.scan;
    let measured = &outcome.measurement;
    let mut document = json!({
        "tool_version": scan.tool_version,
        "root": DisplayPath::from_bytes(scan.root_path.as_os_str().as_encoded_bytes()),
        "quantity": args.quantity.key(),
        "started_unix_ms": scan.started_unix_ms,
        "finished_unix_ms": scan.finished_unix_ms,
        "aggregated": scan.is_aggregated(),
        "partial": scan.is_partial(),
        "volume": {
            "device": scan.volume.device,
            "fs_type": scan.volume.fs_type,
            "mount_point": scan.volume.mount_point,
            "case_sensitive": scan.volume.case_sensitive,
        },
        "options": {
            "cross_filesystems": scan.options.cross_filesystems,
            "count_hard_links_once": scan.options.count_hard_links_once,
            "apply_default_exclusions": scan.options.apply_default_exclusions,
            "aggregate_below_bytes": scan.options.aggregate_below_bytes,
            "threads": measured.threads,
            "exclusions": scan.options.exclusions.len(),
            "exclusion_hash": scan.exclusion_hash,
        },
        "totals": {
            "logical": scan.totals.logical,
            "allocated": scan.totals.allocated,
        },
        "counts": scan.counts,
        "excluded_roots": scan.excluded_roots,
        "tree": tree_json(outcome, args),
    });

    if args.stats {
        let stats = json!({
            "observed_entries": scan.counts.observed_entries,
            "retained_nodes": scan.counts.retained_nodes,
            "directories": scan.counts.directories,
            "arena_bytes": measured.arena_bytes,
            "node_bytes": 48,
            "name_blob_bytes": measured.name_blob_bytes,
            "name_blob_capacity_bytes": measured.name_blob_capacity_bytes,
            "dir_index_bytes": measured.dir_index_bytes,
            "mean_name_bytes": measured.mean_name_bytes,
            "directory_ratio_bp": measured.directory_ratio_bp,
            "peak_rss_bytes": measured.peak_rss_bytes,
            "peak_rss_source": measured.peak_rss_bytes.map(|_| crate::rss::RSS_SOURCE),
            "wall_ms": measured.wall_ms,
            "entries_per_second": per_second(scan.counts.observed_entries, measured.wall_ms),
            "threads": measured.threads,
            "mutations": scan.mutations,
            "errors": {
                "total": scan.error_counts.iter().map(|entry| entry.count).sum::<u64>(),
                "detailed_retained": scan.errors.len(),
                "by_class": scan.error_counts,
            },
            "totals": {
                "logical": scan.totals.logical,
                "allocated": scan.totals.allocated,
            },
        });
        if let Some(map) = document.as_object_mut() {
            map.insert("stats".to_owned(), stats);
        }
    }
    document
}

/// The bounded tree projection.
fn tree_json(outcome: &Outcome, args: &ScanArgs) -> Value {
    let rows = rows(&outcome.scan.tree, args.max_depth, args.max_children, args.quantity);
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "depth": row.depth,
                    "name": row.name,
                    "kind": row.kind.key(),
                    "logical": row.logical,
                    "allocated": row.allocated,
                    "children": row.children,
                    "flags": row.flags,
                })
            })
            .collect(),
    )
}

/// A short suffix naming the flags that change how a row should be read.
fn annotation(row: &Row) -> String {
    let mut marks = Vec::new();
    if row.kind.is_directory() {
        marks.push("dir");
    }
    if row.flags & flags::UNREADABLE != 0 {
        marks.push("unreadable");
    }
    if row.flags & flags::EXCLUDED != 0 {
        marks.push("excluded");
    }
    if row.flags & flags::MOUNT_POINT != 0 {
        marks.push("mount");
    }
    if row.flags & flags::FIRMLINK != 0 {
        marks.push("firmlink");
    }
    if row.flags & flags::HARD_LINK_REPEAT != 0 {
        marks.push("hard-link repeat");
    } else if row.flags & flags::HARD_LINK != 0 {
        marks.push("hard link");
    }
    if row.flags & flags::SPARSE != 0 {
        marks.push("sparse");
    }
    if row.flags & flags::PACKAGE != 0 {
        marks.push("package");
    }
    if row.flags & flags::BROKEN_SYMLINK != 0 {
        marks.push("broken link");
    }
    if marks.is_empty() {
        String::new()
    } else {
        format!("  [{}]", marks.join(", "))
    }
}

/// An integer-only proportional bar. No float touches a byte count.
fn bar(part: u64, whole: u64) -> String {
    let filled = if whole == 0 {
        0
    } else {
        let scaled = u128::from(part).saturating_mul(u128::from(BAR_WIDTH)) / u128::from(whole);
        u64::try_from(scaled).unwrap_or(BAR_WIDTH).min(BAR_WIDTH)
    };
    let empty = BAR_WIDTH - filled;
    let mut out = String::with_capacity(usize::try_from(BAR_WIDTH).unwrap_or(20) * 3);
    for _ in 0..filled {
        out.push('█');
    }
    for _ in 0..empty {
        out.push('·');
    }
    out
}

fn pair(label: &str, value: &str) -> String {
    // Two spaces after the column, so a label wider than the column still
    // separates from its value instead of running into it.
    format!("  {label:<22}  {value}\n")
}

fn duration(wall_ms: u64) -> String {
    format!("{}.{:03} s", wall_ms / 1000, wall_ms % 1000)
}

/// Integer entries-per-second, or `None` for a scan too short to measure.
///
/// A sub-millisecond scan has no meaningful rate; reporting the raw count as a
/// rate would be a fabricated number, and dividing by zero is worse.
fn per_second(entries: u64, wall_ms: u64) -> Option<u64> {
    if wall_ms == 0 {
        return None;
    }
    Some(u64::try_from(u128::from(entries).saturating_mul(1000) / u128::from(wall_ms)).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_is_integer_scaled_and_never_overflows_its_width() {
        assert_eq!(
            bar(0, 0).chars().count(),
            usize::try_from(BAR_WIDTH).expect("width fits")
        );
        assert_eq!(
            bar(1, 1).chars().count(),
            usize::try_from(BAR_WIDTH).expect("width fits")
        );
        assert_eq!(
            bar(u64::MAX, 1).chars().count(),
            usize::try_from(BAR_WIDTH).expect("width fits")
        );
        assert!(bar(1, 1).starts_with('█'));
        assert!(bar(0, 100).starts_with('·'));
    }

    #[test]
    fn throughput_is_absent_rather_than_invented_for_an_unmeasurable_scan() {
        assert_eq!(per_second(100, 0), None);
        assert_eq!(per_second(100, 1000), Some(100));
        assert_eq!(per_second(2000, 500), Some(4000));
    }

    #[test]
    fn durations_are_printed_as_seconds_with_milliseconds() {
        assert_eq!(duration(0), "0.000 s");
        assert_eq!(duration(1234), "1.234 s");
    }
}
