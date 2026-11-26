//! `rdirstat backup plan`: scan (or load a snapshot), rank, skip, budget,
//! write the plan file, print the token.
//!
//! Planning writes the plan file and, in the primary catalog, the scan's
//! `entries` (and a manifest) under the plan's id (`plan-<label>-<ms>`), so
//! apply can carry them to the drive and `backup status` can say what over
//! the floor is not yet backed up. Neither is ever on the destination or
//! under the source (outside the app's own catalog directory; see the module
//! docs of `backup`). The source is walked or `lstat`ed, never opened for
//! writing.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rdirstat_core::{CompletedScan, Kind, NodeId, ScanOptions, flags, format_iec};
use rdirstat_organize::fit::codes;
use rdirstat_organize::{
    BACKUPS_DIR, DEFAULT_CAP, DEFAULT_RESERVE_PRESSURE, DiskFacts, FitInput, FitPlan, INTENT_BACKUP, ItemKind,
    PLAN_SCHEMA_VERSION, PlanFile, PlanItem, RULE_NON_UTF8, RollupScope, Rules, ScanItem, SkippedItem, Warning, fit,
    judge_backup_pair, plan_token, validate_label,
};
use serde_json::{Value, json};

use super::rollup::{self, Count, Coverage, LandedKeys, RollupInput};
use super::{DiskProbe, now_ms};

/// Rule id for a candidate whose `lstat` at plan time disagrees with the scan
/// it came from (or that vanished). It is listed as skipped, never planned.
pub(crate) const RULE_CHANGED_SINCE_SCAN: &str = "builtin:changed-since-scan";

/// Everything `backup plan` was asked.
#[derive(Debug)]
pub(crate) struct PlanRequest {
    /// Source volume root; `None` only with `snapshot`, meaning its scan root.
    pub(crate) source: Option<PathBuf>,
    pub(crate) label: String,
    pub(crate) dest: PathBuf,
    /// Minimum allocated bytes of a candidate.
    pub(crate) floor: u64,
    /// A `.rdstat` to plan from instead of walking.
    pub(crate) snapshot: Option<PathBuf>,
    pub(crate) rules: Rules,
    /// Plan file path; default under `catalog/plans/`.
    pub(crate) out: Option<PathBuf>,
    /// Primary catalog directory (the default plan location hangs off it).
    pub(crate) catalog: PathBuf,
    /// The app's own catalog directory: the one place under the source a
    /// plan or catalog may be written. `None` allows no such place.
    pub(crate) app_catalog: Option<PathBuf>,
    pub(crate) progress: bool,
    /// Cover what is not planned on its own with directory roll-ups
    /// (see [`super::rollup`]). Off, the plan is what it was before
    /// roll-ups: files under the floor or past the cap are only counted.
    pub(crate) rollup: bool,
}

/// How much of the source a plan covers and whether it is written. The
/// default is what `backup plan` without `--profile` does: the whole source,
/// the default review cap, plan file and scan entries written.
#[derive(Debug, Clone)]
pub(crate) struct PlanScope {
    /// Directories and single files under the source, relative to it. Empty:
    /// the whole source. Each directory is walked on its own; each file is
    /// `lstat`ed and offered to the fit as one candidate. Mirror paths stay
    /// relative to the source root.
    pub(crate) includes: Vec<PathBuf>,
    /// Review cap on ranked candidates.
    pub(crate) cap: usize,
    /// Build the plan but write nothing: no plan file, no scan entries.
    pub(crate) dry_run: bool,
}

impl Default for PlanScope {
    fn default() -> Self {
        Self {
            includes: Vec::new(),
            cap: DEFAULT_CAP,
            dry_run: false,
        }
    }
}

/// One walked tree of a plan: the scan and its root relative to the source.
struct Scope {
    scan: CompletedScan,
    base: PathBuf,
}

/// What the roll-up pass added to a plan.
#[derive(Debug, Default)]
pub(crate) struct RollupSummary {
    /// Roll-up items in the plan: count, files, logical bytes.
    pub(crate) planned_items: u64,
    pub(crate) planned: Count,
    /// Of those, whole-tree roll-ups.
    pub(crate) planned_trees: u64,
    /// Roll-ups that did not fit under the destination reserve.
    pub(crate) deferred_items: u64,
    pub(crate) deferred: Count,
    /// Where every file of the scan went.
    pub(crate) coverage: Coverage,
    /// Files the drive's catalog already holds that were not planned on
    /// their own.
    pub(crate) landed_candidates: Count,
    /// Receipt and member keys read from the drive's catalog.
    pub(crate) landed_keys: usize,
}

