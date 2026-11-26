//! Largest-first fit: which items one Backup wave copies.
//!
//! Wave mode, "largest files first" (11-ORGANIZE.md, Amendment A):
//!
//! * Candidates are [`ItemKind::File`] and [`ItemKind::Package`] items whose
//!   allocated size is at least the floor. A package is one unit: nothing
//!   inside it is ever a candidate beside it. Directories, symlinks, and
//!   specials (sockets, FIFOs, devices) are never candidates.
//! * A candidate a `skip` rule applies to (directly or through an ancestor
//!   directory) is listed in [`FitPlan::skipped`] with the rule id.
//! * A candidate whose relative or absolute path is not valid UTF-8 is listed
//!   as skipped under [`RULE_NON_UTF8`]. The plan file is JSON, and JSON
//!   cannot carry such a path without an encoding the copier would then have
//!   to trust; refusing the row keeps the plan honest and the item visible.
//! * The rest are ranked by allocated bytes descending, then `tie` ascending,
//!   so the same scan yields the same plan. Past `cap` the ranking stops and
//!   the remainder is reported in `not_considered` / `not_considered_bytes`
//!   (allocated bytes).
//! * The budget is in logical bytes, because that is what the copy writes:
//!   the destination's projected pressure `(used + planned) / total` stays
//!   strictly below the reserve pressure. A candidate larger than what is
//!   left is deferred, never split, and the walk continues with smaller ones.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pair::DiskFacts;
use crate::rules::Rules;

/// Default and maximum reserve pressure: a plan never fills the destination
/// to 0.75 or beyond. Callers may lower it.
pub const DEFAULT_RESERVE_PRESSURE: f64 = 0.75;
/// Default review cap on ranked candidates.
pub const DEFAULT_CAP: usize = 50_000;
/// Default floor on allocated bytes, 100 MiB.
pub const DEFAULT_FLOOR_ALLOCATED: u64 = 100 * 1024 * 1024;
/// Rule id reported for candidates whose path is not valid UTF-8.
pub const RULE_NON_UTF8: &str = "builtin:non-utf8-path";

/// Stable warning codes.
pub mod codes {
    /// A planned file looks like a VM disk, container image, or database
    /// some process may be writing; the copy is a point-in-time image.
    pub const LIVE_CONTAINER_FILE: &str = "live_container_file";
    /// Two planned paths share one `(dev, ino)`: each lands as a full copy.
    pub const HARDLINKS_MAY_SPLIT: &str = "hardlinks_may_split";
    /// The destination filesystem is not APFS or HFS+, so extended
    /// attributes, resource forks, or flags may not survive.
    pub const METADATA_FILESYSTEM: &str = "metadata_filesystem";
    /// The requested reserve pressure was above 0.75 or not a number.
    pub const RESERVE_CLAMPED: &str = "reserve_clamped";
    /// The candidate list passed the cap; a second wave is needed.
    pub const CANDIDATES_TRUNCATED: &str = "candidates_truncated";
    /// The destination reported zero capacity, so nothing fits.
    pub const DEST_CAPACITY_UNKNOWN: &str = "dest_capacity_unknown";
    /// The destination is already at or past the reserve, so nothing fits.
    pub const DEST_OVER_RESERVE: &str = "dest_over_reserve";
    /// Candidates were refused because their path is not valid UTF-8.
    pub const NON_UTF8_PATH: &str = "non_utf8_path";
}

/// What kind of filesystem object an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// Regular file.
    File,
    /// A macOS package (`.app`, `.photoslibrary`, ...): copied as one unit.
    Package,
    /// Ordinary directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Socket, FIFO, or device node.
    Special,
}

/// One retained node of a completed scan, as the shell hands it to the fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanItem {
    /// Path relative to the source volume root.
    pub relative: PathBuf,
    /// Absolute path on the source.
    pub absolute: PathBuf,
    /// Object kind.
    pub kind: ItemKind,
    /// Logical (apparent) bytes; for a package, its subtree total.
    pub logical: u64,
    /// Allocated bytes; for a package, its subtree total.
    pub allocated: u64,
    /// `st_dev` at scan time.
    pub dev: u64,
    /// `st_ino` at scan time.
    pub ino: u64,
    /// Modification time at scan time, nanoseconds since the epoch.
    pub mtime_ns: i128,
    /// Stable tie-break (the scan's `NodeId`).
    pub tie: u64,
}

