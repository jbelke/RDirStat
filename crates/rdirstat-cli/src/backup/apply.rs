//! `rdirstat backup apply`: check the token, re-judge the disks, then land
//! each planned item through `rdirstat-copy` and receipt the ones that verify.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use rdirstat_catalog::{
    Catalog, CatalogError, HashRow, IndexSummary, METHOD_ROLLUP, PLAN_STATUS_PLANNED, PLAN_STATUS_SKIPPED, PlanRow,
    ReceiptRow, read_receipts, rebuild_index,
};
use rdirstat_copy::{Method, land, partial_path, sha256_file, sha256_hex};
use rdirstat_core::format_iec;
use rdirstat_organize::{
    BACKUPS_DIR, INTENT_BACKUP, ItemKind, PLAN_SCHEMA_VERSION, PlanFile, PlanItem, Rollup, RollupScope,
    judge_backup_pair, mirror_path, plan_token, validate_label,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::plan::tree_logical;
use super::rollup::{self, RollupOutcome};
use super::{DiskProbe, now_ms};

/// Receipts buffered before a part file is written.
const FLUSH_ROWS: usize = 256;
/// Roll-up member rows buffered before a part file is written.
const FLUSH_MEMBERS: usize = 65_536;
/// Longest a landed item waits for its receipt to reach disk.
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Everything `backup apply` was asked.
#[derive(Debug)]
pub(crate) struct ApplyRequest {
    pub(crate) plan: PathBuf,
    pub(crate) token: String,
    /// Primary catalog directory; receipts are mirrored to the destination.
    pub(crate) catalog: PathBuf,
    /// The app's own catalog directory: the one place under the source the
    /// catalog may be. `None` allows no such place.
    pub(crate) app_catalog: Option<PathBuf>,
    /// Print one line per item on stderr.
    pub(crate) progress: bool,
}

/// What happened to one planned item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// `ditto`, then a full compare passed. Receipted.
    Copied,
    /// `clonefile(2)` of a landed duplicate, then a contents compare passed.
    /// Receipted.
    Cloned,
    /// The destination drive's own catalog holds a receipt for this source
    /// identity at this path, and what is there still has the receipt's kind
    /// and size.
    AlreadyLanded,
    /// The source's `(dev, ino, size, mtime)` differs from the plan. Skipped.
    ChangedSinceScan,
    /// The source changed while it was being copied. The copy is left on disk
    /// and named; no receipt.
    ChangedDuringCopy,
    /// Something not receipted by a matching source is at the destination
    /// path, or a receipted copy that no longer has its receipt's kind or
    /// size. It is left alone.
    DestinationOccupied,
    /// The copy or verify failed. A partial, if any, is left and named; no
    /// receipt.
    Failed,
}

impl Outcome {
    const ALL: [Outcome; 7] = [
        Outcome::Copied,
        Outcome::Cloned,
        Outcome::AlreadyLanded,
        Outcome::ChangedSinceScan,
        Outcome::ChangedDuringCopy,
        Outcome::DestinationOccupied,
        Outcome::Failed,
    ];

    const fn key(self) -> &'static str {
        match self {
            Outcome::Copied => "copied",
            Outcome::Cloned => "cloned",
            Outcome::AlreadyLanded => "already_landed",
            Outcome::ChangedSinceScan => "changed_since_scan",
            Outcome::ChangedDuringCopy => "changed_during_copy",
            Outcome::DestinationOccupied => "destination_occupied",
            Outcome::Failed => "failed",
        }
    }

    const fn landed(self) -> bool {
        matches!(self, Outcome::Copied | Outcome::Cloned | Outcome::AlreadyLanded)
    }
}

/// One item's result.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ItemResult {
    pub(crate) relative: String,
    pub(crate) dest: String,
    pub(crate) outcome: Outcome,
    /// Logical bytes of the item.
    pub(crate) bytes: u64,
    /// Why it did not land, when it did not.
    pub(crate) detail: Option<String>,
    /// A partial copy left on disk.
    pub(crate) partial: Option<String>,
    /// For a directory roll-up: the files in its scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) files: Option<u64>,
}

