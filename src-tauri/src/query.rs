//! Read paths against a published tree: child paging and node details.
//!
//! Every function here is `O(page)` or `O(depth)` in node count and takes the
//! frozen [`Tree`] by reference from a cloned `Arc`, so no application lock is
//! held while it runs.

use std::cmp::Ordering;
use std::hash::BuildHasher;
use std::path::Path;

use rdirstat_core::{
    ChildPage, ChildRow, CompletedScan, Cursor, CursorPayload, Details, DirTotals, DisplayPath, Kind, NodeId,
    QueryError, Sort, SortDirection, SortKey, Tree, clamp_page_limit, flags,
};

use crate::cursor;
use crate::fsident;

/// The label for the synthesized direct-files group.
pub(crate) const VIRTUAL_GROUP_NAME: &str = "<Files>";

/// Directory extensions macOS presents as packages.
const PACKAGE_EXTENSIONS: [&str; 12] = [
    "app",
    "bundle",
    "framework",
    "kext",
    "plugin",
    "component",
    "photoslibrary",
    "rtfd",
    "pkg",
    "xcodeproj",
    "playground",
    "logicx",
];

fn name_bytes(tree: &Tree, node: NodeId) -> &[u8] {
    if node.is_virtual_group() {
        VIRTUAL_GROUP_NAME.as_bytes()
    } else {
        tree.name_bytes(node).unwrap_or_default()
    }
}

fn mtime_of(tree: &Tree, node: NodeId) -> i64 {
    if node.is_virtual_group() {
        return tree
            .dir_totals(node)
            .and_then(|totals| totals.latest_mtime())
            .unwrap_or(0);
    }
    tree.node(node).map_or(0, |entry| entry.mtime)
}

fn category_of(tree: &Tree, node: NodeId) -> u8 {
    if node.is_virtual_group() {
        0
    } else {
        tree.node(node).map_or(0, |entry| entry.category().get())
    }
}

/// Directories sort before files; the group sorts with directories.
fn kind_rank(tree: &Tree, node: NodeId) -> u8 {
    if node.is_virtual_group() {
        return 0;
    }
    match tree.node(node).map(|entry| entry.kind()) {
        Some(Kind::Directory) => 0,
        Some(Kind::File) => 1,
        _ => 2,
    }
}

/// The scalar a [`CursorPayload::last_key`] carries for this sort.
///
/// `Name` has no meaningful scalar, so it stores `0` and the resume comparison
/// falls back entirely to the recorded `last_node`, whose name the immutable
/// tree can always reproduce.
fn numeric_key(tree: &Tree, sort: Sort, node: NodeId) -> i64 {
    match sort.key {
        SortKey::Name => 0,
        SortKey::Allocated => i64::try_from(tree.allocated_of(node)).unwrap_or(i64::MAX),
        SortKey::Mtime => mtime_of(tree, node),
        SortKey::Category => i64::from(category_of(tree, node)),
        SortKey::Kind => i64::from(kind_rank(tree, node)),
        // `Logical` and any future variant.
        _ => i64::try_from(tree.logical_of(node)).unwrap_or(i64::MAX),
    }
}

/// The total order for a child page.
///
/// Ties always break on ascending [`NodeId`], regardless of direction, so the
/// order is total and paging can neither repeat nor skip a row.
fn order(tree: &Tree, sort: Sort, a: NodeId, b: NodeId) -> Ordering {
    let primary = match sort.key {
        SortKey::Name => name_bytes(tree, a).cmp(name_bytes(tree, b)),
        SortKey::Allocated => tree.allocated_of(a).cmp(&tree.allocated_of(b)),
        SortKey::Mtime => mtime_of(tree, a).cmp(&mtime_of(tree, b)),
        SortKey::Category => category_of(tree, a)
            .cmp(&category_of(tree, b))
            .then_with(|| name_bytes(tree, a).cmp(name_bytes(tree, b))),
        SortKey::Kind => kind_rank(tree, a)
            .cmp(&kind_rank(tree, b))
            .then_with(|| name_bytes(tree, a).cmp(name_bytes(tree, b))),
        _ => tree.logical_of(a).cmp(&tree.logical_of(b)),
    };
    let directed = if sort.direction == SortDirection::Ascending {
        primary
    } else {
        primary.reverse()
    };
    directed.then_with(|| a.raw().cmp(&b.raw()))
}