/// Everything the fit needs. Pure data; nothing here touches a disk.
#[derive(Debug, Clone, Copy)]
pub struct FitInput<'a> {
    /// The scan's items, in any order.
    pub items: &'a [ScanItem],
    /// Skip/include rules.
    pub rules: &'a Rules,
    /// Minimum allocated bytes for a candidate.
    pub floor_allocated: u64,
    /// Destination facts (capacity and filesystem).
    pub dest: &'a DiskFacts,
    /// Projected destination pressure must stay strictly below this.
    /// Clamped to at most [`DEFAULT_RESERVE_PRESSURE`].
    pub reserve_pressure: f64,
    /// Review cap on ranked candidates.
    pub cap: usize,
}

/// One item the wave copies (or defers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// Path relative to the source volume root. Always valid UTF-8.
    pub relative: PathBuf,
    /// Absolute source path. Always valid UTF-8.
    pub absolute: PathBuf,
    /// [`ItemKind::File`] or [`ItemKind::Package`].
    pub kind: ItemKind,
    /// Logical bytes, the amount the copy writes.
    pub logical: u64,
    /// Allocated bytes on the source.
    pub allocated: u64,
    /// `st_dev` at scan time; apply re-stats against it.
    pub dev: u64,
    /// `st_ino` at scan time.
    pub ino: u64,
    /// Modification time at scan time, nanoseconds.
    pub mtime_ns: i128,
}

impl PlanItem {
    fn from_scan(item: &ScanItem) -> PlanItem {
        PlanItem {
            relative: item.relative.clone(),
            absolute: item.absolute.clone(),
            kind: item.kind,
            logical: item.logical,
            allocated: item.allocated,
            dev: item.dev,
            ino: item.ino,
            mtime_ns: item.mtime_ns,
        }
    }
}

/// A candidate over the floor that a rule left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    /// Relative path (serialized lossily; it may not be UTF-8).
    #[serde(serialize_with = "lossy_path")]
    pub relative: PathBuf,
    /// The rule id, or [`RULE_NON_UTF8`].
    pub rule_id: String,
    /// The rule's reason.
    pub reason: String,
    /// Allocated bytes.
    pub allocated: u64,
}

/// A labelled caveat on the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Stable code from [`codes`].
    pub code: String,
    /// Human sentence.
    pub text: String,
}

impl Warning {
    fn new(code: &str, text: String) -> Warning {
        Warning {
            code: code.to_owned(),
            text,
        }
    }
}

/// The result of a fit.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct FitPlan {
    /// Items this wave copies, largest allocated first.
    pub items: Vec<PlanItem>,
    /// Skipped candidates, largest first, at most `cap` rows.
    pub skipped: Vec<SkippedItem>,
    /// Ranked candidates that did not fit in the remaining budget.
    pub deferred: Vec<PlanItem>,
    /// Candidates beyond the cap, not ranked this wave.
    pub not_considered: u64,
    /// Allocated bytes of `not_considered`.
    pub not_considered_bytes: u64,
    /// Logical bytes of `items`.
    pub total_logical: u64,
    /// Allocated bytes of `items`.
    pub total_allocated: u64,
    /// Caveats with stable codes.
    pub warnings: Vec<Warning>,
    /// Every skipped candidate, including rows past the `skipped` cap.
    pub skipped_count: u64,
    /// Allocated bytes of every skipped candidate.
    pub skipped_bytes: u64,
    /// Logical bytes the destination may still take under the reserve, as
    /// computed before this wave.
    pub budget_logical: u64,
}