/// The whole run.
#[derive(Debug)]
pub(crate) struct ApplyReport {
    pub(crate) run_id: String,
    pub(crate) plan_path: PathBuf,
    pub(crate) source_label: String,
    pub(crate) dest_root: PathBuf,
    pub(crate) results: Vec<ItemResult>,
    pub(crate) receipts_written: usize,
    pub(crate) catalog_primary: PathBuf,
    pub(crate) catalog_mirror: Option<PathBuf>,
    /// A catalog write that failed. Items that landed without a receipt on
    /// disk are not proven, so the run is incomplete.
    pub(crate) catalog_error: Option<String>,
    /// The drive's `index.parquet` rebuilt at the end of the run, or why it
    /// was not. The index is derived; a failure here loses no receipt, and
    /// `rdirstat backup index` rebuilds it.
    pub(crate) index: std::result::Result<IndexSummary, String>,
}

impl ApplyReport {
    /// Every item landed and every receipt reached the catalog.
    pub(crate) fn complete(&self) -> bool {
        self.catalog_error.is_none() && self.results.iter().all(|result| result.outcome.landed())
    }

    fn count(&self, outcome: Outcome) -> (usize, u64) {
        self.results
            .iter()
            .filter(|result| result.outcome == outcome)
            .fold((0, 0), |(n, bytes), result| (n + 1, bytes + result.bytes))
    }
}

/// Runs a Backup plan.
///
/// # Errors
///
/// Refusals that stop the whole plan before anything is written: an unreadable
/// or non-Backup plan file, a token mismatch, a refused disk pair, a
/// destination volume whose UUID is not the plan's, a catalog under the
/// source, or an unreadable catalog.
/// Per-item problems are not errors; they are [`Outcome`]s in the report.
pub(crate) fn apply(request: &ApplyRequest, probe: &dyn DiskProbe) -> Result<ApplyReport> {
    let bytes = std::fs::read(&request.plan).with_context(|| format!("cannot read {}", request.plan.display()))?;
    let plan: PlanFile =
        serde_json::from_slice(&bytes).with_context(|| format!("{} is not a plan file", request.plan.display()))?;
    if plan.intent != INTENT_BACKUP {
        bail!(
            "the plan's intent is {:?}; the CLI applies Backup plans only (Migrate, Organize and Dispose are GUI-only)",
            plan.intent
        );
    }
    if plan.schema_version != PLAN_SCHEMA_VERSION {
        bail!(
            "plan schema version {} is not {PLAN_SCHEMA_VERSION}; make a new plan",
            plan.schema_version
        );
    }
    let token = plan_token(&plan);
    if !request.token.trim().eq_ignore_ascii_case(&token) {
        bail!("the token does not match this plan file; nothing was copied. Make a new plan and review it");
    }
    validate_label(&plan.source_label)?;

    let dest_now = std::fs::canonicalize(&plan.dest_root)
        .with_context(|| format!("the destination {} is not mounted", plan.dest_root.display()))?;
    if dest_now != plan.dest_root {
        bail!(
            "the destination {} now resolves to {}; make a new plan",
            plan.dest_root.display(),
            dest_now.display()
        );
    }
    let source_facts = probe.facts(&plan.source_root)?;
    let dest_facts = probe.facts(&plan.dest_root)?;
    judge_backup_pair(&source_facts, &dest_facts).with_context(|| {
        format!(
            "refusing {} -> {}",
            plan.source_root.display(),
            plan.dest_root.display()
        )
    })?;
    // The BSD disk name is renumbered on every plug and boot, so the volume
    // UUID is what says this is the drive the plan was made for.
    if dest_facts.volume_uuid.as_deref() != Some(plan.dest_volume_uuid.as_str()) {
        bail!(
            "the volume at {} is {:?}, the plan was made for volume {:?}; make a new plan",
            plan.dest_root.display(),
            dest_facts.volume_uuid.as_deref().unwrap_or("unknown"),
            plan.dest_volume_uuid
        );
    }
    super::refuse_write_under_source(
        &plan.source_root,
        "the catalog",
        &request.catalog,
        request.app_catalog.as_deref(),
    )?;

    let catalog = Catalog::for_destination(&request.catalog, &plan.dest_root);
    let mut landed = Landed::load(&catalog)?;
    let run_id = format!("backup-{}-{}", plan.source_label, now_ms());
    let plan_id = super::plan_id(&plan);
    let mut writer = ReceiptWriter::new(&catalog, &run_id, &plan_id);
    writer.write_plan_rows(&plan_rows(&plan, &plan_id));
    let mirror_root = plan.dest_root.join(BACKUPS_DIR).join(&plan.source_label);
    let never = AtomicBool::new(false);

    let mut results = Vec::with_capacity(plan.items.len());
    let total = plan.items.len();
    for (index, item) in plan.items.iter().enumerate() {
        if writer.error.is_some() {
            break;
        }
        if request.progress {
            eprintln!("[{}/{total}] {}", index + 1, item.relative.display());
        }
        let result = land_item(&plan, &mirror_root, item, &mut landed, &mut writer, &never);
        results.push(result);
        writer.maybe_flush();
    }
    writer.flush();
    writer.finish();

    let (receipts_written, catalog_error) = (writer.receipts_written, writer.error);
    let index = rebuild_index(&plan.dest_root).map_err(|error| error.to_string());
    Ok(ApplyReport {
        run_id,
        plan_path: request.plan.clone(),
        source_label: plan.source_label.clone(),
        dest_root: plan.dest_root.clone(),
        results,
        receipts_written,
        catalog_primary: catalog.primary().to_path_buf(),
        catalog_mirror: catalog.mirror().map(Path::to_path_buf),
        catalog_error,
        index,
    })
}

