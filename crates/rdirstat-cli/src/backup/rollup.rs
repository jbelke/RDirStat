//! Directory roll-ups: the files a wave does not plan one by one land as one
//! item per directory (rds-fkk.11).
//!
//! # Why
//!
//! One `ditto` + compare + receipt per file cannot land millions of files: a
//! `--floor 0` plan of a boot Data volume ranks only the review cap (50,000)
//! and used to leave millions of smaller files "not ranked". With roll-ups,
//! every file that is not planned on its own (below the floor, or past the
//! cap) is covered by a directory item instead.
//!
//! # Which directories
//!
//! After the fit has chosen the individual items, the scan tree is walked
//! once, bottom up, and each directory is judged:
//!
//! - A **tree** roll-up ([`RollupScope::Tree`]) is the *highest* directory
//!   whose whole subtree holds no skip-rule path, no individually planned or
//!   deferred item, no candidate skipped for a change since the scan, no
//!   unreadable, excluded, mount-point or firmlink directory, and no socket,
//!   FIFO or device, and is at most [`MAX_TREE_FILES`] files and
//!   [`MAX_TREE_BYTES`] logical bytes. Skip-rule subtrees are excluded by
//!   construction, not by filtering a copy: a directory that holds one is
//!   never a tree roll-up, so `ditto <dir>` can only copy what the plan
//!   describes, and the verify (`compare_tree`) compares exactly that.
//! - A directory that cannot be a tree roll-up gets a **files** roll-up
//!   ([`RollupScope::Files`]) of its own regular files and symlinks, minus an
//!   explicit, token-bound list of names that are not in it (planned on their
//!   own, skipped by a rule, changed since the scan). Its subdirectories are
//!   judged the same way, recursively.
//!
//! So there is one files item per directory on the path to a skipped or
//! individually planned item (or over a cap), and one tree item per clean
//! child of such a directory. Sibling subtrees are not packed together: a
//! directory over the caps with many small subdirectories gives one item per
//! subdirectory, and a large volume can plan hundreds of thousands of items.
//! A files item is not capped either; its memory at apply grows with the
//! directory's loose files.
//!
//! # Already landed
//!
//! The drive's own catalog (`<dest>/.rdirstat/catalog`: file receipts and
//! roll-up members) is read at plan time. A file it already holds is not
//! planned on its own, and a roll-up whose every file it holds is not planned
//! at all, so apply never sees them. "Holds" is therefore proven at plan
//! time, not estimated ([`LandedKeys`]): the source file's path, size, inode
//! and nanosecond mtime match a row, and the mirror path still has a file of
//! that kind and size. Apply checks what it does see again, by the same
//! identity ([`Landed`]).
//!
//! # Landing (apply)
//!
//! Before copying, the source scope is walked and must match the plan: the
//! directory's `(dev, ino, mtime_ns)`, the file count, the logical bytes and
//! the newest mtime. Anything else is `changed_since_scan` for the item.
//!
//! - A tree roll-up whose mirror directory does not exist is one `ditto` of
//!   the directory into a staging name beside it, then `compare_tree`, then
//!   a no-replace rename into place once the source is proven unchanged.
//! - Otherwise (a files roll-up, or a mirror directory that already exists,
//!   partly from an earlier wave or a failed run) the scope is *merged*:
//!   nothing at the destination is ever overwritten. Missing regular files
//!   are landed per directory in batches (one `ditto` per chunk, each file
//!   compared, then renamed into place without replacing anything); a
//!   missing symlink is made directly and a missing subdirectory is one
//!   `ditto`. A file that is there and whose receipt or member row matches
//!   the source identity is already landed. A file that is there without one
//!   is compared with the source: equal, it is adopted; different, it is left
//!   alone and reported (`destination_occupied`).
//!
//! The walk is repeated after landing; a difference is
//! `changed_during_copy` and nothing is recorded. The staged tree is
//! removed, and a merge removes the copies it made of entries that changed:
//! left behind without member rows, they would be refused as occupied in
//! every later wave. Otherwise every file that
//! landed or was adopted gets a `members` row (with its SHA-256), and, when
//! all of the scope landed, the item gets one receipt with method `rollup`.
//! Roll-up files are never clone sources and never cloned: deduplication
//! stays with the files planned one by one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use rdirstat_catalog::{METHOD_ROLLUP, ReceiptRow, read_receipts, visit_members};
use rdirstat_copy::{
    compare_tree, land, land_files, partial_path, rename_no_replace, sha256_file, sha256_hex, staging_name,
};
use rdirstat_core::{CompletedScan, Kind, NodeId, flags};
use rdirstat_organize::{ItemKind, PlanItem, Rollup, RollupScope, Rules, SkippedItem};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::plan::RULE_CHANGED_SINCE_SCAN;

/// Most files a tree roll-up holds. A larger directory is split: its loose
/// files become a files roll-up and its subdirectories are judged on their
/// own. Bounds a tree item's memory at apply and what one failed copy costs;
/// files items are not bounded by it.
pub(crate) const MAX_TREE_FILES: u64 = 100_000;
/// Most logical bytes a tree roll-up holds, for the same reason.
pub(crate) const MAX_TREE_BYTES: u64 = 64 << 30;

/// Directory flags that keep a subtree out of a tree roll-up: what the scan
/// could not read or deliberately did not enter is not something a plan can
/// describe, and `ditto` would copy it anyway.
const TAINT_FLAGS: u16 = flags::UNREADABLE | flags::MOUNT_POINT | flags::FIRMLINK | flags::EXCLUDED;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A 128-bit digest of some fields; collisions are not a concern at this
/// width for the tens of millions of rows a drive can hold.
fn key(parts: &[&[u8]]) -> u128 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    u128::from_le_bytes(out)
}

fn plan_key(source_path: &[u8], size: u64, mtime_s: i64) -> u128 {
    key(&[source_path, &size.to_le_bytes(), &mtime_s.to_le_bytes()])
}