/// A written plan and the facts the report prints beside it.
#[derive(Debug)]
pub(crate) struct PlanOutcome {
    pub(crate) plan: PlanFile,
    pub(crate) token: String,
    pub(crate) plan_path: PathBuf,
    pub(crate) total_logical: u64,
    pub(crate) total_allocated: u64,
    /// Ranked candidates that did not fit under the destination reserve.
    pub(crate) deferred: Vec<PlanItem>,
    pub(crate) not_considered: u64,
    pub(crate) not_considered_bytes: u64,
    /// Allocated bytes of every skipped candidate (rules and changed).
    pub(crate) skipped_bytes: u64,
    pub(crate) budget_logical: u64,
    pub(crate) warnings: Vec<Warning>,
    pub(crate) dest: DiskFacts,
    /// The scan entries written under the plan's id, or why they were not.
    /// A failure here does not void the plan; status just cannot see the
    /// source's unplanned files.
    pub(crate) entries: std::result::Result<PathBuf, String>,
    /// `None` when roll-ups were off.
    pub(crate) rollups: Option<RollupSummary>,
}

/// Builds the plan, writes the plan file, and returns it with its token.
///
/// # Errors
///
/// A bad label, an unreadable source or snapshot, a refused source/destination
/// pair (same filesystem, same physical disk, or an unknown `disk_id`), or a
/// failure writing the plan file.
pub(crate) fn build_plan(request: &PlanRequest, probe: &dyn DiskProbe) -> Result<PlanOutcome> {
    build_plan_scoped(request, &PlanScope::default(), probe)
}