/// The plan's items and listed skips as catalog rows.
fn plan_rows(plan: &PlanFile, plan_id: &str) -> Vec<PlanRow> {
    let row = |status: &str, relative: &Path| PlanRow {
        plan_id: plan_id.to_owned(),
        source_label: plan.source_label.clone(),
        status: status.to_owned(),
        relative: relative.to_string_lossy().into_owned(),
        source_path: plan.source_root.join(relative).to_string_lossy().into_owned(),
        kind: String::new(),
        logical: 0,
        allocated: 0,
        rule_id: String::new(),
        reason: String::new(),
        floor_allocated: plan.floor_allocated,
        created_at_ms: plan.created_at_ms,
    };
    let planned = plan.items.iter().map(|item| PlanRow {
        kind: super::plan::kind_key(item.kind).to_owned(),
        logical: item.logical,
        allocated: item.allocated,
        ..row(PLAN_STATUS_PLANNED, &item.relative)
    });
    let skipped = plan.skipped.iter().map(|item| PlanRow {
        allocated: item.allocated,
        rule_id: item.rule_id.clone(),
        reason: item.reason.clone(),
        ..row(PLAN_STATUS_SKIPPED, &item.relative)
    });
    planned.chain(skipped).collect()
}

/// What the destination drive's own catalog says has landed on it.
struct Landed {
    /// `(size, sha256)` → a landed destination file with that content.
    by_content: HashMap<(u64, String), PathBuf>,
    /// Sizes present in `by_content`: hash a source up front only when a
    /// clone is possible at all.
    sizes: HashSet<u64>,
    /// Destination path → the receipt that put it there.
    by_dest: HashMap<String, ReceiptRow>,
    /// Destination files landed by directory roll-ups (and by file
    /// receipts), keyed compactly: the merge of a roll-up looks every file
    /// up here before it reads one.
    files: rollup::Landed,
}

impl Landed {
    /// Reads receipts from the drive's mirror catalog only. The primary holds
    /// receipts for every drive ever used; another drive mounted at the same
    /// path must not inherit them. A receipt that reached the primary but not
    /// the mirror reads as absent, which refuses (`DestinationOccupied`)
    /// rather than claims.
    fn load(catalog: &Catalog) -> Result<Self> {
        let rows = match catalog.mirror() {
            Some(mirror) => {
                read_receipts(mirror).with_context(|| format!("reading receipts in {}", mirror.display()))?
            }
            None => Vec::new(),
        };
        let files = rollup::Landed::load(catalog.mirror(), &rows)
            .with_context(|| format!("reading roll-up members in {}", catalog.primary().display()))?;
        let mut landed = Landed {
            by_content: HashMap::new(),
            sizes: HashSet::new(),
            by_dest: HashMap::new(),
            files,
        };
        for row in rows {
            if row.method == METHOD_ROLLUP {
                landed.by_dest.insert(row.dest_path.clone(), row);
            } else {
                landed.add(row);
            }
        }
        Ok(landed)
    }