/// Rank, skip, and budget the scan's items. See the module docs.
#[must_use]
pub fn fit(input: &FitInput<'_>) -> FitPlan {
    let mut plan = FitPlan::default();

    let packages: HashSet<&Path> = input
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Package)
        .map(|i| i.relative.as_path())
        .collect();
    let inside_package = |path: &Path| path.ancestors().skip(1).any(|a| packages.contains(a));

    let mut candidates: Vec<&ScanItem> = Vec::new();
    let mut skipped: Vec<(SkippedItem, u64)> = Vec::new();
    let mut non_utf8 = 0_u64;
    for item in input.items {
        if !matches!(item.kind, ItemKind::File | ItemKind::Package)
            || item.allocated < input.floor_allocated
            || inside_package(&item.relative)
        {
            continue;
        }
        if item.relative.to_str().is_none() || item.absolute.to_str().is_none() {
            non_utf8 += 1;
            skipped.push((
                SkippedItem {
                    relative: item.relative.clone(),
                    rule_id: RULE_NON_UTF8.to_owned(),
                    reason: "path is not valid UTF-8 and cannot be carried in a plan file".to_owned(),
                    allocated: item.allocated,
                },
                item.tie,
            ));
            continue;
        }
        if let Some(rule) = input.rules.skipped_by(&item.relative) {
            skipped.push((
                SkippedItem {
                    relative: item.relative.clone(),
                    rule_id: rule.id.clone(),
                    reason: rule.reason.clone(),
                    allocated: item.allocated,
                },
                item.tie,
            ));
            continue;
        }
        candidates.push(item);
    }

    skipped.sort_by(|(a, ta), (b, tb)| b.allocated.cmp(&a.allocated).then(ta.cmp(tb)));
    plan.skipped_count = skipped.len() as u64;
    plan.skipped_bytes = skipped.iter().map(|(s, _)| s.allocated).sum();
    plan.skipped = skipped.into_iter().take(input.cap).map(|(s, _)| s).collect();
    if non_utf8 > 0 {
        plan.warnings.push(Warning::new(
            codes::NON_UTF8_PATH,
            format!("{non_utf8} item(s) over the floor have a non-UTF-8 path and were left out"),
        ));
    }

    candidates.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.tie.cmp(&b.tie)));
    if candidates.len() > input.cap {
        let rest = &candidates[input.cap..];
        plan.not_considered = rest.len() as u64;
        plan.not_considered_bytes = rest.iter().map(|i| i.allocated).sum();
        candidates.truncate(input.cap);
        plan.warnings.push(Warning::new(
            codes::CANDIDATES_TRUNCATED,
            format!(
                "{} more candidate(s) were not considered; copy this wave, then plan another",
                plan.not_considered
            ),
        ));
    }

    let budget = destination_budget(&mut plan, input);
    let mut remaining = budget.unwrap_or(0);
    plan.budget_logical = remaining;

    for item in candidates {
        let row = PlanItem::from_scan(item);
        if budget.is_some() && item.logical <= remaining {
            remaining -= item.logical;
            plan.total_logical += item.logical;
            plan.total_allocated += item.allocated;
            plan.items.push(row);
        } else {
            plan.deferred.push(row);
        }
    }

    add_item_warnings(&mut plan, input.dest);
    plan
}

/// The logical bytes this wave may plan, with the warnings that explain a
/// clamped reserve or an empty budget. `None` means nothing may be planned.
fn destination_budget(plan: &mut FitPlan, input: &FitInput<'_>) -> Option<u64> {
    let (reserve_ppm, clamped) = reserve_ppm(input.reserve_pressure);
    if clamped {
        plan.warnings.push(Warning::new(
            codes::RESERVE_CLAMPED,
            format!(
                "reserve pressure {} is not allowed; using {DEFAULT_RESERVE_PRESSURE}",
                input.reserve_pressure
            ),
        ));
    }
    if input.dest.total_bytes == 0 {
        plan.warnings.push(Warning::new(
            codes::DEST_CAPACITY_UNKNOWN,
            "destination reported zero capacity; nothing is planned".to_owned(),
        ));
    }
    let budget = budget(input.dest, reserve_ppm);
    if budget.is_none() && input.dest.total_bytes > 0 {
        plan.warnings.push(Warning::new(
            codes::DEST_OVER_RESERVE,
            "destination is already at or past the reserve; nothing is planned".to_owned(),
        ));
    }
    budget
}