/// [`build_plan`] narrowed to `scope.includes`, with `scope.cap`, and, for a
/// dry run, writing nothing.
///
/// # Errors
///
/// As [`build_plan`], plus an include that does not exist, is neither a
/// directory nor a regular file, or is combined with `--snapshot`.
#[allow(
    clippy::too_many_lines,
    reason = "the plan's steps in order: judge the pair, scan, fit, roll up, write; each is already a helper"
)]
pub(crate) fn build_plan_scoped(
    request: &PlanRequest,
    scope: &PlanScope,
    probe: &dyn DiskProbe,
) -> Result<PlanOutcome> {
    validate_label(&request.label)?;
    let dest = canonical_dir(&request.dest)?;

    let snapshot = request.snapshot.as_deref().map(load_snapshot).transpose()?;
    let source = match (&request.source, &snapshot) {
        (Some(source), _) => canonical_dir(source)?,
        (None, Some((scan, _))) => canonical_dir(&scan.root_path)?,
        (None, None) => bail!("--source is required without --snapshot"),
    };

    // The pair is judged before the walk: a refused pair costs no scan.
    let source_facts = probe.facts(&source)?;
    let dest_facts = probe.facts(&dest)?;
    judge_backup_pair(&source_facts, &dest_facts)
        .with_context(|| format!("refusing {} -> {}", source.display(), dest.display()))?;
    let Some(dest_disk_id) = dest_facts.disk_id.clone() else {
        bail!("the destination's physical disk is unknown; refusing to guess");
    };
    let Some(dest_volume_uuid) = dest_facts.volume_uuid.clone() else {
        bail!("the destination's volume UUID is unknown; apply could not tell this drive from another");
    };
    let plan_path_for = |created_at_ms: i64| match &request.out {
        Some(path) => path.clone(),
        None => request
            .catalog
            .join("plans")
            .join(format!("{}-{created_at_ms}.json", request.label)),
    };
    let app_catalog = request.app_catalog.as_deref();
    super::refuse_write_under_source(&source, "the catalog", &request.catalog, app_catalog)?;
    super::refuse_write_under_source(&source, "the plan file", &plan_path_for(0), app_catalog)?;

    if snapshot.is_some() && !scope.includes.is_empty() {
        bail!("a snapshot covers its whole scan root; it cannot be narrowed to includes");
    }
    let mut scopes = Vec::new();
    let mut named_files = Vec::new();
    let mut identities = Vec::new();
    if let Some((scan, identity)) = snapshot {
        identities.push(identity);
        scopes.push(scope_of(scan, &source)?);
    } else if scope.includes.is_empty() {
        let (scan, identity) = walk(&source, request.progress)?;
        identities.push(identity);
        scopes.push(scope_of(scan, &source)?);
    } else {
        for include in &scope.includes {
            let absolute = source.join(include);
            let meta = std::fs::symlink_metadata(&absolute)
                .with_context(|| format!("include {} of {}", include.display(), source.display()))?;
            if meta.file_type().is_dir() {
                let (scan, identity) = walk(&absolute, request.progress)?;
                identities.push(identity);
                scopes.push(scope_of(scan, &source)?);
            } else if meta.file_type().is_file() {
                identities.push(format!("file:{}:{}:{}", meta.ino(), meta.size(), meta.mtime()));
                named_files.push((include.clone(), absolute, meta));
            } else {
                bail!(
                    "include {} is neither a directory nor a regular file",
                    absolute.display()
                );
            }
        }
    }
    let snapshot_identity = if identities.len() == 1 {
        identities.remove(0)
    } else {
        format!("set:[{}]", identities.join(";"))
    };

    // The drive's own catalog, read only, so files already on it are not
    // planned again. Only with roll-ups; without them a plan is unchanged.
    let landed = if request.rollup {
        LandedKeys::load(
            &rdirstat_catalog::dest_catalog_dir(&dest),
            &dest.join(BACKUPS_DIR).join(&request.label),
            &request.label,
        )
        .context("reading the destination's catalog")?
    } else {
        LandedKeys::default()
    };
    let mut items = Vec::new();
    let mut changed = Vec::new();
    let mut landed_candidates = Count::default();
    for (index, part) in scopes.iter().enumerate() {
        let (mut found, mut moved, already) =
            candidates(&part.scan, &source, &part.base, request.floor, &request.rules, &landed);
        let offset = (index as u64) << 40;
        for item in &mut found {
            item.tie = item.tie.saturating_add(offset);
        }
        items.append(&mut found);
        changed.append(&mut moved);
        landed_candidates.files += already.files;
        landed_candidates.bytes += already.bytes;
    }
    for (index, (relative, absolute, meta)) in named_files.into_iter().enumerate() {
        let item = ScanItem {
            relative,
            absolute,
            kind: ItemKind::File,
            logical: meta.size(),
            allocated: meta.blocks().saturating_mul(512),
            dev: meta.dev(),
            ino: meta.ino(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            tie: u64::MAX - index as u64,
        };
        if landed.contains(&item.absolute, &item.relative, meta.size(), meta.mtime()) {
            landed_candidates.files += 1;
            landed_candidates.bytes += meta.size();
            continue;
        }
        items.push(item);
    }
    let mut fitted = fit(&FitInput {
        items: &items,
        rules: &request.rules,
        floor_allocated: request.floor,
        dest: &dest_facts,
        reserve_pressure: DEFAULT_RESERVE_PRESSURE,
        cap: scope.cap,
    });

    let rollups = if request.rollup {
        let summary = add_rollups(
            &RollupContext {
                scopes: &scopes,
                source: &source,
                rules: &request.rules,
                landed: &landed,
            },
            &mut fitted,
            &mut changed,
        );
        Some(RollupSummary {
            landed_candidates,
            landed_keys: landed.len(),
            ..summary
        })
    } else {
        None
    };

    let changed_bytes: u64 = changed.iter().map(|item| item.allocated).sum();
    let changed_count = changed.len() as u64;
    let mut skipped = fitted.skipped;
    skipped.extend(changed);
    skipped.sort_by(|a, b| b.allocated.cmp(&a.allocated).then_with(|| a.relative.cmp(&b.relative)));

    let created_at_ms = now_ms();
    let plan = PlanFile {
        schema_version: PLAN_SCHEMA_VERSION,
        intent: INTENT_BACKUP.to_owned(),
        source_root: source,
        source_label: request.label.clone(),
        dest_root: dest,
        dest_disk_id,
        dest_volume_uuid,
        source_snapshot: snapshot_identity,
        rules_hash: request.rules.hash_hex(),
        rules_version: request.rules.version(),
        floor_allocated: request.floor,
        items: fitted.items,
        skipped_count: fitted.skipped_count.saturating_add(changed_count),
        skipped,
        created_at_ms,
    };
    let token = plan_token(&plan);
    let plan_path = plan_path_for(created_at_ms);
    let entries = if scope.dry_run {
        Err("dry run: nothing written".to_owned())
    } else {
        write_plan(&plan, &plan_path)?;
        write_scan_entries(&scopes, &plan, &request.catalog)
    };

    Ok(PlanOutcome {
        token,
        plan_path,
        total_logical: fitted.total_logical,
        total_allocated: fitted.total_allocated,
        deferred: fitted.deferred,
        not_considered: rollups
            .as_ref()
            .map_or(fitted.not_considered, |r| r.coverage.not_covered.files),
        not_considered_bytes: rollups
            .as_ref()
            .map_or(fitted.not_considered_bytes, |r| r.coverage.not_covered.bytes),
        skipped_bytes: fitted.skipped_bytes.saturating_add(changed_bytes),
        budget_logical: fitted.budget_logical,
        warnings: fitted.warnings,
        dest: dest_facts,
        entries,
        plan,
        rollups,
    })
}

/// The scan facts [`add_rollups`] reads.
struct RollupContext<'a> {
    scopes: &'a [Scope],
    source: &'a Path,
    rules: &'a Rules,
    landed: &'a LandedKeys,
}