    fn add(&mut self, row: ReceiptRow) {
        self.sizes.insert(row.size);
        self.by_content
            .entry((row.size, row.sha256.clone()))
            .or_insert_with(|| PathBuf::from(&row.dest_path));
        self.by_dest.insert(row.dest_path.clone(), row);
    }

    /// A landed regular file with this size and hash that still exists with
    /// that size. `land` compares it against the source before cloning and
    /// copies instead if it has gone stale.
    fn clone_source(&self, size: u64, sha256: &str) -> Option<&Path> {
        let path = self.by_content.get(&(size, sha256.to_owned()))?;
        let meta = std::fs::symlink_metadata(path).ok()?;
        (meta.file_type().is_file() && meta.size() == size).then_some(path.as_path())
    }
}

/// Buffers receipts and their hash rows and writes them as part files.
struct ReceiptWriter<'a> {
    catalog: &'a Catalog,
    run_id: &'a str,
    /// The plan's id: its scan entries in the primary travel with this run.
    plan_id: &'a str,
    receipts: Vec<ReceiptRow>,
    hashes: Vec<HashRow>,
    /// Files landed by directory roll-ups, written before their receipts.
    members: Vec<ReceiptRow>,
    last_flush: Instant,
    receipts_written: usize,
    parts_written: usize,
    error: Option<String>,
}

impl<'a> ReceiptWriter<'a> {
    fn new(catalog: &'a Catalog, run_id: &'a str, plan_id: &'a str) -> Self {
        Self {
            catalog,
            run_id,
            plan_id,
            receipts: Vec::new(),
            hashes: Vec::new(),
            members: Vec::new(),
            last_flush: Instant::now(),
            receipts_written: 0,
            parts_written: 0,
            error: None,
        }
    }

    /// Records what the plan chose and skipped, before anything is copied,
    /// so the drive knows what was meant to land even if the run stops.
    fn write_plan_rows(&mut self, rows: &[PlanRow]) {
        match self.catalog.write_plan_rows(rows, self.run_id) {
            Ok(_) | Err(CatalogError::Mirror { .. }) => self.parts_written += 1,
            Err(error) => self.error = Some(format!("writing the plan rows: {error}")),
        }
    }

    fn maybe_flush(&mut self) {
        if self.receipts.len() >= FLUSH_ROWS
            || self.members.len() >= FLUSH_MEMBERS
            || self.last_flush.elapsed() >= FLUSH_INTERVAL
        {
            self.flush();
        }
    }

    /// Writes buffered hashes, then receipts. A mirror-only failure keeps the
    /// primary part and is retried by [`ReceiptWriter::finish`]; any other
    /// failure stops the run.
    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if self.error.is_some() {
            return;
        }
        if !self.hashes.is_empty() {
            let rows = std::mem::take(&mut self.hashes);
            match self.catalog.write_hashes(&rows, self.run_id) {
                Ok(_) | Err(CatalogError::Mirror { .. }) => self.parts_written += 1,
                Err(error) => {
                    self.error = Some(format!("writing hashes: {error}"));
                    return;
                }
            }
        }
        if !self.members.is_empty() {
            let rows = std::mem::take(&mut self.members);
            match self.catalog.write_members(&rows, self.run_id) {
                Ok(_) | Err(CatalogError::Mirror { .. }) => self.parts_written += 1,
                Err(error) => {
                    self.error = Some(format!("writing {} roll-up members: {error}", rows.len()));
                    return;
                }
            }
        }
        if !self.receipts.is_empty() {
            let rows = std::mem::take(&mut self.receipts);
            let count = rows.len();
            match self.catalog.write_receipts(&rows, self.run_id) {
                Ok(_) | Err(CatalogError::Mirror { .. }) => {
                    self.parts_written += 1;
                    self.receipts_written += count;
                }
                Err(error) => self.error = Some(format!("writing {count} receipts: {error}")),
            }
        }
    }

    /// Mirrors anything the destination lacks and writes the run's manifest.
    fn finish(&mut self) {
        if self.parts_written == 0 || self.error.is_some() {
            return;
        }
        if let Err(error) = self.catalog.sync_mirror(self.run_id) {
            self.error = Some(format!("mirroring the catalog to the destination: {error}"));
            return;
        }
        if let Err(error) = self.catalog.sync_mirror(self.plan_id) {
            self.error = Some(format!("mirroring the plan's scan entries to the destination: {error}"));
            return;
        }
        if let Err(error) = self.catalog.write_manifest(self.run_id, now_ms()) {
            self.error = Some(format!("writing the manifest: {error}"));
        }
    }
}