fn exact_key(source_path: &[u8], size: u64, ino: u64, mtime_ns: i64, dest_path: &[u8]) -> u128 {
    key(&[
        source_path,
        &size.to_le_bytes(),
        &ino.to_le_bytes(),
        &mtime_ns.to_le_bytes(),
        dest_path,
    ])
}

/// What the drive's own catalog says has landed, for the plan.
///
/// The scan only knows a file's path, size and whole-second mtime, so those
/// are the first, cheap test (`cheap`). A file that passes it is then
/// `lstat`ed on the source and must match a receipt or member row by inode
/// and nanosecond mtime too, at the mirror path the plan would give it
/// (`exact`), and that mirror path must still hold a file of the same kind
/// and size. Only then is it "already landed" and left out of the plan: a
/// file rewritten within the same second, or a landed copy since deleted or
/// truncated on the drive, is planned again. Two `lstat`s per landed file is
/// the price; a file the drive has never seen costs none.
#[derive(Debug, Default)]
pub(crate) struct LandedKeys {
    cheap: HashSet<u128>,
    exact: HashSet<u128>,
    /// `<dest>/Backups/<label>`.
    mirror_root: PathBuf,
}

impl LandedKeys {
    /// Reads file receipts and roll-up members for `label` from `catalog`
    /// (the destination's mirror catalog), whose mirror tree is
    /// `mirror_root`. A missing catalog is empty.
    ///
    /// # Errors
    ///
    /// An unreadable part.
    pub(crate) fn load(catalog: &Path, mirror_root: &Path, label: &str) -> anyhow::Result<Self> {
        let mut landed = Self {
            mirror_root: mirror_root.to_path_buf(),
            ..Self::default()
        };
        for row in read_receipts(catalog)? {
            if row.source_label == label && row.method != METHOD_ROLLUP {
                landed.insert(
                    row.source_path.as_bytes(),
                    row.size,
                    row.source_ino,
                    row.mtime_ns,
                    row.dest_path.as_bytes(),
                );
            }
        }
        visit_members(catalog, |row| {
            if row.source_label == label {
                landed.insert(
                    row.source_path.as_bytes(),
                    row.size,
                    row.source_ino,
                    row.mtime_ns,
                    row.dest_path.as_bytes(),
                );
            }
        })?;
        Ok(landed)
    }

    fn insert(&mut self, source: &[u8], size: u64, ino: u64, mtime_ns: i64, dest: &[u8]) {
        self.cheap
            .insert(plan_key(source, size, mtime_ns.div_euclid(1_000_000_000)));
        self.exact.insert(exact_key(source, size, ino, mtime_ns, dest));
    }