/// A scan with its root made relative to `source`.
fn scope_of(scan: CompletedScan, source: &Path) -> Result<Scope> {
    let scan_root = std::fs::canonicalize(&scan.root_path).unwrap_or_else(|_| scan.root_path.clone());
    let Ok(base) = scan_root.strip_prefix(source).map(Path::to_path_buf) else {
        bail!(
            "the scan root {} is not inside the source {}",
            scan_root.display(),
            source.display()
        );
    };
    Ok(Scope { scan, base })
}

/// Adds `more` into `total`, field by field.
fn absorb(total: &mut Coverage, more: &Coverage) {
    for (into, from) in [
        (&mut total.individual, &more.individual),
        (&mut total.rolled_up, &more.rolled_up),
        (&mut total.landed, &more.landed),
        (&mut total.skipped, &more.skipped),
        (&mut total.not_covered, &more.not_covered),
    ] {
        into.files = into.files.saturating_add(from.files);
        into.bytes = into.bytes.saturating_add(from.bytes);
    }
    total.unreadable_dirs = total.unreadable_dirs.saturating_add(more.unreadable_dirs);
}

/// Adds roll-up items to a fitted plan: everything not planned or deferred
/// on its own, not skipped, and not already on the drive. Roll-ups take the
/// budget left after the individual items, largest first; one that does not
/// fit is deferred. Directories that changed since the scan join `changed`.
fn add_rollups(context: &RollupContext<'_>, fitted: &mut FitPlan, changed: &mut Vec<SkippedItem>) -> RollupSummary {
    let individual: HashSet<PathBuf> = fitted
        .items
        .iter()
        .chain(&fitted.deferred)
        .map(|item| item.relative.clone())
        .collect();
    let other_skips: HashSet<PathBuf> = changed
        .iter()
        .map(|item| item.relative.clone())
        .chain(
            fitted
                .skipped
                .iter()
                .filter(|item| item.rule_id == RULE_NON_UTF8)
                .map(|item| item.relative.clone()),
        )
        .collect();
    let mut found = rollup::RollupCandidates::default();
    for part in context.scopes {
        let mut more = rollup::candidates(&RollupInput {
            scan: &part.scan,
            source: context.source,
            base: &part.base,
            rules: context.rules,
            individual: &individual,
            other_skips: &other_skips,
            landed: context.landed,
        });
        found.items.append(&mut more.items);
        found.changed.append(&mut more.changed);
        absorb(&mut found.coverage, &more.coverage);
    }
    changed.append(&mut found.changed);

    let budget_known = !fitted
        .warnings
        .iter()
        .any(|w| w.code == codes::DEST_OVER_RESERVE || w.code == codes::DEST_CAPACITY_UNKNOWN);
    let mut remaining = fitted.budget_logical.saturating_sub(fitted.total_logical);
    found
        .items
        .sort_by(|a, b| b.logical.cmp(&a.logical).then_with(|| a.relative.cmp(&b.relative)));
    let mut summary = RollupSummary {
        coverage: found.coverage,
        ..RollupSummary::default()
    };
    for item in found.items {
        let files = item.rollup.as_ref().map_or(0, |r| r.files);
        if budget_known && item.logical <= remaining {
            remaining -= item.logical;
            fitted.total_logical += item.logical;
            fitted.total_allocated += item.allocated;
            summary.planned_items += 1;
            summary.planned.files += files;
            summary.planned.bytes += item.logical;
            if item.rollup.as_ref().is_some_and(|r| r.scope == RollupScope::Tree) {
                summary.planned_trees += 1;
            }
            fitted.items.push(item);
        } else {
            summary.deferred_items += 1;
            summary.deferred.files += files;
            summary.deferred.bytes += item.logical;
            fitted.deferred.push(item);
        }
    }
    // Past the cap is no longer "plan another wave": roll-ups cover it.
    fitted.warnings.retain(|w| w.code != codes::CANDIDATES_TRUNCATED);
    summary
}