fn land_item(
    plan: &PlanFile,
    mirror_root: &Path,
    item: &PlanItem,
    landed: &mut Landed,
    writer: &mut ReceiptWriter<'_>,
    cancel: &AtomicBool,
) -> ItemResult {
    let mut result = ItemResult {
        relative: item.relative.to_string_lossy().into_owned(),
        dest: String::new(),
        outcome: Outcome::Failed,
        bytes: item.logical,
        detail: None,
        partial: None,
        files: item.rollup.as_ref().map(|r| r.files),
    };
    let dest = match destination_of(plan, mirror_root, item) {
        Ok(dest) => dest,
        Err(reason) => {
            result.detail = Some(reason);
            return result;
        }
    };
    result.dest = dest.to_string_lossy().into_owned();
    if let Some(rollup) = &item.rollup {
        land_rollup_item(plan, item, rollup, &dest, landed, writer, cancel, &mut result);
        return result;
    }

    if let Err(reason) = unchanged(item) {
        result.outcome = Outcome::ChangedSinceScan;
        result.detail = Some(reason);
        return result;
    }

    if std::fs::symlink_metadata(&dest).is_ok() {
        match landed.by_dest.get(&result.dest) {
            Some(receipt) if receipt_matches(receipt, plan, item) => match still_landed(&dest, item.kind, receipt) {
                Ok(()) => result.outcome = Outcome::AlreadyLanded,
                Err(reason) => {
                    result.outcome = Outcome::DestinationOccupied;
                    result.detail = Some(format!("receipted, but the copy at the destination {reason}"));
                }
            },
            _ => {
                result.outcome = Outcome::DestinationOccupied;
                result.detail = Some("something without a matching receipt is already at the destination path".into());
            }
        }
        return result;
    }

    // Hash up front only when the catalog holds a landed file of this size:
    // otherwise there is nothing to clone and the verified copy is hashed.
    let mut sha256 = None;
    if item.kind == ItemKind::File && landed.sizes.contains(&item.logical) {
        match sha256_file(&item.absolute) {
            Ok(digest) => sha256 = Some(sha256_hex(&digest)),
            Err(error) => {
                result.detail = Some(format!("cannot read the source: {error}"));
                return result;
            }
        }
    }
    let clone_from = sha256
        .as_deref()
        .and_then(|sha| landed.clone_source(item.logical, sha))
        .map(Path::to_path_buf);

    let outcome = match land(&item.absolute, &dest, clone_from.as_deref(), cancel) {
        Ok(done) => done,
        Err(error) => {
            result.partial = partial_path(&error).map(|path| path.to_string_lossy().into_owned());
            result.detail = Some(error.to_string());
            return result;
        }
    };

    // The compare proved destination == source as the source was then. If
    // the source moved under the copy, that proof is about a file the plan
    // never described: no receipt.
    if let Err(reason) = unchanged(item) {
        result.outcome = Outcome::ChangedDuringCopy;
        result.detail = Some(reason);
        result.partial = Some(result.dest.clone());
        return result;
    }

    let digest = match (item.kind, sha256) {
        (ItemKind::File, Some(sha)) => Ok(sha),
        (ItemKind::File, None) => sha256_file(&dest).map(|digest| sha256_hex(&digest)),
        _ => tree_digest(&dest),
    };
    let digest = match digest {
        Ok(digest) => digest,
        Err(error) => {
            result.detail = Some(format!("verified, but hashing the copy failed: {error}"));
            result.partial = Some(result.dest.clone());
            return result;
        }
    };

    record(plan, item, &result.dest, outcome.method, digest, landed, writer);
    result.outcome = match outcome.method {
        Method::Copy => Outcome::Copied,
        Method::Clone => Outcome::Cloned,
    };
    result
}