fn row(tree: &Tree, node: NodeId) -> ChildRow {
    let is_virtual_group = node.is_virtual_group();
    ChildRow {
        node,
        name: DisplayPath::from_bytes(name_bytes(tree, node)).as_str().to_owned(),
        // A group is not a filesystem object, so it has no `Kind`. The frontend
        // routes on `is_virtual_group`, never on the kind, for these rows.
        kind: if is_virtual_group {
            Kind::Unknown
        } else {
            tree.node(node).map_or(Kind::Unknown, |entry| entry.kind())
        },
        category: rdirstat_core::CategoryId::from_raw(category_of(tree, node)),
        logical: tree.logical_of(node),
        allocated: tree.allocated_of(node),
        mtime: mtime_of(tree, node),
        flags: if is_virtual_group {
            flags::NONE
        } else {
            tree.node(node).map_or(flags::NONE, |entry| entry.flags)
        },
        children: if is_virtual_group { 0 } else { tree.child_count(node) },
        is_virtual_group,
    }
}

/// One bounded page of `parent`'s children.
///
/// `limit` is **clamped** to [`MAX_CHILD_PAGE`], never rejected: a UI that asks
/// for one row too many should get a page, not a failure.
///
/// # Errors
///
/// [`QueryError::VirtualGroup`] (a group cannot be expanded),
/// [`QueryError::UnknownNode`], or [`QueryError::InvalidCursor`].
pub(crate) fn children<S: BuildHasher>(
    scan: &CompletedScan,
    keys: &S,
    parent: NodeId,
    sort: Sort,
    cursor_text: Option<&Cursor>,
    limit: u32,
) -> Result<ChildPage, QueryError> {
    let tree = &scan.tree;
    let generation = scan.generation;
    if parent.is_virtual_group() {
        return Err(QueryError::VirtualGroup { node: parent });
    }
    if !tree.contains(parent) {
        return Err(QueryError::UnknownNode { node: parent });
    }
    let limit = clamp_page_limit(limit);
    let take = limit as usize;

    let group = tree.virtual_group(parent);
    let total_children = tree.child_count(parent).saturating_add(u32::from(group.is_some()));

    let mut candidates: Vec<NodeId> = Vec::with_capacity(total_children as usize);
    candidates.extend(tree.children(parent));
    candidates.extend(group);

    if let Some(text) = cursor_text {
        let payload = cursor::decode(keys, text, generation, parent, sort)?;
        // The cursor's scalar must still describe its node in *this* tree.
        if numeric_key(tree, sort, payload.last_node) != payload.last_key {
            return Err(QueryError::InvalidCursor);
        }
        if !tree.contains(payload.last_node) && !payload.last_node.is_virtual_group() {
            return Err(QueryError::InvalidCursor);
        }
        candidates.retain(|node| order(tree, sort, *node, payload.last_node) == Ordering::Greater);
    }

    let has_more = candidates.len() > take;
    if has_more && take > 0 {
        candidates.select_nth_unstable_by(take, |a, b| order(tree, sort, *a, *b));
        candidates.truncate(take);
    }
    candidates.sort_unstable_by(|a, b| order(tree, sort, *a, *b));

    let next = if has_more {
        candidates.last().map(|last| {
            cursor::encode(
                keys,
                &CursorPayload {
                    generation,
                    parent,
                    sort,
                    last_key: numeric_key(tree, sort, *last),
                    last_node: *last,
                },
            )
        })
    } else {
        None
    };

    let rows: Vec<ChildRow> = candidates.iter().map(|node| row(tree, *node)).collect();
    debug_assert!(rows.len() <= limit as usize, "a page may never exceed its limit");
    Ok(ChildPage {
        generation,
        parent,
        rows,
        next,
        total_children,
        limit,
    })
}