/// Writes each scan's entries, then a manifest, to the primary catalog under
/// the plan's id. Returns the last entries part written.
fn write_scan_entries(scopes: &[Scope], plan: &PlanFile, catalog: &Path) -> std::result::Result<PathBuf, String> {
    let id = super::plan_id(plan);
    let primary = rdirstat_catalog::Catalog::new(catalog);
    let mut written = None;
    for part in scopes {
        written = Some(
            primary
                .write_entries(&part.scan, &plan.source_label, &id)
                .map_err(|error| format!("writing scan entries: {error}"))?,
        );
    }
    let Some(written) = written else {
        return Err("no directory was walked; only named files are planned".to_owned());
    };
    primary
        .write_manifest(&id, plan.created_at_ms)
        .map_err(|error| format!("writing the scan manifest: {error}"))?;
    Ok(written.primary)
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    let metadata = std::fs::metadata(&resolved).with_context(|| format!("cannot read {}", resolved.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", resolved.display());
    }
    Ok(resolved)
}

/// Reads a `.rdstat`; its identity is the SHA-256 of the file's bytes.
fn load_snapshot(path: &Path) -> Result<(CompletedScan, String)> {
    let digest = rdirstat_copy::sha256_file(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let scan = rdirstat_core::snapshot::read(&mut reader, rdirstat_core::snapshot::Limits::DESIGN_PROFILE)
        .with_context(|| format!("cannot decode snapshot {}", path.display()))?;
    Ok((scan, format!("rdstat:sha256:{}", rdirstat_copy::sha256_hex(&digest))))
}

/// Walks `source` with the scanner; the identity names the root and start.
#[allow(
    clippy::field_reassign_with_default,
    reason = "ScanOptions is #[non_exhaustive]; a struct literal is not permitted outside rdirstat-core"
)]
fn walk(source: &Path, progress: bool) -> Result<(CompletedScan, String)> {
    let mut options = ScanOptions::default();
    options.cross_filesystems = false;
    options.count_hard_links_once = true;
    options.apply_default_exclusions = true;
    options.aggregate_below_bytes = None;
    options.exclusions = crate::exclude::effective_rules(&[], true, source == Path::new("/"));
    let outcome = crate::engine::scan(&crate::engine::Config {
        root: source.to_path_buf(),
        options,
        progress,
    })
    .with_context(|| format!("scanning {}", source.display()))?;
    let identity = format!(
        "walk:{}:{}:{}",
        outcome.scan.started_unix_ms,
        outcome.scan.finished_unix_ms,
        source.display()
    );
    Ok((outcome.scan, identity))
}