/// Lands a directory roll-up and queues its member rows and receipt.
#[allow(clippy::too_many_arguments, reason = "the same per-item state land_item threads")]
fn land_rollup_item(
    plan: &PlanFile,
    item: &PlanItem,
    rollup: &Rollup,
    dest: &Path,
    landed: &mut Landed,
    writer: &mut ReceiptWriter<'_>,
    cancel: &AtomicBool,
    result: &mut ItemResult,
) {
    let context = rollup::RowContext {
        run_id: writer.run_id,
        source_label: &plan.source_label,
    };
    let existing = landed.by_dest.get(&result.dest).cloned();
    match rollup::land_rollup(item, rollup, dest, &landed.files, &context, existing.as_ref(), cancel) {
        RollupOutcome::Landed {
            copied,
            adopted,
            already,
            members,
            receipt,
        } => {
            // Adopted files were already there; only a file this run wrote
            // makes the item "copied".
            result.outcome = if copied > 0 {
                Outcome::Copied
            } else {
                Outcome::AlreadyLanded
            };
            if adopted > 0 || already > 0 {
                result.detail = Some(format!(
                    "{copied} copied, {adopted} already there and verified, {already} already receipted"
                ));
            }
            for member in &members {
                landed.files.record(member);
            }
            writer.members.extend(members);
            if let Some(receipt) = receipt {
                landed.by_dest.insert(receipt.dest_path.clone(), receipt.clone());
                writer.receipts.push(receipt);
            }
        }
        RollupOutcome::ChangedSinceScan(reason) => {
            result.outcome = Outcome::ChangedSinceScan;
            result.detail = Some(reason);
        }
        RollupOutcome::ChangedDuringCopy(reason) => {
            result.outcome = Outcome::ChangedDuringCopy;
            result.detail = Some(reason);
            result.partial = Some(result.dest.clone());
        }
        RollupOutcome::Incomplete {
            failed,
            detail,
            partial,
            members,
            ..
        } => {
            result.outcome = if failed > 0 {
                Outcome::Failed
            } else {
                Outcome::DestinationOccupied
            };
            result.detail = Some(detail);
            result.partial = partial.map(|path| path.to_string_lossy().into_owned());
            for member in &members {
                landed.files.record(member);
            }
            writer.members.extend(members);
        }
    }
}

/// Queues the receipt (and, for a file, the hash row) of a verified item and
/// makes it a clone source for the rest of the run.
fn record(
    plan: &PlanFile,
    item: &PlanItem,
    dest: &str,
    method: Method,
    digest: String,
    landed: &mut Landed,
    writer: &mut ReceiptWriter<'_>,
) {
    let mtime_ns = clamp_i64(item.mtime_ns);
    let receipt = ReceiptRow {
        run_id: writer.run_id.to_owned(),
        source_label: plan.source_label.clone(),
        source_path: item.absolute.to_string_lossy().into_owned(),
        source_dev: item.dev,
        source_ino: item.ino,
        size: item.logical,
        mtime_ns,
        dest_path: dest.to_owned(),
        method: match method {
            Method::Copy => "copy",
            Method::Clone => "clone",
        }
        .to_owned(),
        sha256: digest.clone(),
        verified_at_ms: now_ms(),
    };
    if item.kind == ItemKind::File {
        writer.hashes.push(HashRow {
            path: receipt.source_path.clone(),
            dev: item.dev,
            ino: item.ino,
            size: item.logical,
            mtime_ns,
            sha256: digest,
        });
        landed.add(receipt.clone());
    } else {
        landed.by_dest.insert(receipt.dest_path.clone(), receipt.clone());
    }
    writer.receipts.push(receipt);
}