/// Whether macOS presents this directory as a package.
///
/// Extension first (free), then the on-demand `Contents/Info.plist` probe for
/// the bundles that do not announce themselves in the name.
fn is_package(path: &Path, kind: Kind) -> bool {
    if kind != Kind::Directory {
        return false;
    }
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if PACKAGE_EXTENSIONS.contains(&extension.as_str()) {
        return true;
    }
    path.join("Contents/Info.plist").is_file()
}

/// Everything the details panel shows for one node.
///
/// Performs **one `lstat` on demand** — the arena stores no per-node inode and
/// no freshness bit, so this is where "changed since the scan" is detected. A
/// disagreement adds [`flags::MUTATED`] to the reported flags; it is never an
/// error, because the tree is a past observation and the panel says so.
///
/// # Errors
///
/// [`QueryError::UnknownNode`] or [`QueryError::PathTooDeep`].
pub(crate) fn details(scan: &CompletedScan, node: NodeId) -> Result<Details, QueryError> {
    let tree = &scan.tree;
    let owner = node.group_owner();
    if let Some(owner) = owner
        && !tree.contains(owner)
    {
        return Err(QueryError::UnknownNode { node });
    }
    if owner.is_none() && !tree.contains(node) {
        return Err(QueryError::UnknownNode { node });
    }

    let path_node = owner.unwrap_or(node);
    let path = fsident::node_path(scan, path_node)?;
    let recorded = tree.node(path_node).copied();
    let kind = if owner.is_some() {
        Kind::Unknown
    } else {
        recorded.map_or(Kind::Unknown, rdirstat_core::Node::kind)
    };

    let mut reported_flags = if owner.is_some() {
        flags::NONE
    } else {
        recorded.map_or(flags::NONE, |entry| entry.flags)
    };
    let mut package = false;
    match fsident::observe(&path) {
        Ok(observed) => {
            if let Some(entry) = recorded
                && owner.is_none()
                && entry.kind() == Kind::File
                && (observed.size != entry.size || observed.mtime != entry.mtime)
            {
                reported_flags |= flags::MUTATED;
            }
            package = is_package(&path, observed.kind);
        }
        Err(_) => {
            // Gone, or unreadable now. Either way the recorded numbers are no
            // longer current and the panel must say so.
            reported_flags |= flags::MUTATED;
        }
    }

    let subtree = if let Some(owner) = owner {
        // A group's "subtree" is its direct files only, never the owning
        // directory's whole subtree, which would be a different number.
        tree.dir_totals(owner).map(|totals| DirTotals {
            logical: totals.direct_logical,
            allocated: totals.direct_allocated,
            direct_logical: totals.direct_logical,
            direct_allocated: totals.direct_allocated,
            latest_mtime: totals.latest_mtime,
            observed_entries: u32::from(totals.direct_files > 0) * totals.direct_files,
            retained_nodes: totals.direct_files,
            direct_files: totals.direct_files,
            unreadable: 0,
        })
    } else {
        tree.dir_totals(node).copied()
    };

    let display_name = if owner.is_some() {
        VIRTUAL_GROUP_NAME.to_owned()
    } else {
        DisplayPath::from_bytes(name_bytes(tree, node)).as_str().to_owned()
    };

    Ok(Details {
        generation: scan.generation,
        node,
        path: DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes()),
        name: display_name,
        kind,
        category: rdirstat_core::CategoryId::from_raw(category_of(tree, node)),
        logical: tree.logical_of(node),
        allocated: tree.allocated_of(node),
        mtime: mtime_of(tree, node),
        flags: reported_flags,
        subtree,
        // The arena stores no back-reference from a repeated hard link to the
        // path that counted its bytes; `rdirstat-scan` owns that side table.
        counted_at: None,
        is_package: package,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::Arc;

    use rdirstat_core::{MAX_CHILD_PAGE, ScanId, ScanOptions, TreeGeneration};

    use super::*;
    use crate::engine::{ScanOutcome, ScanRequest};
    use crate::progress::ProgressCounters;
    use crate::state::CancelToken;

    fn scan_of(root: &Path) -> Arc<CompletedScan> {
        let outcome = crate::engine::run(ScanRequest {
            root: root.to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        });
        match outcome {
            ScanOutcome::Completed(scan) => Arc::from(scan),
            other => panic!("expected a completed scan, got {other:?}"),
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<CompletedScan>) {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..25_u32 {
            std::fs::write(
                dir.path().join(format!("f{index:02}.bin")),
                vec![b'x'; (index as usize + 1) * 10],
            )
            .expect("write");
        }
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/deep.bin"), vec![b'y'; 5_000]).expect("write");
        let scan = scan_of(dir.path());
        (dir, scan)
    }

    #[test]
    fn a_page_never_exceeds_its_clamped_limit() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let page = children(&scan, &keys, scan.root, Sort::default(), None, u32::MAX).expect("page");
        assert_eq!(page.limit, MAX_CHILD_PAGE);
        assert!(page.rows.len() <= MAX_CHILD_PAGE as usize);
        assert_eq!(page.generation, scan.generation);
    }

    #[test]
    fn paging_covers_every_child_exactly_once() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let mut seen: Vec<NodeId> = Vec::new();
        let mut cursor_text: Option<Cursor> = None;
        loop {
            let page = children(&scan, &keys, scan.root, Sort::default(), cursor_text.as_ref(), 4).expect("page");
            assert!(page.rows.len() <= 4);
            seen.extend(page.rows.iter().map(|row| row.node));
            match page.next {
                Some(next) => cursor_text = Some(next),
                None => break,
            }
        }
        let total = scan.tree.child_count(scan.root) + u32::from(scan.tree.virtual_group(scan.root).is_some());
        assert_eq!(seen.len(), total as usize, "every child, once");
        let mut deduped = seen.clone();
        deduped.sort_unstable_by_key(|node| node.raw());
        deduped.dedup();
        assert_eq!(deduped.len(), seen.len(), "no row repeated");
    }

    #[test]
    fn the_default_sort_is_largest_logical_first() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let page = children(&scan, &keys, scan.root, Sort::default(), None, 500).expect("page");
        let sizes: Vec<u64> = page.rows.iter().map(|row| row.logical).collect();
        assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "{sizes:?}");
    }

    #[test]
    fn every_sort_key_pages_without_repeating() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        for key in [
            SortKey::Name,
            SortKey::Logical,
            SortKey::Allocated,
            SortKey::Mtime,
            SortKey::Category,
            SortKey::Kind,
        ] {
            for direction in [SortDirection::Ascending, SortDirection::Descending] {
                let sort = Sort { key, direction };
                let mut seen = Vec::new();
                let mut cursor_text: Option<Cursor> = None;
                loop {
                    let page = children(&scan, &keys, scan.root, sort, cursor_text.as_ref(), 3).expect("page");
                    seen.extend(page.rows.iter().map(|row| row.node.raw()));
                    match page.next {
                        Some(next) => cursor_text = Some(next),
                        None => break,
                    }
                }
                let mut deduped = seen.clone();
                deduped.sort_unstable();
                deduped.dedup();
                assert_eq!(deduped.len(), seen.len(), "{key:?}/{direction:?} repeated a row");
            }
        }
    }

    #[test]
    fn the_direct_files_group_appears_and_cannot_be_expanded() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let page = children(&scan, &keys, scan.root, Sort::default(), None, 500).expect("page");
        let group = page
            .rows
            .iter()
            .find(|row| row.is_virtual_group)
            .expect("a directory with direct files gets a group");
        assert_eq!(group.name, VIRTUAL_GROUP_NAME);
        assert_eq!(group.children, 0);
        assert!(group.logical > 0);
        assert_eq!(
            children(&scan, &keys, group.node, Sort::default(), None, 10).expect_err("this call must be rejected"),
            QueryError::VirtualGroup { node: group.node }
        );
    }

    #[test]
    fn a_cursor_from_another_generation_is_rejected() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let page = children(&scan, &keys, scan.root, Sort::default(), None, 3).expect("page");
        let stale = cursor::encode(
            &keys,
            &CursorPayload {
                generation: TreeGeneration::from_raw(99),
                parent: scan.root,
                sort: Sort::default(),
                last_key: 0,
                last_node: page.rows[0].node,
            },
        );
        assert_eq!(
            children(&scan, &keys, scan.root, Sort::default(), Some(&stale), 3)
                .expect_err("this call must be rejected"),
            QueryError::InvalidCursor
        );
    }

    #[test]
    fn an_unknown_parent_is_rejected() {
        let (_dir, scan) = fixture();
        let keys = RandomState::new();
        let missing = NodeId::from_raw(500_000);
        assert_eq!(
            children(&scan, &keys, missing, Sort::default(), None, 10).expect_err("this call must be rejected"),
            QueryError::UnknownNode { node: missing }
        );
    }

    #[test]
    fn details_reconstruct_a_real_path_and_report_the_subtree() {
        let (dir, scan) = fixture();
        let keys = RandomState::new();
        let page = children(&scan, &keys, scan.root, Sort::default(), None, 500).expect("page");
        let sub = page
            .rows
            .iter()
            .find(|row| row.name == "sub")
            .expect("the subdirectory is a child");

        let details = details(&scan, sub.node).expect("details");
        assert_eq!(details.name, "sub");
        assert_eq!(details.kind, Kind::Directory);
        assert_eq!(details.path.as_str(), dir.path().join("sub").to_string_lossy());
        assert_eq!(details.subtree.map(|totals| totals.logical), Some(5_000));
        assert!(!details.is_package);
        assert_eq!(details.generation, scan.generation);
    }

    #[test]
    fn details_flag_a_file_that_changed_after_the_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.bin"), vec![b'x'; 100]).expect("write");
        let scan = scan_of(dir.path());
        let child = scan.tree.children(scan.root).next().expect("one child");
        assert_eq!(details(&scan, child).expect("details").flags & flags::MUTATED, 0);

        std::fs::write(dir.path().join("a.bin"), vec![b'x'; 900]).expect("rewrite");
        let after = details(&scan, child).expect("details");
        assert_ne!(after.flags & flags::MUTATED, 0, "the on-demand lstat must notice");
    }

    #[test]
    fn details_of_a_group_report_direct_files_only() {
        let (_dir, scan) = fixture();
        let group = scan.tree.virtual_group(scan.root).expect("a group");
        let details = details(&scan, group).expect("details");
        assert_eq!(details.name, VIRTUAL_GROUP_NAME);
        let totals = details.subtree.expect("a group has direct totals");
        assert_eq!(totals.logical, totals.direct_logical);
        assert_eq!(totals.direct_files, 25);
        assert!(totals.logical < scan.totals.logical, "the group excludes sub/deep.bin");
    }

    #[test]
    fn a_package_directory_is_detected_by_its_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("Thing.app");
        std::fs::create_dir_all(app.join("Contents")).expect("mkdir");
        std::fs::write(app.join("Contents/Info.plist"), b"<plist/>").expect("write");
        let scan = scan_of(dir.path());
        let child = scan
            .tree
            .children(scan.root)
            .find(|node| scan.tree.name_bytes(*node) == Some(OsStr::new("Thing.app").as_bytes()))
            .expect("the bundle is a child");
        assert!(details(&scan, child).expect("details").is_package);
    }
}