/// The scan's files and packages as fit input.
///
/// Packages are units: the walk does not descend into them. Every candidate
/// at or over the floor that no skip rule covers is `lstat`ed now, so the plan
/// carries its real `(dev, ino, mtime_ns)` — a `.rdstat` records neither inodes
/// nor sub-second times. One whose size or whole-second mtime no longer
/// matches the scan, or whose kind changed, or that is gone, is returned as
/// skipped under [`RULE_CHANGED_SINCE_SCAN`] instead.
fn candidates(
    scan: &CompletedScan,
    source: &Path,
    base: &Path,
    floor: u64,
    rules: &Rules,
    landed: &LandedKeys,
) -> (Vec<ScanItem>, Vec<SkippedItem>, Count) {
    use std::os::unix::ffi::OsStrExt as _;

    let tree = &scan.tree;
    let mut items = Vec::new();
    let mut changed = Vec::new();
    let mut already = Count::default();
    let mut stack: Vec<(NodeId, PathBuf)> = vec![(scan.root, base.to_path_buf())];
    while let Some((directory, relative_dir)) = stack.pop() {
        for id in tree.children(directory) {
            let (Some(node), Some(name)) = (tree.node(id), tree.name_bytes(id)) else {
                continue;
            };
            let relative = relative_dir.join(std::ffi::OsStr::from_bytes(name));
            let package = node.kind() == Kind::Directory && node.flags & flags::PACKAGE != 0;
            if node.kind() == Kind::Directory && !package {
                stack.push((id, relative));
                continue;
            }
            let (kind, logical, allocated) = if package {
                (ItemKind::Package, tree.logical_of(id), tree.allocated_of(id))
            } else if node.kind() == Kind::File {
                (ItemKind::File, node.size, node.alloc)
            } else {
                continue;
            };
            let absolute = source.join(&relative);
            let mut item = ScanItem {
                relative,
                absolute,
                kind,
                logical,
                allocated,
                dev: scan.volume.device,
                ino: 0,
                mtime_ns: i128::from(node.mtime) * 1_000_000_000,
                tie: u64::from(id.raw()),
            };
            // Under the floor, or skipped by a rule: the fit reports it from the
            // scan's own numbers; there is nothing to re-stat.
            if allocated < floor || rules.skipped_by(&item.relative).is_some() {
                items.push(item);
                continue;
            }
            // Already on the drive (by path, size, inode, nanosecond mtime,
            // and a copy still at its mirror path): not ranked again.
            if kind == ItemKind::File && landed.contains(&item.absolute, &item.relative, node.size, node.mtime) {
                already.files += 1;
                already.bytes += node.size;
                continue;
            }
            match restat(&item, node.mtime) {
                Some((dev, ino, mtime_ns, logical)) => {
                    item.dev = dev;
                    item.ino = ino;
                    item.mtime_ns = mtime_ns;
                    item.logical = logical;
                    items.push(item);
                }
                None => changed.push(SkippedItem {
                    relative: item.relative,
                    rule_id: RULE_CHANGED_SINCE_SCAN.to_owned(),
                    reason: "changed or vanished between the scan and the plan".to_owned(),
                    allocated: item.allocated,
                }),
            }
        }
    }
    (items, changed, already)
}

/// `(dev, ino, mtime_ns, logical)` now, or `None` if the item no longer is
/// what the scan saw. A package's logical size is recomputed with
/// [`tree_logical`], the same function apply re-checks it with.
fn restat(item: &ScanItem, scan_mtime_s: i64) -> Option<(u64, u64, i128, u64)> {
    let meta = std::fs::symlink_metadata(&item.absolute).ok()?;
    if meta.mtime() != scan_mtime_s {
        return None;
    }
    let mtime_ns = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    match item.kind {
        ItemKind::File if meta.file_type().is_file() && meta.size() == item.logical => {
            Some((meta.dev(), meta.ino(), mtime_ns, meta.size()))
        }
        ItemKind::Package if meta.file_type().is_dir() => {
            let logical = tree_logical(&item.absolute).ok()?;
            Some((meta.dev(), meta.ino(), mtime_ns, logical))
        }
        _ => None,
    }
}

/// Logical bytes under a package: the `st_size` of every regular file and
/// symlink below `root`, each `(dev, ino)` counted once, without following
/// symlinks or crossing onto another filesystem.
///
/// # Errors
///
/// Any `lstat` or `readdir` failure: a package that cannot be read whole is
/// not one apply can vouch for.
pub(crate) fn tree_logical(root: &Path) -> std::io::Result<u64> {
    let device = std::fs::symlink_metadata(root)?.dev();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            if kind.is_dir() {
                if meta.dev() == device {
                    stack.push(path);
                }
            } else if kind.is_file() || kind.is_symlink() {
                if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                    continue;
                }
                total = total.saturating_add(meta.size());
            }
        }
    }
    Ok(total)
}