/// The mirror path for `item`, after checking the plan row is self-consistent
/// and that nothing on the way to it on the destination is a symlink that
/// could carry the write somewhere else.
fn destination_of(plan: &PlanFile, mirror_root: &Path, item: &PlanItem) -> Result<PathBuf, String> {
    match (item.kind, &item.rollup) {
        (ItemKind::File | ItemKind::Package, None) | (ItemKind::Dir, Some(_)) => {}
        (ItemKind::Dir, None) => return Err("a directory item without roll-up facts is never planned".into()),
        (_, Some(_)) => return Err("roll-up facts on an item that is not a directory".into()),
        (kind, None) => return Err(format!("a {kind:?} item is never planned")),
    }
    if item.relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("the relative path is not a plain relative path".into());
    }
    if item.absolute != plan.source_root.join(&item.relative) {
        return Err("the absolute path is not the source root plus the relative path".into());
    }
    // The volume root's own files: a files roll-up whose mirror directory
    // is the mirror root itself.
    let root_files =
        item.relative.as_os_str().is_empty() && item.rollup.as_ref().is_some_and(|r| r.scope == RollupScope::Files);
    let dest = if root_files {
        validate_label(&plan.source_label).map_err(|error| error.to_string())?;
        mirror_root.to_path_buf()
    } else {
        mirror_path(&plan.dest_root, &plan.source_label, &item.relative).map_err(|error| error.to_string())?
    };
    let mut walk = plan.dest_root.clone();
    let below = dest.strip_prefix(&plan.dest_root).map_err(|error| error.to_string())?;
    let mut components = below.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() && !root_files {
            break;
        }
        walk.push(component);
        match std::fs::symlink_metadata(&walk) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} on the destination is a symlink; refusing to write through it",
                    walk.display()
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("{} on the destination is not a directory", walk.display()));
            }
            _ => {}
        }
    }
    debug_assert!(dest.starts_with(mirror_root));
    Ok(dest)
}

/// `Ok` when the source is still what the plan describes:
/// `(dev, ino, size, mtime_ns)` for a file, and for a package the root's
/// `(dev, ino, mtime_ns)` plus its recomputed logical total.
fn unchanged(item: &PlanItem) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(&item.absolute).map_err(|error| format!("cannot stat the source: {error}"))?;
    let kind = meta.file_type();
    let mtime_ns = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    let mut changes = Vec::new();
    match item.kind {
        ItemKind::File if !kind.is_file() => return Err("is no longer a regular file".into()),
        ItemKind::Package if !kind.is_dir() => return Err("is no longer a directory".into()),
        _ => {}
    }
    if meta.dev() != item.dev {
        changes.push("dev");
    }
    if meta.ino() != item.ino {
        changes.push("ino");
    }
    if mtime_ns != item.mtime_ns {
        changes.push("mtime");
    }
    let size = match item.kind {
        ItemKind::Package => {
            tree_logical(&item.absolute).map_err(|error| format!("cannot read the package: {error}"))?
        }
        _ => meta.size(),
    };
    if size != item.logical {
        changes.push("size");
    }
    if changes.is_empty() {
        Ok(())
    } else {
        Err(format!("{} changed since the scan", changes.join(", ")))
    }
}

/// The receipt was written for this source item. `st_dev` is left out on
/// purpose: macOS assigns it from the BSD device node, which changes when a
/// drive is replugged or the machine reboots, so an unchanged file would stop
/// matching its own receipt. The label and absolute path scope the inode.
fn receipt_matches(receipt: &ReceiptRow, plan: &PlanFile, item: &PlanItem) -> bool {
    receipt.source_label == plan.source_label
        && receipt.source_path.as_bytes() == item.absolute.as_os_str().as_encoded_bytes()
        && receipt.source_ino == item.ino
        && receipt.size == item.logical
        && receipt.mtime_ns == clamp_i64(item.mtime_ns)
}

/// `Ok` when what is at `dest` still has the receipt's kind and logical size
/// (a regular file of `size` bytes, or a directory whose tree totals `size`).
/// It does not re-read contents.
fn still_landed(dest: &Path, kind: ItemKind, receipt: &ReceiptRow) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(dest).map_err(|error| format!("cannot be read: {error}"))?;
    let size = match kind {
        ItemKind::File if meta.file_type().is_file() => meta.size(),
        ItemKind::Package if meta.file_type().is_dir() => {
            tree_logical(dest).map_err(|error| format!("cannot be read: {error}"))?
        }
        ItemKind::File => return Err("is no longer a regular file".into()),
        _ => return Err("is no longer a directory".into()),
    };
    if size == receipt.size {
        Ok(())
    } else {
        Err(format!("is {size} bytes, the receipt says {}", receipt.size))
    }
}