/// Reserve pressure in parts per million, clamped to the default maximum.
/// The bool is true when the request was clamped.
fn reserve_ppm(pressure: f64) -> (u64, bool) {
    const MAX_PPM: u64 = 750_000;
    if !pressure.is_finite() || pressure > DEFAULT_RESERVE_PRESSURE {
        return (MAX_PPM, true);
    }
    if pressure <= 0.0 {
        return (0, false);
    }
    let ppm = (pressure * 1_000_000.0).floor();
    // `ppm` is finite and within 0..=750_000 by the checks above.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded to 0..=750_000 above"
    )]
    let ppm = ppm as u64;
    (ppm.min(MAX_PPM), false)
}

/// Largest logical byte count `b` with `(used + b) / total < ppm / 1e6`.
///
/// `None` when no `b` satisfies it, not even zero: the destination is
/// already at or past the reserve, so not even an empty file may be planned.
fn budget(dest: &DiskFacts, ppm: u64) -> Option<u64> {
    let limit = u128::from(ppm) * u128::from(dest.total_bytes);
    if limit == 0 {
        return None;
    }
    // Largest total T with T * 1e6 < limit.
    let max_total = (limit - 1) / 1_000_000;
    let max_total = u64::try_from(max_total).unwrap_or(u64::MAX);
    max_total.checked_sub(dest.used_bytes())
}

fn add_item_warnings(plan: &mut FitPlan, dest: &DiskFacts) {
    let fs = dest.filesystem.to_ascii_lowercase();
    if !(fs.contains("apfs") || fs.contains("hfs")) {
        plan.warnings.push(Warning::new(
            codes::METADATA_FILESYSTEM,
            format!(
                "destination filesystem {:?} may not keep extended attributes, resource forks, or flags",
                dest.filesystem
            ),
        ));
    }
    let mut links: HashMap<(u64, u64), u32> = HashMap::new();
    for item in &plan.items {
        *links.entry((item.dev, item.ino)).or_default() += 1;
    }
    let shared = links.values().filter(|&&n| n > 1).count();
    if shared > 0 {
        plan.warnings.push(Warning::new(
            codes::HARDLINKS_MAY_SPLIT,
            format!("{shared} hard-link group(s) appear more than once; each path lands as an independent copy"),
        ));
    }
    let live: Vec<Warning> = plan
        .items
        .iter()
        .filter(|item| is_live_container(&item.relative))
        .map(|item| {
            Warning::new(
                codes::LIVE_CONTAINER_FILE,
                format!(
                    "{} may be in use; the copy is a point-in-time image",
                    item.relative.display()
                ),
            )
        })
        .collect();
    plan.warnings.extend(live);
}