/// Writes the plan as pretty JSON, mode 0600 (it lists file names), through a
/// temp file and a rename so a reader never sees half a plan.
fn write_plan(plan: &PlanFile, path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let bytes = serde_json::to_vec_pretty(plan).context("encoding the plan")?;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("cannot create {}", temp.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(())
}

/// Skipped candidates grouped by rule: `(rule_id, reason, count, bytes)`,
/// largest first. Built from the listed rows (at most the fit's cap).
fn skipped_by_rule(plan: &PlanFile) -> Vec<(String, String, u64, u64)> {
    let mut groups: BTreeMap<&str, (&str, u64, u64)> = BTreeMap::new();
    for item in &plan.skipped {
        let entry = groups.entry(&item.rule_id).or_insert((&item.reason, 0, 0));
        entry.1 += 1;
        entry.2 = entry.2.saturating_add(item.allocated);
    }
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|(id, (reason, count, bytes))| (id.to_owned(), reason.to_owned(), count, bytes))
        .collect();
    rows.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    rows
}

fn entries_line(outcome: &PlanOutcome) -> String {
    match &outcome.entries {
        Ok(path) => format!("Scan:  {}", path.display()),
        Err(error) => format!("Scan entries not written ({error}); backup status cannot list unplanned files"),
    }
}

/// The "not ranked" line: with roll-ups, only files no roll-up can cover.
fn not_ranked_line(out: &mut String, outcome: &PlanOutcome) {
    if outcome.not_considered > 0 {
        if outcome.rollups.is_some() {
            let _ = writeln!(
                out,
                "  not ranked {} files ({}): no roll-up can cover them (a name that is not UTF-8)",
                outcome.not_considered,
                format_iec(outcome.not_considered_bytes)
            );
        } else {
            let _ = writeln!(
                out,
                "  not ranked {} items ({}) past the review cap",
                outcome.not_considered,
                format_iec(outcome.not_considered_bytes)
            );
        }
    }
}

/// The roll-up and coverage lines of the plan report.
fn rollup_lines(out: &mut String, rollups: &RollupSummary) {
    let _ = writeln!(
        out,
        "  roll-ups {} directories ({} whole trees), {} files, {} logical",
        rollups.planned_items,
        rollups.planned_trees,
        rollups.planned.files,
        format_iec(rollups.planned.bytes)
    );
    if rollups.deferred_items > 0 {
        let _ = writeln!(
            out,
            "  roll-ups deferred {} directories, {} files, {} logical",
            rollups.deferred_items,
            rollups.deferred.files,
            format_iec(rollups.deferred.bytes)
        );
    }
    let c = &rollups.coverage;
    let _ = writeln!(
        out,
        "  files    {} on their own, {} in roll-ups, {} already on the drive, {} skipped, {} not covered",
        c.individual.files, c.rolled_up.files, c.landed.files, c.skipped.files, c.not_covered.files
    );
    if rollups.landed_candidates.files > 0 {
        let _ = writeln!(
            out,
            "  already on the drive, not ranked again: {} files ({})",
            rollups.landed_candidates.files,
            format_iec(rollups.landed_candidates.bytes)
        );
    }
    if c.unreadable_dirs > 0 {
        let _ = writeln!(
            out,
            "  unreadable {} directories: their contents are unknown and not planned",
            c.unreadable_dirs
        );
    }
}

/// The plan report as text.
pub(crate) fn text(outcome: &PlanOutcome, top: usize) -> String {
    let plan = &outcome.plan;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Backup plan: {} -> {}/Backups/{} ({})",
        plan.source_root.display(),
        plan.dest_root.display(),
        plan.source_label,
        plan.dest_disk_id
    );
    let _ = writeln!(out, "  snapshot  {}", plan.source_snapshot);
    let _ = writeln!(
        out,
        "  rules     v{} sha256:{}",
        plan.rules_version,
        plan.rules_hash.get(..12).unwrap_or(&plan.rules_hash)
    );
    let _ = writeln!(out, "  floor     {} allocated", format_iec(plan.floor_allocated));
    let _ = writeln!(
        out,
        "  dest      {} free of {}; this wave may write {}",
        format_iec(outcome.dest.available_bytes),
        format_iec(outcome.dest.total_bytes),
        format_iec(outcome.budget_logical)
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Totals: {} items, {} logical, {} allocated",
        plan.items.len(),
        format_iec(outcome.total_logical),
        format_iec(outcome.total_allocated)
    );
    let deferred_bytes: u64 = outcome.deferred.iter().map(|item| item.logical).sum();
    let _ = writeln!(
        out,
        "  deferred {} items ({}) that do not fit this wave",
        outcome.deferred.len(),
        format_iec(deferred_bytes)
    );
    not_ranked_line(&mut out, outcome);
    let _ = writeln!(
        out,
        "  skipped {} items ({})",
        plan.skipped_count,
        format_iec(outcome.skipped_bytes)
    );
    if let Some(rollups) = &outcome.rollups {
        rollup_lines(&mut out, rollups);
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Largest items:");
    if plan.items.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for item in plan.items.iter().take(top) {
        let _ = writeln!(
            out,
            "  {:>11}  {:<7}  {}",
            format_iec(item.allocated),
            kind_key(item.kind),
            item.relative.display()
        );
    }
    if plan.items.len() > top {
        let _ = writeln!(out, "  ... {} more", plan.items.len() - top);
    }

    let groups = skipped_by_rule(plan);
    if !groups.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Skipped by rule:");
        for (id, reason, count, bytes) in groups {
            let _ = writeln!(out, "  {:>11}  {count:>6} items  {id}  ({reason})", format_iec(bytes));
        }
    }

    if !outcome.warnings.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Warnings:");
        for warning in &outcome.warnings {
            let _ = writeln!(out, "  [{}] {}", warning.code, warning.text);
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "{}", entries_line(outcome));
    let _ = writeln!(out, "Plan:  {}", outcome.plan_path.display());
    let _ = writeln!(out, "Token: {}", outcome.token);
    let _ = writeln!(
        out,
        "Apply: rdirstat backup apply --plan {} --token {}",
        outcome.plan_path.display(),
        outcome.token
    );
    out
}