fn clamp_i64(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

/// SHA-256 naming a directory tree's contents: every entry's relative path
/// bytes, kind, and content hash (or link target), sorted by path.
fn tree_digest(root: &Path) -> std::io::Result<String> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut entries: Vec<(Vec<u8>, u8, Vec<u8>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            let relative = path
                .strip_prefix(root)
                .map_err(std::io::Error::other)?
                .as_os_str()
                .as_bytes()
                .to_vec();
            let kind = std::fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() {
                entries.push((relative, b'd', Vec::new()));
                stack.push(path);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&path)?;
                entries.push((relative, b'l', target.as_os_str().as_bytes().to_vec()));
            } else if kind.is_file() {
                entries.push((relative, b'f', sha256_file(&path)?.to_vec()));
            } else {
                entries.push((relative, b'o', Vec::new()));
            }
        }
    }
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"rdirstat-tree-digest\0");
    for (relative, kind, content) in &entries {
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative);
        hasher.update([*kind]);
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(sha256_hex(&digest))
}

/// The apply report as text.
pub(crate) fn text(report: &ApplyReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Backup apply {}: {} -> {}/{BACKUPS_DIR}/{}",
        report.run_id,
        report.source_label,
        report.dest_root.display(),
        report.source_label
    );
    for outcome in Outcome::ALL {
        let (count, bytes) = report.count(outcome);
        if count > 0 {
            let _ = writeln!(out, "  {:<21} {count:>7}  {}", outcome.key(), format_iec(bytes));
        }
    }
    let rollups: Vec<&ItemResult> = report.results.iter().filter(|r| r.files.is_some()).collect();
    if !rollups.is_empty() {
        let landed: Vec<&&ItemResult> = rollups.iter().filter(|r| r.outcome.landed()).collect();
        let _ = writeln!(
            out,
            "  roll-ups              {:>7}  {} files landed in {} of them",
            rollups.len(),
            landed.iter().filter_map(|r| r.files).sum::<u64>(),
            landed.len()
        );
    }
    let _ = writeln!(
        out,
        "Receipts: {} written to {}{}",
        report.receipts_written,
        report.catalog_primary.display(),
        report
            .catalog_mirror
            .as_ref()
            .map(|mirror| format!(" and {}", mirror.display()))
            .unwrap_or_default()
    );
    if let Some(error) = &report.catalog_error {
        let _ = writeln!(out, "Catalog error: {error}");
    }
    match &report.index {
        Ok(index) => {
            let _ = writeln!(
                out,
                "Index: {} files, {} -> {}",
                index.rows,
                format_iec(index.bytes),
                index.index.display()
            );
        }
        Err(error) => {
            let _ = writeln!(
                out,
                "Index not rebuilt ({error}); run: rdirstat backup index --dest {}",
                report.dest_root.display()
            );
        }
    }
    let not_landed: Vec<&ItemResult> = report.results.iter().filter(|r| !r.outcome.landed()).collect();
    if !not_landed.is_empty() {
        let _ = writeln!(out, "Not landed:");
        for result in not_landed {
            let _ = writeln!(
                out,
                "  {:<21} {}  {}",
                result.outcome.key(),
                result.relative,
                result.detail.as_deref().unwrap_or("")
            );
            if let Some(partial) = &result.partial {
                let _ = writeln!(out, "  {:<21} left on disk at {partial}", "");
            }
        }
    }
    out
}

/// The apply report as one JSON document.
pub(crate) fn json(report: &ApplyReport) -> Value {
    let mut counts = serde_json::Map::new();
    for outcome in Outcome::ALL {
        let (count, bytes) = report.count(outcome);
        counts.insert(outcome.key().to_owned(), json!({ "items": count, "logical": bytes }));
    }
    json!({
        "run_id": report.run_id,
        "plan_path": report.plan_path.to_string_lossy(),
        "source_label": report.source_label,
        "dest_root": report.dest_root.to_string_lossy(),
        "complete": report.complete(),
        "counts": counts,
        "receipts_written": report.receipts_written,
        "catalog_primary": report.catalog_primary.to_string_lossy(),
        "catalog_mirror": report.catalog_mirror.as_ref().map(|m| m.to_string_lossy().into_owned()),
        "catalog_error": report.catalog_error,
        "index": match &report.index {
            Ok(index) => json!({ "path": index.index.to_string_lossy(), "sql": index.sql.to_string_lossy(), "files": index.rows, "logical": index.bytes }),
            Err(error) => json!({ "error": error }),
        },
        "items": report.results,
    })
}