    /// `absolute` (at `relative` under the source root, as the scan saw it:
    /// `size` bytes, whole-second `mtime_s`) is on the drive, proven as the
    /// type documents.
    pub(crate) fn contains(&self, absolute: &Path, relative: &Path, size: u64, mtime_s: i64) -> bool {
        let source_bytes = absolute.as_os_str().as_bytes();
        if self.cheap.is_empty() || !self.cheap.contains(&plan_key(source_bytes, size, mtime_s)) {
            return false;
        }
        let Ok(meta) = std::fs::symlink_metadata(absolute) else {
            return false;
        };
        let kind = meta.file_type();
        if !(kind.is_file() || kind.is_symlink()) || meta.size() != size {
            return false;
        }
        let dest = self.mirror_root.join(relative);
        if !self.exact.contains(&exact_key(
            source_bytes,
            size,
            meta.ino(),
            clamp_i64(mtime_ns(&meta)),
            dest.as_os_str().as_bytes(),
        )) {
            return false;
        }
        std::fs::symlink_metadata(&dest).is_ok_and(|landed| {
            landed.file_type().is_file() == kind.is_file()
                && landed.file_type().is_symlink() == kind.is_symlink()
                && landed.size() == size
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.exact.len()
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Files and logical bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Count {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

impl Count {
    fn add(&mut self, files: u64, bytes: u64) {
        self.files = self.files.saturating_add(files);
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

/// Where every file (regular file or symlink) of the scan went. The parts
/// add up to every file the scan saw outside excluded directories.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Coverage {
    /// Planned (or deferred) on their own.
    pub(crate) individual: Count,
    /// Inside a roll-up item (planned or deferred).
    pub(crate) rolled_up: Count,
    /// Already on the drive, with nothing else in their roll-up to land.
    pub(crate) landed: Count,
    /// Under a skip rule, or a candidate skipped as changed since the scan.
    pub(crate) skipped: Count,
    /// In none of the above: a name the plan file cannot carry (not UTF-8).
    pub(crate) not_covered: Count,
    /// Directories the scan could not read; their contents are unknown.
    pub(crate) unreadable_dirs: u64,
}

/// The roll-up candidates of a scan, before the budget.
#[derive(Debug, Default)]
pub(crate) struct RollupCandidates {
    pub(crate) items: Vec<PlanItem>,
    /// Directories whose `lstat` at plan time disagrees with the scan.
    pub(crate) changed: Vec<SkippedItem>,
    pub(crate) coverage: Coverage,
}

/// What the roll-up pass needs from the plan.
pub(crate) struct RollupInput<'a> {
    pub(crate) scan: &'a CompletedScan,
    pub(crate) source: &'a Path,
    pub(crate) base: &'a Path,
    pub(crate) rules: &'a Rules,
    /// Relative paths planned or deferred on their own.
    pub(crate) individual: &'a HashSet<PathBuf>,
    /// Relative paths of candidates the plan lists as skipped for a reason
    /// that is not a rule (changed since the scan, non-UTF-8 name).
    pub(crate) other_skips: &'a HashSet<PathBuf>,
    pub(crate) landed: &'a LandedKeys,
}

/// Per-directory totals over its whole subtree.
#[derive(Debug, Default, Clone, Copy)]
struct Agg {
    files: u64,
    bytes: u64,
    allocated: u64,
    newest: i64,
    landed_files: u64,
    landed_bytes: u64,
    /// Something below makes a tree roll-up impossible.
    taint: bool,
    /// This directory is under a skip rule (itself or an ancestor).
    skipped: bool,
    /// This directory is itself planned on its own (a package), or skipped
    /// for a reason that is not a rule.
    excluded: bool,
}

/// How a non-directory child of a directory is treated.
enum Leaf {
    Skipped,
    Individual,
    Member {
        landed: bool,
    },
    /// Not a regular file or symlink, or flagged: never a member.
    Other,
}

fn is_file_like(kind: Kind) -> bool {
    matches!(kind, Kind::File | Kind::Symlink)
}

/// Builds the roll-up candidates. The source is only `lstat`ed (one call per
/// candidate directory); everything else comes from the scan.
pub(crate) fn candidates(input: &RollupInput<'_>) -> RollupCandidates {
    let aggs = aggregate(input);
    let mut out = RollupCandidates::default();
    emit(input, &aggs, input.scan.root, input.base.to_path_buf(), true, &mut out);
    out
}

fn classify(
    input: &RollupInput<'_>,
    relative: &Path,
    kind: Kind,
    node: &rdirstat_core::Node,
    under_skip: bool,
) -> Leaf {
    if under_skip || input.rules.skipped_by(relative).is_some() || input.other_skips.contains(relative) {
        return Leaf::Skipped;
    }
    if input.individual.contains(relative) {
        return Leaf::Individual;
    }
    if !is_file_like(kind) || node.flags & TAINT_FLAGS != 0 {
        return Leaf::Other;
    }
    let landed = input
        .landed
        .contains(&input.source.join(relative), relative, node.size, node.mtime);
    Leaf::Member { landed }
}

/// Bottom-up totals for every directory. Paths are built on the way down
/// and dropped; only the per-directory totals are kept.
fn aggregate(input: &RollupInput<'_>) -> HashMap<NodeId, Agg> {
    let tree = &input.scan.tree;
    let mut aggs: HashMap<NodeId, Agg> = HashMap::new();
    // Pre-order list of (dir, parent); folded in reverse.
    let mut order: Vec<(NodeId, Option<NodeId>)> = Vec::new();
    let mut stack: Vec<(NodeId, Option<NodeId>, PathBuf, bool)> =
        vec![(input.scan.root, None, input.base.to_path_buf(), false)];
    while let Some((dir, parent, relative, under_skip)) = stack.pop() {
        let mut agg = Agg {
            skipped: under_skip,
            ..Agg::default()
        };
        if let Some(node) = tree.node(dir) {
            agg.newest = node.mtime;
            if node.flags & TAINT_FLAGS != 0 {
                agg.taint = true;
            }
        }
        if parent.is_some() {
            agg.excluded = input.individual.contains(&relative) || input.other_skips.contains(&relative);
            if agg.excluded {
                agg.taint = true;
            }
        }
        for id in tree.children(dir) {
            let (Some(node), Some(name)) = (tree.node(id), tree.name_bytes(id)) else {
                continue;
            };
            let child = relative.join(std::ffi::OsStr::from_bytes(name));
            let kind = node.kind();
            if kind == Kind::Directory {
                let skipped = under_skip || input.rules.skipped_by(&child).is_some();
                stack.push((id, Some(dir), child, skipped));
                continue;
            }
            agg.newest = agg.newest.max(node.mtime);
            match classify(input, &child, kind, node, under_skip) {
                Leaf::Skipped | Leaf::Individual | Leaf::Other => {
                    agg.taint = true;
                    if is_file_like(kind) {
                        agg.files += 1;
                        if kind == Kind::File {
                            agg.bytes = agg.bytes.saturating_add(node.size);
                            agg.allocated = agg.allocated.saturating_add(node.alloc);
                        }
                    }
                }
                Leaf::Member { landed } => {
                    agg.files += 1;
                    let bytes = if kind == Kind::File { node.size } else { 0 };
                    agg.bytes = agg.bytes.saturating_add(bytes);
                    if kind == Kind::File {
                        agg.allocated = agg.allocated.saturating_add(node.alloc);
                    }
                    if landed {
                        agg.landed_files += 1;
                        agg.landed_bytes = agg.landed_bytes.saturating_add(bytes);
                    }
                }
            }
        }
        if under_skip {
            agg.taint = true;
        }
        aggs.insert(dir, agg);
        order.push((dir, parent));
    }
    for &(dir, parent) in order.iter().rev() {
        let Some(parent) = parent else { continue };
        let Some(child) = aggs.get(&dir).copied() else {
            continue;
        };
        if let Some(up) = aggs.get_mut(&parent) {
            up.files = up.files.saturating_add(child.files);
            up.bytes = up.bytes.saturating_add(child.bytes);
            up.allocated = up.allocated.saturating_add(child.allocated);
            up.newest = up.newest.max(child.newest);
            up.landed_files = up.landed_files.saturating_add(child.landed_files);
            up.landed_bytes = up.landed_bytes.saturating_add(child.landed_bytes);
            up.taint |= child.taint;
        }
    }
    aggs
}

/// `(dev, ino, mtime_ns)` of a directory now, if it is still a directory
/// with the scan's whole-second mtime.
fn restat_dir(absolute: &Path, scan_mtime_s: i64) -> Option<(u64, u64, i128)> {
    let meta = std::fs::symlink_metadata(absolute).ok()?;
    if !meta.file_type().is_dir() || meta.mtime() != scan_mtime_s {
        return None;
    }
    Some((
        meta.dev(),
        meta.ino(),
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
    ))
}

fn changed_dir(relative: &Path, allocated: u64) -> SkippedItem {
    SkippedItem {
        relative: relative.to_path_buf(),
        rule_id: RULE_CHANGED_SINCE_SCAN.to_owned(),
        reason: "directory changed or vanished between the scan and the plan".to_owned(),
        allocated,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one top-down pass; splitting it would thread the same six values through helpers"
)]
fn emit(
    input: &RollupInput<'_>,
    aggs: &HashMap<NodeId, Agg>,
    dir: NodeId,
    relative: PathBuf,
    is_root: bool,
    out: &mut RollupCandidates,
) {
    let tree = &input.scan.tree;
    let Some(agg) = aggs.get(&dir).copied() else {
        return;
    };
    let Some(node) = tree.node(dir) else { return };
    let absolute = input.source.join(&relative);
    let utf8 = relative.to_str().is_some() && !relative.as_os_str().is_empty();

    if node.flags & flags::UNREADABLE != 0 {
        out.coverage.unreadable_dirs += 1;
    }

    // The highest directory that may be one tree roll-up.
    let tree_ok =
        !is_root && utf8 && !agg.taint && agg.files > 0 && agg.files <= MAX_TREE_FILES && agg.bytes <= MAX_TREE_BYTES;
    if tree_ok {
        if agg.landed_files == agg.files {
            out.coverage.landed.add(agg.files, agg.bytes);
            return;
        }
        if let Some((dev, ino, mtime_ns)) = restat_dir(&absolute, node.mtime) {
            out.coverage.rolled_up.add(agg.files, agg.bytes);
            out.items.push(PlanItem {
                relative,
                absolute,
                kind: ItemKind::Dir,
                logical: agg.bytes,
                allocated: agg.allocated,
                dev,
                ino,
                mtime_ns,
                rollup: Some(Rollup {
                    scope: RollupScope::Tree,
                    files: agg.files,
                    newest_mtime_s: agg.newest,
                    exclude: Vec::new(),
                }),
            });
            return;
        }
        // Changed since the scan: judge its parts instead, so what did not
        // change still lands. Its own files will be reported as changed.
    }

    // Loose files of this directory.
    let mut members = Count::default();
    let mut members_allocated = 0_u64;
    let mut landed = Count::default();
    let mut newest = i64::MIN;
    let mut exclude: Vec<Vec<u8>> = Vec::new();
    let mut subdirs: Vec<(NodeId, PathBuf)> = Vec::new();
    for id in tree.children(dir) {
        let (Some(child_node), Some(name)) = (tree.node(id), tree.name_bytes(id)) else {
            continue;
        };
        let child = relative.join(std::ffi::OsStr::from_bytes(name));
        let kind = child_node.kind();
        if kind == Kind::Directory {
            subdirs.push((id, child));
            continue;
        }
        let file_like = is_file_like(kind);
        let bytes = if kind == Kind::File { child_node.size } else { 0 };
        match classify(input, &child, kind, child_node, agg.skipped) {
            Leaf::Member { landed: is_landed } => {
                members.add(1, bytes);
                members_allocated =
                    members_allocated.saturating_add(if kind == Kind::File { child_node.alloc } else { 0 });
                newest = newest.max(child_node.mtime);
                if is_landed {
                    landed.add(1, bytes);
                }
            }
            Leaf::Skipped => {
                exclude.push(name.to_vec());
                if file_like {
                    out.coverage.skipped.add(1, bytes);
                }
            }
            Leaf::Individual => {
                exclude.push(name.to_vec());
                if file_like {
                    out.coverage.individual.add(1, bytes);
                }
            }
            Leaf::Other => {
                exclude.push(name.to_vec());
                if file_like {
                    out.coverage.not_covered.add(1, bytes);
                }
            }
        }
    }
    if members.files > 0 {
        let names: Option<Vec<String>> = exclude
            .iter()
            .map(|name| std::str::from_utf8(name).ok().map(str::to_owned))
            .collect();
        match names {
            _ if landed.files == members.files => out.coverage.landed.add(members.files, members.bytes),
            // The volume root's own files are a files roll-up too; its
            // mirror directory is the mirror root, `Backups/<label>`.
            Some(mut names) if utf8 || relative.as_os_str().is_empty() => {
                if let Some((dev, ino, mtime_ns)) = restat_dir(&absolute, node.mtime) {
                    names.sort();
                    out.coverage.rolled_up.add(members.files, members.bytes);
                    out.items.push(PlanItem {
                        relative: relative.clone(),
                        absolute: absolute.clone(),
                        kind: ItemKind::Dir,
                        logical: members.bytes,
                        allocated: members_allocated,
                        dev,
                        ino,
                        mtime_ns,
                        rollup: Some(Rollup {
                            scope: RollupScope::Files,
                            files: members.files,
                            newest_mtime_s: newest,
                            exclude: names,
                        }),
                    });
                } else {
                    out.coverage.skipped.add(members.files, members.bytes);
                    out.changed.push(changed_dir(&relative, members_allocated));
                }
            }
            _ => out.coverage.not_covered.add(members.files, members.bytes),
        }
    }

    for (id, child) in subdirs {
        let Some(child_agg) = aggs.get(&id).copied() else {
            continue;
        };
        let Some(child_node) = tree.node(id) else { continue };
        if child_agg.skipped {
            out.coverage.skipped.add(child_agg.files, child_agg.bytes);
            continue;
        }
        if child_agg.excluded {
            if input.individual.contains(&child) {
                out.coverage.individual.add(child_agg.files, child_agg.bytes);
            } else {
                out.coverage.skipped.add(child_agg.files, child_agg.bytes);
            }
            continue;
        }
        if child_node.flags & (flags::MOUNT_POINT | flags::FIRMLINK | flags::EXCLUDED) != 0 {
            continue;
        }
        emit(input, aggs, id, child, false, out);
    }
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// What the drive's mirror catalog says about files at destination paths,
/// for the merge: `dest path key -> (source path key, ino, size, mtime_ns)`.
#[derive(Debug, Default)]
pub(crate) struct Landed {
    by_dest: HashMap<u128, (u128, u64, u64, i64)>,
}

fn path_key(path: &[u8]) -> u128 {
    key(&[path])
}

/// The destination directories of a plan's roll-ups: the only places a
/// merge looks files up, so the only rows [`Landed::load`] keeps.
#[derive(Debug, Default)]
pub(crate) struct RollupDests {
    /// Mirror directories of tree roll-ups: any file below one.
    pub(crate) trees: HashSet<PathBuf>,
    /// Mirror directories of files roll-ups: their direct files only.
    pub(crate) files: HashSet<PathBuf>,
}

impl RollupDests {
    pub(crate) fn is_empty(&self) -> bool {
        self.trees.is_empty() && self.files.is_empty()
    }

    fn covers(&self, dest: &Path) -> bool {
        let Some(parent) = dest.parent() else {
            return false;
        };
        self.files.contains(parent) || (!self.trees.is_empty() && parent.ancestors().any(|a| self.trees.contains(a)))
    }
}

impl Landed {
    /// Loads the members and file receipts of `catalog` (the drive's mirror;
    /// `None` loads nothing, so nothing is trusted) whose destination is in
    /// one of `dests`. A plan without roll-ups loads nothing, and a drive
    /// holding tens of millions of member rows costs memory only for the
    /// directories this plan merges into.
    ///
    /// # Errors
    ///
    /// An unreadable part.
    pub(crate) fn load(mirror: Option<&Path>, receipts: &[ReceiptRow], dests: &RollupDests) -> anyhow::Result<Self> {
        let mut landed = Self::default();
        let Some(mirror) = mirror else {
            return Ok(landed);
        };
        if dests.is_empty() {
            return Ok(landed);
        }
        for row in receipts.iter().filter(|r| r.method != METHOD_ROLLUP) {
            if dests.covers(Path::new(&row.dest_path)) {
                landed.insert(
                    row.dest_path.as_bytes(),
                    row.source_path.as_bytes(),
                    row.source_ino,
                    row.size,
                    row.mtime_ns,
                );
            }
        }
        visit_members(mirror, |row| {
            if dests.covers(Path::new(row.dest_path)) {
                landed.insert(
                    row.dest_path.as_bytes(),
                    row.source_path.as_bytes(),
                    row.source_ino,
                    row.size,
                    row.mtime_ns,
                );
            }
        })?;
        Ok(landed)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_dest.len()
    }

    fn insert(&mut self, dest: &[u8], source: &[u8], ino: u64, size: u64, mtime_ns: i64) {
        self.by_dest
            .insert(path_key(dest), (path_key(source), ino, size, mtime_ns));
    }

    fn matches(&self, dest: &Path, source: &Path, entry: &Entry) -> bool {
        self.by_dest.get(&path_key(dest.as_os_str().as_bytes()))
            == Some(&(
                path_key(source.as_os_str().as_bytes()),
                entry.ino,
                entry.size,
                clamp_i64(entry.mtime_ns),
            ))
    }

    pub(crate) fn record(&mut self, row: &ReceiptRow) {
        self.insert(
            row.dest_path.as_bytes(),
            row.source_path.as_bytes(),
            row.source_ino,
            row.size,
            row.mtime_ns,
        );
    }
}

fn clamp_i64(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EntryKind {
    Dir,
    File,
    Symlink,
}

/// One entry of a roll-up's scope at apply time.
#[derive(Debug, Clone)]
struct Entry {
    /// Relative to the roll-up directory.
    relative: PathBuf,
    kind: EntryKind,
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

/// The scope as it is on the source now.
#[derive(Debug)]
struct ScopeWalk {
    dev: u64,
    ino: u64,
    mtime_ns: i128,
    files: u64,
    bytes: u64,
    newest: i64,
    /// Sockets, FIFOs, devices, or a directory on another filesystem.
    other: u64,
    /// Sorted by path, parents before children.
    entries: Vec<Entry>,
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

/// Walks the source scope: the whole tree, or the directory's own files and
/// symlinks minus the exclusions. Only `lstat` and `readdir`.
fn walk_scope(root: &Path, rollup: &Rollup) -> std::io::Result<ScopeWalk> {
    let meta = std::fs::symlink_metadata(root)?;
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::other("is no longer a directory"));
    }
    let mut walk = ScopeWalk {
        dev: meta.dev(),
        ino: meta.ino(),
        mtime_ns: mtime_ns(&meta),
        files: 0,
        bytes: 0,
        newest: i64::MIN,
        other: 0,
        entries: Vec::new(),
    };
    let exclude: HashSet<&[u8]> = rollup.exclude.iter().map(String::as_bytes).collect();
    let tree = rollup.scope == RollupScope::Tree;
    if tree {
        walk.newest = meta.mtime();
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        for entry in std::fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let name = entry.file_name();
            if !tree && exclude.contains(name.as_bytes()) {
                continue;
            }
            let child = relative.join(&name);
            let meta = std::fs::symlink_metadata(root.join(&child))?;
            let file_type = meta.file_type();
            let kind = if file_type.is_dir() {
                if !tree {
                    continue;
                }
                if meta.dev() != walk.dev {
                    walk.other += 1;
                    continue;
                }
                stack.push(child.clone());
                EntryKind::Dir
            } else if file_type.is_file() {
                walk.files += 1;
                walk.bytes = walk.bytes.saturating_add(meta.size());
                EntryKind::File
            } else if file_type.is_symlink() {
                walk.files += 1;
                EntryKind::Symlink
            } else {
                if tree {
                    walk.other += 1;
                }
                continue;
            };
            walk.newest = walk.newest.max(meta.mtime());
            walk.entries.push(Entry {
                relative: child,
                kind,
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.size(),
                mtime_ns: mtime_ns(&meta),
            });
        }
    }
    walk.entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(walk)
}

/// `Ok` when the walk is what the plan describes.
fn matches_plan(walk: &ScopeWalk, item: &PlanItem, rollup: &Rollup) -> Result<(), String> {
    let mut changes = Vec::new();
    if walk.dev != item.dev {
        changes.push("dev".to_owned());
    }
    if walk.ino != item.ino {
        changes.push("ino".to_owned());
    }
    if walk.mtime_ns != item.mtime_ns {
        changes.push("directory mtime".to_owned());
    }
    if walk.files != rollup.files {
        changes.push(format!("file count ({} now, {} planned)", walk.files, rollup.files));
    }
    if walk.bytes != item.logical {
        changes.push(format!("size ({} now, {} planned)", walk.bytes, item.logical));
    }
    if walk.files > 0 && walk.newest != rollup.newest_mtime_s {
        changes.push("newest mtime".to_owned());
    }
    if walk.other > 0 {
        changes.push(format!("{} special file(s) or mount point(s) appeared", walk.other));
    }
    if changes.is_empty() {
        Ok(())
    } else {
        Err(format!("{} changed since the scan", changes.join(", ")))
    }
}

/// What happened to a roll-up item, for the apply report.
#[derive(Debug)]
pub(crate) enum RollupOutcome {
    /// Everything in scope is on the drive. `copied` files were written by
    /// this run, `adopted` were there already and compared equal, the rest
    /// were already receipted.
    Landed {
        copied: u64,
        adopted: u64,
        already: u64,
        members: Vec<ReceiptRow>,
        receipt: Option<ReceiptRow>,
    },
    ChangedSinceScan(String),
    /// The source moved under the copy; nothing is recorded. `partial` is a
    /// path this run wrote that is still on disk (a copy of a file that did
    /// not change), never a directory that was there before the run.
    ChangedDuringCopy {
        reason: String,
        partial: Option<PathBuf>,
    },
    /// Some files did not land. `members` are the ones that did and
    /// verified; they are recorded, the item's receipt is not.
    Incomplete {
        failed: u64,
        detail: String,
        partial: Option<PathBuf>,
        members: Vec<ReceiptRow>,
    },
}

/// The fields a member or receipt row takes from the run.
pub(crate) struct RowContext<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) source_label: &'a str,
}

/// The first problem of an item and the first partial it left.
#[derive(Debug, Default)]
struct Problems {
    first: Option<String>,
    partial: Option<PathBuf>,
}

impl Problems {
    fn note(&mut self, problem: String, at: Option<PathBuf>) {
        if self.first.is_none() {
            self.first = Some(problem);
        }
        if self.partial.is_none() {
            self.partial = at;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Copied,
    Adopted,
    Already,
    Failed,
    Occupied,
}

/// Lands one roll-up item at `dest` (its mirror directory, already checked
/// by the caller to be under the mirror root with no symlink on the way).
pub(crate) fn land_rollup(
    item: &PlanItem,
    rollup: &Rollup,
    dest: &Path,
    landed: &Landed,
    context: &RowContext<'_>,
    existing_receipt: Option<&ReceiptRow>,
    cancel: &AtomicBool,
) -> RollupOutcome {
    let walk = match walk_scope(&item.absolute, rollup) {
        Ok(walk) => walk,
        Err(error) => return RollupOutcome::ChangedSinceScan(format!("cannot read the source: {error}")),
    };
    if let Err(reason) = matches_plan(&walk, item, rollup) {
        return RollupOutcome::ChangedSinceScan(reason);
    }

    let mut states: Vec<State> = vec![State::Failed; walk.entries.len()];
    let mut problems = Problems::default();

    // A tree roll-up whose mirror directory does not exist is copied whole
    // into a staging name beside it and moved into place only once the
    // source is proven not to have moved under the copy. A copy of a source
    // that changed is not a backup of anything the plan described; left at
    // the mirror path without member rows, every changed file in it would
    // read as someone else's file (`destination_occupied`) in every later
    // wave.
    let fast = rollup.scope == RollupScope::Tree && std::fs::symlink_metadata(dest).is_err();
    let staging = dest.with_file_name(staging_name());
    let mut written: Vec<Written> = Vec::new();
    if fast {
        match land(&item.absolute, &staging, None, cancel) {
            Ok(_) => states.fill(State::Copied),
            Err(error) => {
                let at = partial_path(&error).cloned();
                problems.note(error.to_string(), at);
            }
        }
    } else {
        written = merge(&item.absolute, &walk, dest, landed, cancel, &mut states, &mut problems);
    }

    #[cfg(test)]
    after_copy_hook(&item.relative, dest);

    // Did the source move under the copy? Then no proof is about the plan.
    let after = walk_scope(&item.absolute, rollup);
    let verdict = match &after {
        Ok(after) => matches_plan(after, item, rollup),
        Err(error) => Err(format!("cannot read the source: {error}")),
    };
    if let Err(reason) = verdict {
        let partial = if fast {
            discard_staging(&staging)
        } else {
            discard_changed(&walk, after.as_ref().ok(), written)
        };
        return RollupOutcome::ChangedDuringCopy { reason, partial };
    }
    if fast
        && states.iter().all(|state| *state == State::Copied)
        && let Err(error) = rename_no_replace(&staging, dest)
    {
        states.fill(State::Failed);
        problems.note(
            format!("moving the verified copy into place: {error}"),
            Some(staging.clone()),
        );
    }

    let Tally {
        copied,
        adopted,
        already,
        failed,
        occupied,
        members,
    } = collect(&item.absolute, &walk, &states, dest, context, &mut problems);

    if failed + occupied > 0 {
        let total = walk.files;
        return RollupOutcome::Incomplete {
            failed,
            detail: format!(
                "{} of {total} files did not land ({failed} failed, {occupied} occupied); first: {}",
                failed + occupied,
                problems.first.unwrap_or_default()
            ),
            partial: problems.partial,
            members,
        };
    }

    let manifest = manifest_digest(&walk);
    let receipt = ReceiptRow {
        run_id: context.run_id.to_owned(),
        source_label: context.source_label.to_owned(),
        source_path: item.absolute.to_string_lossy().into_owned(),
        source_dev: item.dev,
        source_ino: item.ino,
        size: item.logical,
        mtime_ns: clamp_i64(item.mtime_ns),
        dest_path: dest.to_string_lossy().into_owned(),
        method: METHOD_ROLLUP.to_owned(),
        sha256: manifest,
        verified_at_ms: super::now_ms(),
    };
    let same = existing_receipt.is_some_and(|old| {
        old.method == receipt.method
            && old.source_path == receipt.source_path
            && old.source_ino == receipt.source_ino
            && old.size == receipt.size
            && old.mtime_ns == receipt.mtime_ns
            && old.sha256 == receipt.sha256
    });
    RollupOutcome::Landed {
        copied,
        adopted,
        already,
        members,
        receipt: (!same || copied + adopted > 0).then_some(receipt),
    }
}

/// A path this run wrote at the destination, and the entries it holds
/// (`start..end` of the walk: one file, or a whole subdirectory).
#[derive(Debug)]
struct Written {
    start: usize,
    end: usize,
    target: PathBuf,
}

/// The merge: lands what is missing at the destination and checks what is
/// there, never writing over anything. `states` is filled per entry. Returns
/// what this run wrote.
///
/// Missing regular files are landed per directory in one batch
/// ([`land_files`]: one `ditto` process per chunk of up to thousands of
/// files), not one process each. A missing symlink is made directly, and a
/// missing subdirectory is one `ditto` of the whole subtree.
fn merge(
    source_root: &Path,
    walk: &ScopeWalk,
    dest: &Path,
    landed: &Landed,
    cancel: &AtomicBool,
    states: &mut [State],
    problems: &mut Problems,
) -> Vec<Written> {
    let mut written = Vec::new();
    // Missing regular files, by their directory relative to the roll-up.
    let mut batches: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    let mut index = 0;
    while index < walk.entries.len() {
        let entry = &walk.entries[index];
        let source = source_root.join(&entry.relative);
        let target = dest.join(&entry.relative);
        // Entries below this one (a directory), which share its fate when
        // it is landed or refused whole.
        let subtree_end = walk.entries[index + 1..]
            .iter()
            .position(|e| !e.relative.starts_with(&entry.relative))
            .map_or(walk.entries.len(), |offset| index + 1 + offset);
        let whole = |states: &mut [State], state: State| {
            for slot in states.iter_mut().take(subtree_end).skip(index) {
                *slot = state;
            }
        };
        match std::fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && entry.kind == EntryKind::File => {
                let parent = entry.relative.parent().map(Path::to_path_buf).unwrap_or_default();
                batches.entry(parent).or_default().push(index);
                index += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match land(&source, &target, None, cancel) {
                    Ok(_) => {
                        whole(states, State::Copied);
                        written.push(Written {
                            start: index,
                            end: subtree_end,
                            target,
                        });
                    }
                    Err(error) => {
                        whole(states, State::Failed);
                        let at = partial_path(&error).cloned();
                        problems.note(error.to_string(), at);
                    }
                }
                index = subtree_end;
            }
            Err(error) => {
                whole(states, State::Failed);
                problems.note(format!("{}: {error}", target.display()), None);
                index = subtree_end;
            }
            Ok(meta) => {
                let file_type = meta.file_type();
                match entry.kind {
                    EntryKind::Dir if file_type.is_dir() => {
                        states[index] = State::Already;
                        index += 1;
                    }
                    EntryKind::File | EntryKind::Symlink
                        if (file_type.is_file() == (entry.kind == EntryKind::File))
                            && (file_type.is_symlink() == (entry.kind == EntryKind::Symlink))
                            && meta.size() == entry.size
                            && landed.matches(&target, &source, entry) =>
                    {
                        states[index] = State::Already;
                        index += 1;
                    }
                    EntryKind::File | EntryKind::Symlink => {
                        match compare_tree(&source, &target) {
                            Ok(_) => states[index] = State::Adopted,
                            Err(error) => {
                                states[index] = State::Occupied;
                                problems.note(
                                    format!("{} is already there and differs: {error}", target.display()),
                                    None,
                                );
                            }
                        }
                        index += 1;
                    }
                    EntryKind::Dir => {
                        whole(states, State::Occupied);
                        problems.note(
                            format!("{} is already there and is not a directory", target.display()),
                            None,
                        );
                        index = subtree_end;
                    }
                }
            }
        }
    }

    land_batches(
        source_root,
        walk,
        dest,
        &batches,
        cancel,
        states,
        problems,
        &mut written,
    );
    written.sort_by(|a, b| a.target.cmp(&b.target));
    written
}

/// Lands the merge's missing regular files, one [`land_files`] call per
/// directory.
#[allow(clippy::too_many_arguments, reason = "the merge's own state, threaded through")]
fn land_batches(
    source_root: &Path,
    walk: &ScopeWalk,
    dest: &Path,
    batches: &BTreeMap<PathBuf, Vec<usize>>,
    cancel: &AtomicBool,
    states: &mut [State],
    problems: &mut Problems,
    written: &mut Vec<Written>,
) {
    for (parent, indices) in batches {
        let names: Vec<std::ffi::OsString> = indices
            .iter()
            .map(|&i| walk.entries[i].relative.file_name().unwrap_or_default().to_owned())
            .collect();
        let results = land_files(&source_root.join(parent), &names, &dest.join(parent), cancel);
        for (&i, result) in indices.iter().zip(results) {
            match result {
                Ok(_) => {
                    states[i] = State::Copied;
                    written.push(Written {
                        start: i,
                        end: i + 1,
                        target: dest.join(&walk.entries[i].relative),
                    });
                }
                Err(error) => {
                    states[i] = State::Failed;
                    let at = partial_path(&error).cloned();
                    problems.note(error.to_string(), at);
                }
            }
        }
    }
}

/// Removes a fast path's staged tree after the source moved under it.
/// `Some` names it only if it could not be removed.
fn discard_staging(staging: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(staging).ok()?;
    std::fs::remove_dir_all(staging).err().map(|_| staging.to_path_buf())
}

/// After the source moved under a merge: removes what this run wrote for
/// entries that are no longer what was copied (gone, or another inode, size
/// or mtime, or for a directory, entries added or removed below it), and
/// returns the first path it wrote that is still there.
///
/// A copy of a file that then changed proves nothing and has no member row;
/// left in place it would be compared with the changed source in the next
/// wave, differ, and be refused as someone else's file in every wave after.
/// What this run wrote is its own output: no one else's file is removed.
/// When the source cannot be read at all, nothing is removed.
fn discard_changed(walk: &ScopeWalk, after: Option<&ScopeWalk>, written: Vec<Written>) -> Option<PathBuf> {
    let Some(after) = after else {
        return written.into_iter().next().map(|w| w.target);
    };
    let now: HashMap<&Path, &Entry> = after.entries.iter().map(|e| (e.relative.as_path(), e)).collect();
    let below = |entries: &[Entry], root: &Path| entries.iter().filter(|e| e.relative.starts_with(root)).count();
    let mut kept = None;
    for w in written {
        let copied = walk.entries.get(w.start..w.end).unwrap_or_default();
        let same = copied.iter().all(|before| {
            now.get(before.relative.as_path()).is_some_and(|e| {
                e.kind == before.kind && e.ino == before.ino && e.size == before.size && e.mtime_ns == before.mtime_ns
            })
        }) && copied
            .first()
            .is_none_or(|root| root.kind != EntryKind::Dir || below(&after.entries, &root.relative) == copied.len());
        let removed = !same
            && match std::fs::symlink_metadata(&w.target) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&w.target).is_ok(),
                Ok(_) => std::fs::remove_file(&w.target).is_ok(),
                Err(_) => true,
            };
        if !removed && kept.is_none() {
            kept = Some(w.target);
        }
    }
    kept
}

/// Tallies per-entry states and builds a member row, with the copy's
/// SHA-256, for every file this run copied or adopted.
fn collect(
    source_root: &Path,
    walk: &ScopeWalk,
    states: &[State],
    dest: &Path,
    context: &RowContext<'_>,
    problems: &mut Problems,
) -> Tally {
    let now = super::now_ms();
    let mut members = Vec::new();
    let (mut copied, mut adopted, mut already, mut failed, mut occupied) = (0, 0, 0, 0, 0);
    for (entry, state) in walk.entries.iter().zip(states) {
        if entry.kind == EntryKind::Dir {
            continue;
        }
        match state {
            State::Copied | State::Adopted => {
                let target = dest.join(&entry.relative);
                let digest = match entry.kind {
                    EntryKind::Symlink => std::fs::read_link(&target)
                        .map(|link| sha256_hex(&Sha256::digest(link.as_os_str().as_bytes()).into())),
                    _ => sha256_file(&target).map(|digest| sha256_hex(&digest)),
                };
                match digest {
                    Ok(sha256) => {
                        if *state == State::Copied {
                            copied += 1;
                        } else {
                            adopted += 1;
                        }
                        members.push(ReceiptRow {
                            run_id: context.run_id.to_owned(),
                            source_label: context.source_label.to_owned(),
                            source_path: source_root.join(&entry.relative).to_string_lossy().into_owned(),
                            source_dev: entry.dev,
                            source_ino: entry.ino,
                            size: entry.size,
                            mtime_ns: clamp_i64(entry.mtime_ns),
                            dest_path: target.to_string_lossy().into_owned(),
                            method: "copy".to_owned(),
                            sha256,
                            verified_at_ms: now,
                        });
                    }
                    Err(error) => {
                        failed += 1;
                        // An adopted file was there before this run: it is
                        // not a partial this run left.
                        let at = (*state == State::Copied).then_some(target.clone());
                        problems.note(
                            format!("verified, but hashing {} failed: {error}", target.display()),
                            at,
                        );
                    }
                }
            }
            State::Already => already += 1,
            State::Failed => failed += 1,
            State::Occupied => occupied += 1,
        }
    }

    Tally {
        copied,
        adopted,
        already,
        failed,
        occupied,
        members,
    }
}

#[cfg(test)]
type AfterCopyHook = Box<dyn FnMut(&Path, &Path)>;

#[cfg(test)]
thread_local! {
    /// Called with `(item relative path, mirror directory)` after a roll-up
    /// is copied and before the source is walked again: lets a test change
    /// the source, or the destination, mid-copy.
    pub(crate) static AFTER_COPY: std::cell::RefCell<Option<AfterCopyHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn after_copy_hook(relative: &Path, dest: &Path) {
    AFTER_COPY.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(relative, dest);
        }
    });
}

/// What [`collect`] counted.
struct Tally {
    copied: u64,
    adopted: u64,
    already: u64,
    failed: u64,
    occupied: u64,
    members: Vec<ReceiptRow>,
}

/// SHA-256 over the scope's manifest: each entry's relative path, kind,
/// size, and nanosecond mtime, in path order. Names what the receipt is a
/// receipt for; the content proofs are the member rows.
fn manifest_digest(walk: &ScopeWalk) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rdirstat-rollup-manifest\0");
    for entry in &walk.entries {
        let path = entry.relative.as_os_str().as_bytes();
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        hasher.update([entry.kind as u8]);
        hasher.update(entry.size.to_le_bytes());
        hasher.update(entry.mtime_ns.to_le_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    sha256_hex(&digest)
}