/// The plan report as one JSON document.
pub(crate) fn json(outcome: &PlanOutcome, top: usize) -> Value {
    let plan = &outcome.plan;
    let deferred_bytes: u64 = outcome.deferred.iter().map(|item| item.logical).sum();
    let top_items: Vec<Value> = plan
        .items
        .iter()
        .take(top)
        .map(|item| {
            json!({
                "relative": item.relative.to_string_lossy(),
                "kind": kind_key(item.kind),
                "logical": item.logical,
                "allocated": item.allocated,
            })
        })
        .collect();
    let skipped: Vec<Value> = skipped_by_rule(plan)
        .into_iter()
        .map(
            |(id, reason, count, bytes)| json!({ "rule_id": id, "reason": reason, "items": count, "allocated": bytes }),
        )
        .collect();
    json!({
        "plan_path": outcome.plan_path.to_string_lossy(),
        "token": outcome.token,
        "intent": plan.intent,
        "source_root": plan.source_root.to_string_lossy(),
        "source_label": plan.source_label,
        "dest_root": plan.dest_root.to_string_lossy(),
        "dest_disk_id": plan.dest_disk_id,
        "dest_volume_uuid": plan.dest_volume_uuid,
        "source_snapshot": plan.source_snapshot,
        "rules_hash": plan.rules_hash,
        "rules_version": plan.rules_version,
        "floor_allocated": plan.floor_allocated,
        "totals": {
            "items": plan.items.len(),
            "logical": outcome.total_logical,
            "allocated": outcome.total_allocated,
            "deferred_items": outcome.deferred.len(),
            "deferred_logical": deferred_bytes,
            "not_considered_items": outcome.not_considered,
            "not_considered_allocated": outcome.not_considered_bytes,
            "skipped_items": plan.skipped_count,
            "skipped_allocated": outcome.skipped_bytes,
            "budget_logical": outcome.budget_logical,
        },
        "rollups": outcome.rollups.as_ref().map(|r| json!({
            "items": r.planned_items,
            "tree_items": r.planned_trees,
            "files": r.planned.files,
            "logical": r.planned.bytes,
            "deferred_items": r.deferred_items,
            "deferred_files": r.deferred.files,
            "deferred_logical": r.deferred.bytes,
            "coverage": r.coverage,
            "already_landed_candidates": r.landed_candidates,
            "drive_catalog_keys": r.landed_keys,
        })),
        "top_items": top_items,
        "skipped_by_rule": skipped,
        "warnings": outcome.warnings,
        "plan_id": super::plan_id(plan),
        "entries_part": outcome.entries.as_ref().ok().map(|p| p.to_string_lossy().into_owned()),
        "entries_error": outcome.entries.as_ref().err(),
    })
}

pub(crate) fn kind_key(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::File => "file",
        ItemKind::Package => "package",
        ItemKind::Dir => "dir",
        ItemKind::Symlink => "symlink",
        ItemKind::Special => "special",
    }
}
