//! Reveal in Finder and Move to Trash.
//!
//! These are the only two commands that touch the filesystem after a scan, and
//! they are the reason the arena stores components rather than paths. The rules
//! they enforce, in order:
//!
//! 1. The scan root and every virtual `<Files>` group are **not actionable**.
//! 2. The path is reconstructed from `CompletedScan::root_path` plus stored
//!    components — never from anything JavaScript sent.
//! 3. The parent directory is canonicalized and must still resolve inside the
//!    scan root, so a component that became a symlink after the scan cannot
//!    redirect the move.
//! 4. The target is re-`lstat`ed and must still agree on device, kind, size, and
//!    mtime.
//! 5. `move_to_trash` additionally verifies the confirmation token against the
//!    `(dev, ino)` pairs observed **now**, so an object swapped between the
//!    sheet and the confirm button fails closed.
//! 6. The move itself goes through `NSFileManager.trashItemAtURL` so Finder's
//!    "Put Back" works. **`fs::remove_file` is never called, anywhere.**

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::PathBuf;

use rdirstat_core::{
    ActionError, CompletedScan, ConfirmationToken, DisplayPath, MAX_TREE_DEPTH, NodeId, Operation, TrashItemResult,
    TrashPreview, TrashPreviewItem, TrashReport,
};

use crate::fsident;
use crate::token::{self, ItemIdentity};

/// A selection after virtual groups, the scan root, and covered descendants are
/// removed.
#[derive(Debug, Default)]
struct Normalized {
    keep: Vec<NodeId>,
    rejected: Vec<NodeId>,
    dropped_descendants: u32,
}

/// Normalizes a selection.
///
/// A node whose ancestor is also selected is dropped: moving the ancestor moves
/// it too, and counting both would double the bytes the confirmation sheet
/// shows.
fn normalize(scan: &CompletedScan, nodes: &[NodeId]) -> Normalized {
    let mut out = Normalized::default();
    let mut selected: HashSet<u32> = HashSet::with_capacity(nodes.len());
    let mut candidates: Vec<NodeId> = Vec::with_capacity(nodes.len());

    for node in nodes {
        if node.is_virtual_group() || *node == scan.root || !scan.tree.contains(*node) {
            if !out.rejected.contains(node) {
                out.rejected.push(*node);
            }
            continue;
        }
        if selected.insert(node.raw()) {
            candidates.push(*node);
        }
    }

    for node in candidates {
        let mut cursor = node;
        let mut covered = false;
        for _ in 0..MAX_TREE_DEPTH {
            let Some(parent) = scan.tree.parent(cursor) else { break };
            if parent.is_none() {
                break;
            }
            if selected.contains(&parent.raw()) {
                covered = true;
                break;
            }
            cursor = parent;
        }
        if covered {
            out.dropped_descendants = out.dropped_descendants.saturating_add(1);
        } else {
            out.keep.push(node);
        }
    }
    out
}

/// Resolves and revalidates one selected node.
fn resolve(scan: &CompletedScan, node: NodeId) -> Result<(PathBuf, fsident::Observation), ActionError> {
    let path = fsident::action_path(scan, node, false)?;
    fsident::confirm_within_root(scan, &path)?;
    let observation = fsident::revalidate(scan, node, &path)?;
    Ok((path, observation))
}

