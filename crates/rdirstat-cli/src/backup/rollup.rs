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
//! So the item count is bounded by the number of directories on the paths to
//! skipped or individually planned items, plus one per [`MAX_TREE_FILES`]
//! files: tens of thousands on a volume of tens of millions of inodes.
//!
//! # Already landed
//!
//! The drive's own catalog (`<dest>/.rdirstat/catalog`: file receipts and
//! roll-up members) is read at plan time. A file it already holds, by source
//! path, size and whole-second mtime, is not planned on its own, and a
//! roll-up whose every file it holds is not planned at all. Apply decides
//! again for itself, by inode and nanosecond mtime ([`Landed`]); the plan's
//! reading is only an estimate.
//!
//! # Landing (apply)
//!
//! Before copying, the source scope is walked and must match the plan: the
//! directory's `(dev, ino, mtime_ns)`, the file count, the logical bytes and
//! the newest mtime. Anything else is `changed_since_scan` for the item.
//!
//! - A tree roll-up whose mirror directory does not exist is one `ditto` of
//!   the directory, then `compare_tree`.
//! - Otherwise (a files roll-up, or a mirror directory that already exists,
//!   partly from an earlier wave or a failed run) the scope is *merged*:
//!   nothing at the destination is ever overwritten. A missing file, symlink
//!   or whole subdirectory is landed with `ditto` and compared. A file that
//!   is there and whose receipt or member row matches the source identity is
//!   already landed. A file that is there without one is compared with the
//!   source: equal, it is adopted; different, it is left alone and reported
//!   (`destination_occupied`).
//!
//! The walk is repeated after landing; a difference is
//! `changed_during_copy` and nothing is recorded. Otherwise every file that
//! landed or was adopted gets a `members` row (with its SHA-256), and, when
//! all of the scope landed, the item gets one receipt with method `rollup`.
//! Roll-up files are never clone sources and never cloned: deduplication
//! stays with the files planned one by one.

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use rdirstat_catalog::{METHOD_ROLLUP, ReceiptRow, read_receipts, visit_members};
use rdirstat_copy::{compare_tree, land, partial_path, sha256_file, sha256_hex};
use rdirstat_core::{CompletedScan, Kind, NodeId, flags};
use rdirstat_organize::{ItemKind, PlanItem, Rollup, RollupScope, Rules, SkippedItem};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::plan::RULE_CHANGED_SINCE_SCAN;

/// Most files a tree roll-up holds. A larger directory is split: its loose
/// files become a files roll-up and its subdirectories are judged on their
/// own. Bounds one item's memory at apply and what one failed copy costs.
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

/// What the drive's own catalog says has landed, as plan-time keys:
/// `(source path, size, whole-second mtime)`.
#[derive(Debug, Default)]
pub(crate) struct LandedKeys {
    keys: HashSet<u128>,
}

impl LandedKeys {
    /// Reads file receipts and roll-up members for `label` from `catalog`
    /// (the destination's mirror catalog). A missing catalog is empty.
    ///
    /// # Errors
    ///
    /// An unreadable part.
    pub(crate) fn load(catalog: &Path, label: &str) -> anyhow::Result<Self> {
        let mut keys = HashSet::new();
        for row in read_receipts(catalog)? {
            if row.source_label == label && row.method != METHOD_ROLLUP {
                keys.insert(plan_key(
                    row.source_path.as_bytes(),
                    row.size,
                    row.mtime_ns.div_euclid(1_000_000_000),
                ));
            }
        }
        visit_members(catalog, |row| {
            if row.source_label == label {
                keys.insert(plan_key(
                    row.source_path.as_bytes(),
                    row.size,
                    row.mtime_ns.div_euclid(1_000_000_000),
                ));
            }
        })?;
        Ok(Self { keys })
    }

    pub(crate) fn contains(&self, absolute: &Path, size: u64, mtime_s: i64) -> bool {
        !self.keys.is_empty()
            && self
                .keys
                .contains(&plan_key(absolute.as_os_str().as_bytes(), size, mtime_s))
    }

    pub(crate) fn len(&self) -> usize {
        self.keys.len()
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
        .contains(&input.source.join(relative), node.size, node.mtime);
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

impl Landed {
    /// Loads the members and file receipts of `catalog` (the drive's mirror;
    /// `None` loads nothing, so nothing is trusted).
    ///
    /// # Errors
    ///
    /// An unreadable part.
    pub(crate) fn load(mirror: Option<&Path>, receipts: &[ReceiptRow]) -> anyhow::Result<Self> {
        let mut landed = Self::default();
        let Some(mirror) = mirror else {
            return Ok(landed);
        };
        for row in receipts.iter().filter(|r| r.method != METHOD_ROLLUP) {
            landed.insert(
                row.dest_path.as_bytes(),
                row.source_path.as_bytes(),
                row.source_ino,
                row.size,
                row.mtime_ns,
            );
        }
        visit_members(mirror, |row| {
            landed.insert(
                row.dest_path.as_bytes(),
                row.source_path.as_bytes(),
                row.source_ino,
                row.size,
                row.mtime_ns,
            );
        })?;
        Ok(landed)
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
    ChangedDuringCopy(String),
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

    let fast = rollup.scope == RollupScope::Tree && std::fs::symlink_metadata(dest).is_err();
    if fast {
        match land(&item.absolute, dest, None, cancel) {
            Ok(_) => states.fill(State::Copied),
            Err(error) => {
                let at = partial_path(&error).cloned();
                problems.note(error.to_string(), at);
            }
        }
    } else {
        merge(&item.absolute, &walk, dest, landed, cancel, &mut states, &mut problems);
    }

    // Did the source move under the copy? Then no proof is about the plan.
    match walk_scope(&item.absolute, rollup) {
        Ok(after) => {
            if let Err(reason) = matches_plan(&after, item, rollup) {
                return RollupOutcome::ChangedDuringCopy(reason);
            }
        }
        Err(error) => return RollupOutcome::ChangedDuringCopy(format!("cannot read the source: {error}")),
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

/// The merge: lands what is missing at the destination and checks what is
/// there, never writing over anything. `states` is filled per entry.
fn merge(
    source_root: &Path,
    walk: &ScopeWalk,
    dest: &Path,
    landed: &Landed,
    cancel: &AtomicBool,
    states: &mut [State],
    problems: &mut Problems,
) {
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match land(&source, &target, None, cancel) {
                    Ok(_) => whole(states, State::Copied),
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
                        problems.note(
                            format!("verified, but hashing {} failed: {error}", target.display()),
                            Some(target),
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