/// A label, not a lock: names that are usually VM disks, container images,
/// or databases a running process writes.
fn is_live_container(path: &Path) -> bool {
    const EXTENSIONS: &[&str] = &[
        "raw",
        "qcow2",
        "vmdk",
        "vdi",
        "vhd",
        "vhdx",
        "img",
        "sparseimage",
        "sparsebundle",
        "sqlite",
        "sqlite3",
        "db",
        "vscdb",
        "hdd",
        "utm",
    ];
    const NAMES: &[&str] = &["datadisk", "diffdisk", "basedisk", "Docker.raw"];
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if NAMES.contains(&name) {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn lossy_path<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn item(rel: &str, kind: ItemKind, logical: u64, allocated: u64, tie: u64) -> ScanItem {
        ScanItem {
            relative: PathBuf::from(rel),
            absolute: Path::new("/System/Volumes/Data").join(rel),
            kind,
            logical,
            allocated,
            dev: 1,
            ino: tie + 1000,
            mtime_ns: 1_700_000_000_000_000_000,
            tie,
        }
    }

    fn file(rel: &str, size: u64, tie: u64) -> ScanItem {
        item(rel, ItemKind::File, size, size, tie)
    }

    fn dest(total: u64, available: u64) -> DiskFacts {
        DiskFacts {
            disk_id: Some("disk14".into()),
            st_dev: 9,
            filesystem: "apfs".into(),
            total_bytes: total,
            available_bytes: available,
        }
    }

    fn run(items: &[ScanItem], d: &DiskFacts, floor: u64, cap: usize) -> FitPlan {
        let rules = Rules::default_backup();
        fit(&FitInput {
            items,
            rules: &rules,
            floor_allocated: floor,
            dest: d,
            reserve_pressure: DEFAULT_RESERVE_PRESSURE,
            cap,
        })
    }

    fn rels(items: &[PlanItem]) -> Vec<&str> {
        items.iter().map(|i| i.relative.to_str().unwrap_or("?")).collect()
    }

    #[test]
    fn screenshot_big_files_skip_and_movie_is_planned() {
        let items = [
            file("Users/josh/.colima/_lima/_disks/colima/datadisk", 300 * GIB, 1),
            file(
                "Users/josh/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                40 * GIB,
                2,
            ),
            file(
                "System/Library/AssetsV2/com_apple_MobileAsset_iOSSimulatorRuntime/x.asset/AssetData/a.dmg",
                20 * GIB,
                3,
            ),
            file("Users/josh/.android/avd/x.avd/userdata-qemu.img.qcow2", 10 * GIB, 4),
            file("Users/josh/Movies/a.mov", 5 * GIB, 5),
        ];
        let plan = run(&items, &dest(22_000 * GIB, 22_000 * GIB), 100 * MIB, DEFAULT_CAP);
        assert_eq!(rels(&plan.items), ["Users/josh/Movies/a.mov"]);
        let ids: Vec<&str> = plan.skipped.iter().map(|s| s.rule_id.as_str()).collect();
        assert_eq!(
            ids,
            ["colima-lima-vm", "editor-state-db", "mobile-asset", "qcow2-image"]
        );
        assert_eq!(plan.skipped_count, 4);
        assert_eq!(plan.skipped_bytes, 370 * GIB);
        assert_eq!(plan.total_logical, 5 * GIB);
        assert!(plan.deferred.is_empty());
    }

    #[test]
    fn package_is_one_unit_and_its_children_are_not_candidates() {
        let items = [
            item(
                "Users/a/Pictures/P.photoslibrary",
                ItemKind::Package,
                50 * GIB,
                50 * GIB,
                1,
            ),
            file("Users/a/Pictures/P.photoslibrary/originals/big.heic", 20 * GIB, 2),
            item(
                "Users/a/Pictures/P.photoslibrary/inner.app",
                ItemKind::Package,
                2 * GIB,
                2 * GIB,
                3,
            ),
            item("Users/a/Pictures", ItemKind::Dir, 50 * GIB, 50 * GIB, 4),
            file("Users/a/Pictures/loose.jpg", GIB, 5),
        ];
        let plan = run(&items, &dest(1000 * GIB, 1000 * GIB), 100 * MIB, DEFAULT_CAP);
        assert_eq!(
            rels(&plan.items),
            ["Users/a/Pictures/P.photoslibrary", "Users/a/Pictures/loose.jpg"]
        );
        assert_eq!(plan.total_logical, 51 * GIB);
    }

    #[test]
    fn dirs_symlinks_specials_and_small_files_are_never_candidates() {
        let items = [
            item("Users/a/dir", ItemKind::Dir, 9 * GIB, 9 * GIB, 1),
            item("Users/a/link", ItemKind::Symlink, 9 * GIB, 9 * GIB, 2),
            item("Users/a/sock", ItemKind::Special, 9 * GIB, 9 * GIB, 3),
            file("Users/a/small", 99 * MIB, 4),
            file("Users/a/floor", 100 * MIB, 5),
        ];
        let plan = run(&items, &dest(1000 * GIB, 1000 * GIB), 100 * MIB, DEFAULT_CAP);
        assert_eq!(rels(&plan.items), ["Users/a/floor"]);
    }

    #[test]
    fn an_item_over_the_remaining_budget_is_deferred_not_split() {
        // total 100 GiB, empty: budget is just under 75 GiB.
        let items = [
            file("Users/a/one", 60 * GIB, 1),
            file("Users/a/two", 20 * GIB, 2),
            file("Users/a/three", 10 * GIB, 3),
        ];
        let plan = run(&items, &dest(100 * GIB, 100 * GIB), 100 * MIB, DEFAULT_CAP);
        assert_eq!(rels(&plan.items), ["Users/a/one", "Users/a/three"]);
        assert_eq!(rels(&plan.deferred), ["Users/a/two"]);
        assert_eq!(plan.total_logical, 70 * GIB);
    }

    #[test]
    fn reserve_keeps_projected_pressure_strictly_below_threshold() {
        let d = dest(1000, 1000);
        // budget: largest b with b/1000 < 0.75 -> 749.
        let exact = [file("Users/a/x", 750, 1)];
        let plan = run(&exact, &d, 0, DEFAULT_CAP);
        assert!(plan.items.is_empty());
        assert_eq!(plan.budget_logical, 749);
        let under = [file("Users/a/x", 749, 1)];
        let plan = run(&under, &d, 0, DEFAULT_CAP);
        assert_eq!(plan.items.len(), 1);

        // Used space counts against the budget.
        let used = dest(1000, 500);
        let plan = run(&[file("Users/a/x", 250, 1)], &used, 0, DEFAULT_CAP);
        assert!(plan.items.is_empty());
        let plan = run(&[file("Users/a/x", 249, 1)], &used, 0, DEFAULT_CAP);
        assert_eq!(plan.items.len(), 1);

        // Already over the reserve: nothing fits.
        let full = dest(1000, 100);
        assert_eq!(run(&[file("Users/a/x", 1, 1)], &full, 0, 10).items.len(), 0);
    }

    #[test]
    fn reserve_can_be_lowered_but_not_raised() {
        let rules = Rules::default_backup();
        let d = dest(1000, 1000);
        let items = [file("Users/a/x", 1, 1)];
        let at = |p: f64| {
            fit(&FitInput {
                items: &items,
                rules: &rules,
                floor_allocated: 0,
                dest: &d,
                reserve_pressure: p,
                cap: 10,
            })
        };
        assert_eq!(at(0.5).budget_logical, 499);
        let high = at(0.95);
        assert_eq!(high.budget_logical, 749);
        assert!(high.warnings.iter().any(|w| w.code == codes::RESERVE_CLAMPED));
        assert_eq!(at(f64::NAN).budget_logical, 749);
        assert_eq!(at(0.0).budget_logical, 0);
        assert_eq!(at(-1.0).budget_logical, 0);
    }

    #[test]
    fn huge_capacities_do_not_overflow() {
        let d = dest(u64::MAX, u64::MAX);
        let plan = run(&[file("Users/a/x", u64::MAX / 2, 1)], &d, 0, 10);
        assert_eq!(plan.items.len(), 1);
        assert!(plan.budget_logical < u64::MAX);
    }

    #[test]
    fn ranking_is_allocated_desc_then_tie() {
        let items = [
            file("Users/a/c", 5 * GIB, 30),
            file("Users/a/b", 5 * GIB, 10),
            file("Users/a/a", 9 * GIB, 20),
        ];
        let plan = run(&items, &dest(1000 * GIB, 1000 * GIB), 0, DEFAULT_CAP);
        assert_eq!(rels(&plan.items), ["Users/a/a", "Users/a/b", "Users/a/c"]);
    }

    #[test]
    fn over_cap_is_truncated_and_counted() {
        let items: Vec<ScanItem> = (0..10_u64)
            .map(|i| file(&format!("Users/a/f{i}"), (i + 1) * GIB, i))
            .collect();
        let plan = run(&items, &dest(1000 * GIB, 1000 * GIB), 0, 3);
        assert_eq!(rels(&plan.items), ["Users/a/f9", "Users/a/f8", "Users/a/f7"]);
        assert_eq!(plan.not_considered, 7);
        assert_eq!(plan.not_considered_bytes, (1..=7).sum::<u64>() * GIB);
        assert!(plan.warnings.iter().any(|w| w.code == codes::CANDIDATES_TRUNCATED));
    }

    #[test]
    fn skipped_list_is_capped_but_counted() {
        let items: Vec<ScanItem> = (0..5_u64)
            .map(|i| file(&format!("Users/a/node_modules/f{i}"), (i + 1) * GIB, i))
            .collect();
        let plan = run(&items, &dest(1000 * GIB, 1000 * GIB), 0, 2);
        assert_eq!(plan.skipped.len(), 2);
        assert_eq!(plan.skipped_count, 5);
        assert_eq!(plan.skipped_bytes, 15 * GIB);
    }

    #[test]
    fn non_utf8_paths_are_skipped_with_a_builtin_rule() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut bad = file("Users/a/x", GIB, 1);
        bad.relative = PathBuf::from(OsStr::from_bytes(b"Users/a/\xff.mov"));
        let plan = run(&[bad], &dest(1000 * GIB, 1000 * GIB), 0, 10);
        assert!(plan.items.is_empty());
        assert_eq!(plan.skipped[0].rule_id, RULE_NON_UTF8);
        assert!(plan.warnings.iter().any(|w| w.code == codes::NON_UTF8_PATH));
        // The skipped list still serializes.
        assert!(serde_json::to_string(&plan).is_ok());
    }

    #[test]
    fn warnings_carry_stable_codes() {
        let mut a = file("Users/a/vm/win.vhdx", GIB, 1);
        let mut b = file("Users/a/hard/link", GIB, 2);
        a.ino = 77;
        b.ino = 77;
        let mut d = dest(1000 * GIB, 1000 * GIB);
        d.filesystem = "exfat".into();
        let plan = run(&[a, b], &d, 0, 10);
        let codes_seen: HashSet<&str> = plan.warnings.iter().map(|w| w.code.as_str()).collect();
        for code in [
            codes::LIVE_CONTAINER_FILE,
            codes::HARDLINKS_MAY_SPLIT,
            codes::METADATA_FILESYSTEM,
        ] {
            assert!(codes_seen.contains(code), "{code}");
        }
        let apfs = run(&[file("Users/a/m.mov", GIB, 1)], &dest(1000 * GIB, 1000 * GIB), 0, 10);
        assert!(apfs.warnings.is_empty(), "{:?}", apfs.warnings);
    }

    #[test]
    fn zero_capacity_plans_nothing() {
        let plan = run(&[file("Users/a/x", 1, 1)], &dest(0, 0), 0, 10);
        assert!(plan.items.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert!(plan.warnings.iter().any(|w| w.code == codes::DEST_CAPACITY_UNKNOWN));
    }

    #[test]
    fn a_destination_past_the_reserve_plans_nothing_not_even_empty_files() {
        let plan = run(&[file("Users/a/empty", 0, 1)], &dest(1, 0), 0, 10);
        assert!(plan.items.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert!(plan.warnings.iter().any(|w| w.code == codes::DEST_OVER_RESERVE));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn plan_respects_budget_and_partitions_candidates(
                sizes in proptest::collection::vec(0_u64..1_000, 0..60),
                total in 1_u64..20_000,
                avail_frac in 0_u64..=100,
                cap in 0_usize..80,
            ) {
                let items: Vec<ScanItem> = sizes
                    .iter()
                    .enumerate()
                    .map(|(i, &s)| file(&format!("Users/a/f{i}"), s, i as u64))
                    .collect();
                let d = dest(total, total * avail_frac / 100);
                let plan = run(&items, &d, 0, cap);
                let used = u128::from(d.used_bytes());
                if !plan.items.is_empty() {
                    let after = used + u128::from(plan.total_logical);
                    prop_assert!(after * 1_000_000 < 750_000 * u128::from(total));
                }
                let considered = plan.items.len() + plan.deferred.len();
                prop_assert_eq!(considered as u64 + plan.not_considered, items.len() as u64);
                prop_assert!(considered <= cap);
                let again = run(&items, &d, 0, cap);
                prop_assert_eq!(again, plan);
            }
        }
    }
}