/// Builds the confirmation sheet's contents and mints the token that authorizes
/// the move.
///
/// Items that no longer match what the scan recorded are reported in
/// [`TrashPreview::rejected`] alongside groups and the scan root: the sheet
/// shows only what will actually move.
pub(crate) fn preview<S: BuildHasher>(
    scan: &CompletedScan,
    keys: &S,
    now_unix_ms: i64,
    nodes: &[NodeId],
) -> TrashPreview {
    let mut normalized = normalize(scan, nodes);
    let mut items = Vec::with_capacity(normalized.keep.len());
    let mut identities = Vec::with_capacity(normalized.keep.len());
    let mut total_logical = 0_u64;
    let mut total_allocated = 0_u64;

    for node in normalized.keep {
        match resolve(scan, node) {
            Ok((path, observation)) => {
                let logical = scan.tree.logical_of(node);
                let allocated = scan.tree.allocated_of(node);
                total_logical = total_logical.saturating_add(logical);
                total_allocated = total_allocated.saturating_add(allocated);
                items.push(TrashPreviewItem {
                    node,
                    path: DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes()),
                    logical,
                    allocated,
                });
                identities.push(ItemIdentity {
                    node,
                    device: observation.device,
                    inode: observation.inode,
                });
            }
            Err(_) => normalized.rejected.push(node),
        }
    }

    TrashPreview {
        generation: scan.generation,
        token: token::mint(keys, scan.generation, now_unix_ms, &identities),
        items,
        total_logical,
        total_allocated,
        dropped_descendants: normalized.dropped_descendants,
        rejected: normalized.rejected,
    }
}

/// Moves a confirmed selection to the Trash.
///
/// # Errors
///
/// [`ActionError::InvalidConfirmation`] when the token does not bind this
/// generation and the `(dev, ino)` set observed right now. That is deliberately
/// strict: if anything in the selection changed between the sheet and the
/// confirm, the user re-previews rather than trashing something they did not
/// see counted.
pub(crate) fn move_to_trash<S: BuildHasher>(
    scan: &CompletedScan,
    keys: &S,
    now_unix_ms: i64,
    nodes: &[NodeId],
    confirmation: &ConfirmationToken,
) -> Result<TrashReport, ActionError> {
    let normalized = normalize(scan, nodes);

    // Re-observe every survivor first, so the token is checked against the
    // filesystem as it is *now*, not as the preview left it.
    let mut resolved: Vec<(NodeId, PathBuf, Result<fsident::Observation, ActionError>)> =
        Vec::with_capacity(normalized.keep.len());
    let mut identities = Vec::with_capacity(normalized.keep.len());
    for node in &normalized.keep {
        match fsident::action_path(scan, *node, false) {
            Ok(path) => {
                let outcome =
                    fsident::confirm_within_root(scan, &path).and_then(|()| fsident::revalidate(scan, *node, &path));
                match &outcome {
                    Ok(observation) => identities.push(ItemIdentity {
                        node: *node,
                        device: observation.device,
                        inode: observation.inode,
                    }),
                    // A sentinel identity: it cannot match the preview's digest,
                    // so the whole batch fails closed rather than moving a
                    // partially-changed selection.
                    Err(_) => identities.push(ItemIdentity {
                        node: *node,
                        device: 0,
                        inode: 0,
                    }),
                }
                resolved.push((*node, path, outcome));
            }
            Err(error) => {
                identities.push(ItemIdentity {
                    node: *node,
                    device: 0,
                    inode: 0,
                });
                resolved.push((*node, PathBuf::new(), Err(error)));
            }
        }
    }

    token::verify(keys, confirmation, scan.generation, now_unix_ms, &identities)?;

    let context = trash_context();
    let mut report = TrashReport {
        generation: scan.generation,
        requested: u32::try_from(resolved.len()).unwrap_or(u32::MAX),
        moved: 0,
        failed: 0,
        items: Vec::with_capacity(resolved.len()),
    };

    for (node, path, outcome) in resolved {
        let original = DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());
        let scan_time_logical = scan.tree.logical_of(node);
        let scan_time_allocated = scan.tree.allocated_of(node);
        let error = match outcome {
            Err(error) => Some(error),
            Ok(_) => match context.delete(&path) {
                Ok(()) => None,
                Err(error) => Some(map_trash_error(&original, &error)),
            },
        };
        if error.is_some() {
            report.failed = report.failed.saturating_add(1);
        } else {
            report.moved = report.moved.saturating_add(1);
        }
        report.items.push(TrashItemResult {
            node,
            original,
            // `trash::TrashContext::delete` discards the resulting item URL that
            // `trashItemAtURL:resultingItemURL:error:` returns, so the landing
            // path is not reportable without an ObjC bridge of our own.
            trashed_to: None,
            scan_time_logical,
            scan_time_allocated,
            error,
        });
    }
    Ok(report)
}

#[cfg(target_os = "macos")]
fn trash_context() -> trash::TrashContext {
    use trash::macos::{DeleteMethod, TrashContextExtMacos as _};
    let mut context = trash::TrashContext::new();
    // `NSFileManager.trashItemAtURL` — not the Finder AppleScript path, and
    // emphatically not `fs::remove_file`. This is what records the "Put Back"
    // information macOS needs.
    context.set_delete_method(DeleteMethod::NsFileManager);
    context
}

#[cfg(not(target_os = "macos"))]
fn trash_context() -> trash::TrashContext {
    trash::TrashContext::new()
}

fn map_trash_error(path: &DisplayPath, error: &trash::Error) -> ActionError {
    match error {
        trash::Error::CouldNotAccess { .. } => ActionError::PermissionDenied {
            path: path.clone(),
            operation: Operation::Trash,
            os_code: 0,
        },
        other => ActionError::TrashFailed {
            path: path.clone(),
            reason: other.to_string(),
        },
    }
}

/// Reveals a node in Finder.
///
/// # Errors
///
/// [`ActionError::NotActionable`] for the scan root or a virtual group,
/// [`ActionError::ChangedSinceScan`] if the object is gone, or
/// [`ActionError::OutsideScanRoot`].
pub(crate) fn reveal(scan: &CompletedScan, node: NodeId) -> Result<(), ActionError> {
    let (path, _) = resolve(scan, node)?;
    tauri_plugin_opener::reveal_item_in_dir(&path).map_err(|error| ActionError::TrashFailed {
        path: DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes()),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;
    use std::path::Path;
    use std::sync::Arc;

    use rdirstat_core::{ScanId, ScanOptions, TreeGeneration};

    use super::*;
    use crate::engine::{ScanOutcome, ScanRequest};
    use crate::progress::ProgressCounters;
    use crate::state::CancelToken;

    const NOW: i64 = 1_800_000_000_000;

    fn scan_of(root: &Path) -> Arc<CompletedScan> {
        match crate::engine::run(ScanRequest {
            root: root.to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        }) {
            ScanOutcome::Completed(scan) => Arc::from(scan),
            other => panic!("expected a completed scan, got {other:?}"),
        }
    }

    fn child_named(scan: &CompletedScan, parent: NodeId, name: &str) -> NodeId {
        scan.tree
            .children(parent)
            .find(|node| scan.tree.name_bytes(*node) == Some(name.as_bytes()))
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    fn fixture() -> (tempfile::TempDir, Arc<CompletedScan>) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("keep/inner")).expect("mkdir");
        std::fs::write(dir.path().join("keep/inner/deep.bin"), vec![b'z'; 300]).expect("write");
        std::fs::write(dir.path().join("junk.bin"), vec![b'x'; 1_000]).expect("write");
        let scan = scan_of(dir.path());
        (dir, scan)
    }

    #[test]
    fn the_scan_root_and_groups_are_never_actionable() {
        let (_dir, scan) = fixture();
        let group = scan.tree.virtual_group(scan.root).expect("a group");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[scan.root, group]);
        assert!(preview.items.is_empty());
        assert_eq!(preview.rejected.len(), 2);
        assert_eq!(
            reveal(&scan, scan.root).expect_err("this call must be rejected"),
            ActionError::NotActionable { node: scan.root }
        );
        assert_eq!(
            reveal(&scan, group).expect_err("this call must be rejected"),
            ActionError::NotActionable { node: group }
        );
    }

    #[test]
    fn a_descendant_of_a_selected_ancestor_is_dropped() {
        let (_dir, scan) = fixture();
        let keep = child_named(&scan, scan.root, "keep");
        let inner = child_named(&scan, keep, "inner");
        let deep = child_named(&scan, inner, "deep.bin");

        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[keep, inner, deep]);
        assert_eq!(preview.items.len(), 1, "only the ancestor moves");
        assert_eq!(preview.items[0].node, keep);
        assert_eq!(preview.dropped_descendants, 2);
        assert_eq!(preview.total_logical, 300);
    }

    #[test]
    fn a_preview_token_authorizes_exactly_its_own_selection() {
        let (dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[junk]);
        assert_eq!(preview.items.len(), 1);
        assert_eq!(preview.total_logical, 1_000);

        let report = move_to_trash(&scan, &keys, NOW, &[junk], &preview.token).expect("authorized");
        assert_eq!(report.moved, 1);
        assert_eq!(report.failed, 0);
        assert!(!dir.path().join("junk.bin").exists(), "the file left its directory");
        assert_eq!(report.items[0].scan_time_logical, 1_000);
    }

    #[test]
    fn a_token_for_one_selection_cannot_move_another() {
        let (_dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keep = child_named(&scan, scan.root, "keep");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[junk]);
        assert_eq!(
            move_to_trash(&scan, &keys, NOW, &[keep], &preview.token).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn no_token_means_nothing_moves() {
        let (dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keys = RandomState::new();
        assert_eq!(
            move_to_trash(
                &scan,
                &keys,
                NOW,
                &[junk],
                &ConfirmationToken::from_encoded(String::new())
            )
            .expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
        assert!(dir.path().join("junk.bin").exists(), "the file must still be there");
    }

    #[test]
    fn a_file_that_changed_after_the_preview_blocks_the_move() {
        let (dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[junk]);

        // Replace the file: same path, different inode and different bytes.
        std::fs::remove_file(dir.path().join("junk.bin")).expect("remove");
        std::fs::write(dir.path().join("junk.bin"), vec![b'q'; 1_000]).expect("rewrite");

        assert_eq!(
            move_to_trash(&scan, &keys, NOW, &[junk], &preview.token).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
        assert!(dir.path().join("junk.bin").exists(), "the replacement must survive");
    }

    #[test]
    fn a_vanished_file_is_reported_as_changed_not_moved() {
        let (dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[junk]);
        std::fs::remove_file(dir.path().join("junk.bin")).expect("remove");
        assert_eq!(
            move_to_trash(&scan, &keys, NOW, &[junk], &preview.token).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn an_expired_token_is_refused() {
        let (dir, scan) = fixture();
        let junk = child_named(&scan, scan.root, "junk.bin");
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[junk]);
        assert_eq!(
            move_to_trash(&scan, &keys, NOW + token::TOKEN_TTL_MS + 1, &[junk], &preview.token)
                .expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
        assert!(dir.path().join("junk.bin").exists());
    }

    #[test]
    fn a_path_that_escapes_the_root_through_a_new_symlink_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("branch")).expect("mkdir");
        std::fs::write(dir.path().join("branch/victim.bin"), vec![b'x'; 10]).expect("write");
        std::fs::write(outside.path().join("victim.bin"), vec![b'x'; 10]).expect("write");
        let scan = scan_of(dir.path());
        let branch = child_named(&scan, scan.root, "branch");
        let victim = child_named(&scan, branch, "victim.bin");

        // After the scan, `branch` becomes a symlink pointing elsewhere.
        std::fs::remove_dir_all(dir.path().join("branch")).expect("rm");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("branch")).expect("symlink");

        let error = reveal(&scan, victim).expect_err("this call must be rejected");
        assert!(
            matches!(error, ActionError::OutsideScanRoot { .. }),
            "expected OutsideScanRoot, got {error:?}"
        );
        assert!(outside.path().join("victim.bin").exists(), "the decoy is untouched");
    }

    #[test]
    fn an_unknown_node_is_rejected_before_any_syscall() {
        let (_dir, scan) = fixture();
        let missing = NodeId::from_raw(400_000);
        let keys = RandomState::new();
        let preview = preview(&scan, &keys, NOW, &[missing]);
        assert!(preview.items.is_empty());
        assert_eq!(preview.rejected, vec![missing]);
        assert_eq!(
            reveal(&scan, missing).expect_err("this call must be rejected"),
            ActionError::UnknownNode { node: missing }
        );
    }
}
