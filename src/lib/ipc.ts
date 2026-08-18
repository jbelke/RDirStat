/**
 * The one adapter between the generated bindings and the React tree.
 *
 * Rules this file exists to enforce:
 *
 * - **No component ever calls `invoke`.** docs/05-UI.md: "Hand-written
 *   `invoke<T>()` wrappers are how the IPC contract silently drifts." Every
 *   call below goes through `commands.*` from `src/lib/bindings.ts`.
 * - `Result` is unwrapped once, here, into a thrown `IpcError` carrying the
 *   typed variant — which is what TanStack Query wants, and what lets a caller
 *   branch on `stale_generation` instead of string-matching a message.
 * - Every `u64` is normalised through `num()` exactly once, at the boundary, so
 *   nothing downstream has to care whether the exporter emitted `number`,
 *   `string`, or `bigint`. `src-tauri` configures
 *   `dangerously_cast_bigints_to_number()`, so today every `u64` is a plain
 *   `number` — but the loose wire shapes below accept all three, which means a
 *   change to that setting is a compile error in this file and nowhere else.
 * - Outgoing ids go the other way through {@link toWireU64}, which likewise
 *   tracks the exporter's choice in exactly one place rather than at every
 *   call site.
 * - Snake_case wire fields become camelCase view models here and nowhere else.
 *
 * The view models are deliberately *not* re-exports of the generated types: a
 * component that binds directly to a generated struct re-breaks on every field
 * rename, and the whole point of the boundary is that the blast radius of a
 * contract change is this file.
 */

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import {
  commands,
  type CancelState,
  type ErrorClass,
  type Kind,
  type LayoutKind,
  type Operation,
  type RelocatePlan,
  type ScanError,
  type ScanOptions,
  type ScanState,
  type StorageReport,
  type Sort,
  type SortDirection,
  type SortKey,
  type TransferJob,
} from "@/lib/bindings";
import { IpcError, MAX_CHILD_PAGE, num, SCAN_PROGRESS_EVENT, unwrap, type NumLike } from "@/lib/wire";

export type {
  CancelState,
  ErrorClass,
  Kind,
  LayoutKind,
  Operation,
  ScanOptions,
  ScanState,
  Sort,
  SortDirection,
  SortKey,
};

// ---------------------------------------------------------------------------
// Loose wire shapes.
//
// These describe the *minimum* structure this adapter reads. The generated
// types satisfy them structurally whichever `BigIntExportBehavior` was chosen,
// which is the point: a change there is a compile error in this file only.
// ---------------------------------------------------------------------------

interface WireChildRow {
  node: NumLike;
  name: string;
  kind: Kind;
  category: NumLike;
  logical: NumLike;
  allocated: NumLike;
  mtime: NumLike;
  flags: NumLike;
  children: NumLike;
  is_virtual_group: boolean;
}

interface WireDirTotals {
  logical: NumLike;
  allocated: NumLike;
  direct_logical: NumLike;
  direct_allocated: NumLike;
  latest_mtime: NumLike;
  observed_entries: NumLike;
  retained_nodes: NumLike;
  direct_files: NumLike;
  unreadable: NumLike;
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/** One row of the hierarchy table. `logical`/`allocated` are never summed together. */
export interface TreeRow {
  readonly node: number;
  readonly name: string;
  readonly kind: Kind;
  readonly category: number;
  readonly logical: number;
  readonly allocated: number;
  readonly mtime: number;
  readonly flags: number;
  /** Child count, so the expander can be drawn without a probe request. */
  readonly children: number;
  /**
   * The synthetic `<Files>` group. It has a tagged id and **cannot be revealed
   * or trashed as a path** — there is no such directory entry.
   */
  readonly isVirtualGroup: boolean;
}

export interface ChildPageView {
  readonly generation: number;
  readonly parent: number;
  readonly rows: readonly TreeRow[];
  /** Opaque; hand it back verbatim. Bound to generation + parent + sort in Rust. */
  readonly next: string | null;
  readonly totalChildren: number;
  readonly limit: number;
}

export interface DirTotalsView {
  readonly logical: number;
  readonly allocated: number;
  readonly directLogical: number;
  readonly directAllocated: number;
  readonly latestMtime: number;
  readonly observedEntries: number;
  readonly retainedNodes: number;
  readonly directFiles: number;
  readonly unreadable: number;
}

export interface DetailsView {
  readonly generation: number;
  readonly node: number;
  /** Display only. **Never** action authority — see `DisplayPath` in the contract. */
  readonly path: string;
  readonly name: string;
  readonly kind: Kind;
  readonly category: number;
  readonly logical: number;
  readonly allocated: number;
  readonly mtime: number;
  readonly flags: number;
  readonly subtree: DirTotalsView | null;
  readonly countedAt: string | null;
  readonly isPackage: boolean;
}

export interface VolumeRow {
  readonly name: string;
  readonly mountPoint: string;
  readonly device: number;
  readonly fsType: string;
  /**
   * Capacity of the *container*, on APFS. Every volume sharing a container
   * reports the same number, which is why the picker groups them and states
   * this once rather than per row.
   */
  readonly totalBytes: number;
  /** Free space in the container, on APFS. Shared, like `totalBytes`. */
  readonly availableBytes: number;
  /**
   * What this volume itself occupies. The only capacity number here that is
   * private to the volume — never `totalBytes - availableBytes`, which on APFS
   * is the whole container's usage.
   */
  readonly usedBytes: number;
  /** `/dev/disk3s1s1`. */
  readonly deviceNode: string;
  /** APFS container reference (`disk3`), or the whole disk for non-APFS. */
  readonly containerId: string | null;
  /** The physical disk backing the container (`disk0`). */
  readonly diskId: string | null;
  /** Media name of that disk, e.g. `APPLE SSD AP1024Z`. */
  readonly diskName: string | null;
  /** Size of the physical disk, which is larger than any one container. */
  readonly diskSizeBytes: number | null;
  readonly isInternal: boolean;
  /** A macOS-owned volume (`Preboot`, `VM`, `Update`, …), not a user volume. */
  readonly isSystem: boolean;
  /**
   * macOS's "available for important usage" — larger than `availableBytes`
   * because it counts purgeable space the system would reclaim under pressure.
   * `null` when the API did not supply it.
   */
  readonly importantAvailableBytes: number | null;
  readonly isRootVolume: boolean;
  readonly isRemovable: boolean;
  /** Presence only. v1 does not claim a snapshot's byte size — `tmutil` has no authoritative total. */
  readonly hasLocalSnapshots: boolean;
}

export interface ScanProgressView {
  readonly scanId: number;
  readonly sequence: number;
  readonly state: ScanState;
  readonly observedEntries: number;
  readonly retainedNodes: number;
  readonly directories: number;
  readonly logicalBytes: number;
  readonly allocatedBytes: number;
  readonly errors: number;
  readonly mutations: number;
  readonly pendingDirs: number;
  readonly resultQueueDepth: number;
  readonly elapsedMs: number;
  readonly currentDir: string | null;
  readonly rssBytes: number;
  readonly projectedPeakRssBytes: number;
}

export interface ErrorClassCountView {
  readonly errorClass: string;
  readonly operation: string | null;
  readonly count: number;
}

export interface ScanSummaryView {
  readonly scanId: number;
  readonly generation: number;
  readonly root: number;
  readonly rootPath: string;
  readonly startedUnixMs: number;
  readonly finishedUnixMs: number;
  readonly toolVersion: string;
  readonly aggregated: boolean;
  readonly partial: boolean;
  readonly mutations: number;
  readonly counts: {
    readonly observedEntries: number;
    readonly retainedNodes: number;
    readonly directories: number;
    readonly files: number;
    readonly symlinks: number;
    readonly special: number;
    readonly unreadableDirs: number;
    readonly excludedPaths: number;
    readonly aggregatedNodes: number;
    readonly hardLinkRepeats: number;
  };
  readonly totals: { readonly logical: number; readonly allocated: number };
  readonly errorCounts: readonly ErrorClassCountView[];
  readonly excludedRoots: readonly string[];
}

export interface ScanStatusView {
  readonly state: ScanState;
  readonly activeScan: number | null;
  readonly generation: number;
  readonly summary: ScanSummaryView | null;
  readonly lastProgress: ScanProgressView | null;
}

// ---------------------------------------------------------------------------
// Mappers
// ---------------------------------------------------------------------------

function toTreeRow(row: WireChildRow): TreeRow {
  return {
    node: num(row.node),
    name: row.name,
    kind: row.kind,
    category: num(row.category),
    logical: num(row.logical),
    allocated: num(row.allocated),
    mtime: num(row.mtime),
    flags: num(row.flags),
    children: num(row.children),
    isVirtualGroup: row.is_virtual_group,
  };
}

function toDirTotals(totals: WireDirTotals): DirTotalsView {
  return {
    logical: num(totals.logical),
    allocated: num(totals.allocated),
    directLogical: num(totals.direct_logical),
    directAllocated: num(totals.direct_allocated),
    latestMtime: num(totals.latest_mtime),
    observedEntries: num(totals.observed_entries),
    retainedNodes: num(totals.retained_nodes),
    directFiles: num(totals.direct_files),
    unreadable: num(totals.unreadable),
  };
}

function toProgress(progress: {
  scan_id: NumLike;
  sequence: NumLike;
  state: ScanState;
  observed_entries: NumLike;
  retained_nodes: NumLike;
  directories: NumLike;
  logical_bytes: NumLike;
  allocated_bytes: NumLike;
  errors: NumLike;
  mutations: NumLike;
  pending_dirs: NumLike;
  result_queue_depth: NumLike;
  elapsed_ms: NumLike;
  current_dir: string | null;
  rss_bytes: NumLike;
  projected_peak_rss_bytes: NumLike;
}): ScanProgressView {
  return {
    scanId: num(progress.scan_id),
    sequence: num(progress.sequence),
    state: progress.state,
    observedEntries: num(progress.observed_entries),
    retainedNodes: num(progress.retained_nodes),
    directories: num(progress.directories),
    logicalBytes: num(progress.logical_bytes),
    allocatedBytes: num(progress.allocated_bytes),
    errors: num(progress.errors),
    mutations: num(progress.mutations),
    pendingDirs: num(progress.pending_dirs),
    resultQueueDepth: num(progress.result_queue_depth),
    elapsedMs: num(progress.elapsed_ms),
    currentDir: progress.current_dir,
    rssBytes: num(progress.rss_bytes),
    projectedPeakRssBytes: num(progress.projected_peak_rss_bytes),
  };
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/** Default options == `ScanOptions::default()`: full detail, no crossing, defaults on. */
export function defaultScanOptions(): ScanOptions {
  return {
    cross_filesystems: false,
    count_hard_links_once: true,
    apply_default_exclusions: true,
    aggregate_below_bytes: null,
    workers: null,
    memory_limit_bytes: null,
    exclusions: [],
  };
}

export const DEFAULT_SORT: Sort = { key: "logical", direction: "descending" };

/**
 * View-model `number` -> the wire's `u64`.
 *
 * `TreeGeneration` and `ScanId` are `u64` newtypes. `src-tauri` exports them
 * with `dangerously_cast_bigints_to_number()`, so `src/lib/bindings.ts`
 * declares `export type ScanId = number` and this returns a `number`.
 *
 * That is a deliberate choice made in `src-tauri/src/lib.rs`, not an accident:
 * Tauri IPC is JSON, so a `u64` already arrives as a JS `number` regardless,
 * and `enable_lossless_bigints()` on tauri-specta rc.25 emits `BigInt(x[0])`
 * wrappers that throw at runtime. Everything above this file uses `number`
 * anyway — a generation counter and a scan counter are small integers, and
 * threading `bigint` through React state and query keys buys nothing but
 * `Cannot mix BigInt and other types` at every arithmetic site.
 *
 * If the exporter ever switches back to `BigIntExportBehavior::BigInt`, the
 * return type here becomes a compile error in this file and nowhere else,
 * which is the whole reason the conversion is centralised.
 *
 * `Math.trunc` keeps a fractional value from reaching the wire as a `u64`.
 */
function toWireU64(value: number): number {
  return Math.trunc(Number.isFinite(value) ? value : 0);
}

/** Returns immediately with a `ScanId`; the scan itself reports through events. */
export async function scanStart(root: string, options: ScanOptions = defaultScanOptions()): Promise<number> {
  return num(await unwrap("scan_start", commands.scanStart(root, options)));
}

/**
 * Asks the main window to show its settings.
 *
 * The menu-bar panel is a second webview with its own React tree, so it cannot
 * call the shell's router. It shows the window and emits this instead, which
 * keeps every route change owned by the shell that draws the routes — a tray
 * that navigated directly would be a second, weaker copy of that logic.
 */
export const OPEN_SETTINGS_EVENT = "rdirstat://open-settings";

/** One child directory in a destination listing. */
export interface BrowseEntryView {
  readonly name: string;
  readonly path: string;
}

/** What is inside a directory, for choosing a move destination. */
export interface BrowseListingView {
  /** The directory actually read, after `~` expansion and canonicalisation. */
  readonly path: string;
  /** `null` at the filesystem root, where there is no "up". */
  readonly parent: string | null;
  readonly directories: readonly BrowseEntryView[];
  /** The listing was cut. Say so; a silent cut hides the folder being sought. */
  readonly truncated: boolean;
  /**
   * Why nothing could be listed. `null` and an empty `directories` are
   * different states — "empty" and "I could not look" must never render alike,
   * or a permission error reads as a valid, empty destination.
   */
  readonly unreadable: string | null;
}

/**
 * Lists the child directories of a path, for the destination pane.
 *
 * Unlike `completePath` this reports failure, because it answers a different
 * question: a completion is a guess offered mid-keystroke, where an error is
 * noise; a browse is a deliberate "show me what is in here", where "you cannot
 * read this" is the answer.
 */
export async function browseDirectories(path: string): Promise<BrowseListingView> {
  const listing = await commands.browseDirectories(path);
  return {
    path: listing.path,
    parent: listing.parent,
    directories: listing.directories,
    truncated: listing.truncated,
    unreadable: listing.unreadable,
  };
}

/**
 * Directory completions for a partially typed path.
 *
 * Never rejects. The backend returns an empty list for a path that does not
 * exist, one it cannot read, and one naming a file — because every one of
 * those is a normal intermediate state while someone is still typing, not a
 * failure worth interrupting them over. The error that *does* mean something
 * arrives from `scanStart`.
 */
export async function completePath(prefix: string): Promise<string[]> {
  return commands.completePath(prefix);
}

export async function scanCancel(scanId: number): Promise<CancelState> {
  return unwrap("scan_cancel", commands.scanCancel(toWireU64(scanId)));
}

export async function scanStatus(): Promise<ScanStatusView> {
  const status = await unwrap("scan_status", commands.scanStatus());
  return {
    state: status.state,
    activeScan: status.active_scan === null ? null : num(status.active_scan),
    generation: num(status.generation),
    summary:
      status.summary === null
        ? null
        : {
            scanId: num(status.summary.scan_id),
            generation: num(status.summary.generation),
            root: num(status.summary.root),
            rootPath: status.summary.root_path,
            startedUnixMs: num(status.summary.started_unix_ms),
            finishedUnixMs: num(status.summary.finished_unix_ms),
            toolVersion: status.summary.tool_version,
            aggregated: status.summary.aggregated,
            partial: status.summary.partial,
            mutations: num(status.summary.mutations),
            counts: {
              observedEntries: num(status.summary.counts.observed_entries),
              retainedNodes: num(status.summary.counts.retained_nodes),
              directories: num(status.summary.counts.directories),
              files: num(status.summary.counts.files),
              symlinks: num(status.summary.counts.symlinks),
              special: num(status.summary.counts.special),
              unreadableDirs: num(status.summary.counts.unreadable_dirs),
              excludedPaths: num(status.summary.counts.excluded_paths),
              aggregatedNodes: num(status.summary.counts.aggregated_nodes),
              hardLinkRepeats: num(status.summary.counts.hard_link_repeats),
            },
            totals: {
              logical: num(status.summary.totals.logical),
              allocated: num(status.summary.totals.allocated),
            },
            errorCounts: status.summary.error_counts.map((entry) => ({
              errorClass: entry.class,
              operation: entry.operation,
              count: num(entry.count),
            })),
            excludedRoots: status.summary.excluded_roots,
          },
    lastProgress: status.last_progress === null ? null : toProgress(status.last_progress),
  };
}

export async function volumes(): Promise<VolumeRow[]> {
  const rows = await unwrap("volumes", commands.volumes());
  return rows.map((volume) => ({
    name: volume.name,
    mountPoint: volume.mount_point,
    device: num(volume.device),
    fsType: volume.fs_type,
    totalBytes: num(volume.total_bytes),
    availableBytes: num(volume.available_bytes),
    usedBytes: num(volume.used_bytes),
    deviceNode: volume.device_node,
    containerId: volume.container_id,
    diskId: volume.disk_id,
    diskName: volume.disk_name,
    diskSizeBytes: volume.disk_size_bytes === null ? null : num(volume.disk_size_bytes),
    isInternal: volume.is_internal,
    isSystem: volume.is_system,
    importantAvailableBytes:
      volume.important_available_bytes === null ? null : num(volume.important_available_bytes),
    isRootVolume: volume.is_root_volume,
    isRemovable: volume.is_removable,
    hasLocalSnapshots: volume.has_local_snapshots,
  }));
}

/**
 * What a scan's recorded failures were.
 *
 * The counter in the status strip says *how many*; this is the only thing in
 * the contract that says *what*. It answers from the running scan while one is
 * active and from the published tree afterwards, so the affordance behaves the
 * same in both states — `live` says which one you got.
 *
 * The samples are flattened here rather than in a component: `ScanError` is a
 * tagged union whose variants carry different fields, and exactly one place
 * should know that `vanished` has no `os_code` and `memory_limit` has no path.
 */
export interface ScanFailureRow {
  readonly kind: string;
  readonly errorClass: ErrorClass;
  readonly operation: Operation | null;
  /** `null` for the failures that are not about one path. */
  readonly path: string | null;
  readonly osCode: number | null;
  /** A one-line rendering, for a row that has nothing better to show. */
  readonly detail: string;
}

export interface ScanErrorsView {
  readonly live: boolean;
  readonly generation: number;
  readonly total: number;
  readonly counts: readonly {
    readonly errorClass: ErrorClass;
    readonly operation: Operation | null;
    readonly count: number;
  }[];
  readonly samples: readonly ScanFailureRow[];
  readonly truncated: boolean;
}

/** How many sample failures to ask for. The backend caps this at 200. */
export const SCAN_ERROR_SAMPLE_LIMIT = 100;

export async function scanErrors(limit: number = SCAN_ERROR_SAMPLE_LIMIT): Promise<ScanErrorsView> {
  const report = await unwrap("scan_errors", commands.scanErrors(limit));
  return {
    live: report.live,
    generation: num(report.generation),
    total: num(report.total),
    counts: report.counts.map((entry) => ({
      errorClass: entry.class,
      operation: entry.operation,
      count: num(entry.count),
    })),
    samples: report.samples.map(toFailureRow),
    truncated: report.truncated,
  };
}

function toFailureRow(error: ScanError): ScanFailureRow {
  switch (error.kind) {
    case "permission_denied":
      return {
        kind: error.kind,
        errorClass: "permission_denied",
        operation: error.detail.operation,
        path: error.detail.path,
        osCode: num(error.detail.os_code),
        detail: "macOS refused access",
      };
    case "vanished":
      return {
        kind: error.kind,
        errorClass: "not_found",
        operation: error.detail.operation,
        path: error.detail.path,
        osCode: null,
        detail: "removed or renamed while it was being read",
      };
    case "invalid_name":
      return {
        kind: error.kind,
        errorClass: "invalid_name",
        operation: null,
        path: error.detail.path,
        osCode: null,
        detail: "the name is not valid UTF-8; the bytes are kept, the text is escaped",
      };
    case "io":
      return {
        kind: error.kind,
        errorClass: error.detail.class,
        operation: error.detail.operation,
        path: error.detail.path,
        osCode: num(error.detail.os_code),
        detail: `errno ${num(error.detail.os_code)}`,
      };
    case "memory_limit":
      return {
        kind: error.kind,
        errorClass: "other",
        operation: null,
        path: null,
        osCode: null,
        detail: `projected ${num(error.detail.projected_bytes)} B over the ${num(error.detail.limit_bytes)} B limit`,
      };
    case "root_unavailable":
      return {
        kind: error.kind,
        errorClass: error.detail.class,
        operation: null,
        path: error.detail.path,
        osCode: null,
        detail: error.detail.reason,
      };
    default:
      // `ScanError` is `#[non_exhaustive]`: a variant added in Rust must show
      // up as an unexplained row rather than crash the panel that lists it.
      return {
        kind: "unknown",
        errorClass: "other",
        operation: null,
        path: null,
        osCode: null,
        detail: "an unrecognised failure kind",
      };
  }
}

/**
 * One page of children. `limit` is clamped here as well as in Rust — asking for
 * more than 500 is `QueryError::LimitExceeded`, and a UI that trips that has a
 * bug rather than an error to display.
 */
export async function children(
  generation: number,
  item: number,
  sort: Sort,
  cursor: string | null,
  limit: number,
): Promise<ChildPageView> {
  const clamped = Math.max(1, Math.min(MAX_CHILD_PAGE, Math.floor(limit)));
  const page = await unwrap("children", commands.children(toWireU64(generation), item, sort, cursor, clamped));
  return {
    generation: num(page.generation),
    parent: num(page.parent),
    rows: page.rows.map(toTreeRow),
    next: page.next,
    totalChildren: num(page.total_children),
    limit: num(page.limit),
  };
}

/**
 * One row of the size-band histogram, with both unit systems already resolved.
 *
 * Band edges are binary because the user's reference is `du -h`, which is
 * 1024-based; the rest of the interface is decimal SI because Finder is. Rather
 * than pick a winner, every edge carries both renderings and the UI shows the
 * conversion.
 */
export interface SizeBandView {
  readonly band: number;
  readonly lowerBytes: number;
  /** `null` on the open-ended top band. */
  readonly upperBytes: number | null;
  readonly files: number;
  readonly logical: number;
  readonly allocated: number;
}

/** One file inside a band. The breakdown is a leaderboard, not an enumeration. */
export interface SizeBandEntryView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly logical: number;
  readonly mtime: number;
  readonly category: number;
}

export async function sizeBandEntries(
  generation: number,
  node: number,
  band: number,
  limit: number,
): Promise<SizeBandEntryView[]> {
  const rows = await unwrap(
    "size_band_entries",
    commands.sizeBandEntries(toWireU64(generation), node, band, limit),
  );
  return rows.map((row) => ({
    node: num(row.node),
    path: row.path,
    allocated: num(row.allocated),
    logical: num(row.logical),
    mtime: num(row.mtime),
    category: num(row.category),
  }));
}

/** One content-category row of the Types report. */
export interface CategoryRowView {
  readonly category: number;
  readonly files: number;
  readonly logical: number;
  readonly allocated: number;
}

/** One file inside a category. A leaderboard row, not an enumeration. */
export interface CategoryEntryView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly logical: number;
  readonly mtime: number;
}

export async function categoryTotals(generation: number, node: number): Promise<CategoryRowView[]> {
  const rows = await unwrap("category_totals", commands.categoryTotals(toWireU64(generation), node));
  return rows.map((row) => ({
    category: num(row.category),
    files: num(row.files),
    logical: num(row.logical),
    allocated: num(row.allocated),
  }));
}

export async function categoryEntries(
  generation: number,
  node: number,
  category: number,
  limit: number,
): Promise<CategoryEntryView[]> {
  const rows = await unwrap(
    "category_entries",
    commands.categoryEntries(toWireU64(generation), node, category, limit),
  );
  return rows.map((row) => ({
    node: num(row.node),
    path: row.path,
    allocated: num(row.allocated),
    logical: num(row.logical),
    mtime: num(row.mtime),
  }));
}

/** One age bucket of the Ages report. */
export interface AgeBucketView {
  readonly bucket: number;
  readonly lowerSeconds: number;
  /** `null` on the open-ended oldest bucket. */
  readonly upperSeconds: number | null;
  readonly files: number;
  readonly logical: number;
  readonly allocated: number;
}

export interface AgeBucketEntryView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly logical: number;
  readonly mtime: number;
  readonly category: number;
}

export async function ageBuckets(
  generation: number,
  node: number,
  nowUnixSeconds: number,
): Promise<AgeBucketView[]> {
  const rows = await unwrap("age_buckets", commands.ageBuckets(toWireU64(generation), node, nowUnixSeconds));
  return rows.map((row) => ({
    bucket: num(row.bucket),
    lowerSeconds: num(row.lower_seconds),
    upperSeconds: row.upper_seconds === null ? null : num(row.upper_seconds),
    files: num(row.files),
    logical: num(row.logical),
    allocated: num(row.allocated),
  }));
}

export async function ageBucketEntries(
  generation: number,
  node: number,
  nowUnixSeconds: number,
  bucket: number,
  limit: number,
): Promise<AgeBucketEntryView[]> {
  const rows = await unwrap(
    "age_bucket_entries",
    commands.ageBucketEntries(toWireU64(generation), node, nowUnixSeconds, bucket, limit),
  );
  return rows.map((row) => ({
    node: num(row.node),
    path: row.path,
    allocated: num(row.allocated),
    logical: num(row.logical),
    mtime: num(row.mtime),
    category: num(row.category),
  }));
}

/**
 * The Dupes report: files that share a logical size.
 *
 * CANDIDATES, not duplicates. Nothing was opened and no content was hashed, so
 * `contentVerified` is always false at this stage and the recovery figure is an
 * upper bound rather than a promise. Same size is not same content, and on APFS
 * two copies may already be clones sharing their storage.
 */
export interface DupesMemberView {
  readonly node: number;
  readonly path: string;
  readonly allocated: number;
  readonly mtime: number;
  readonly category: number;
  readonly hardLinked: boolean;
}

export interface DupesClusterView {
  readonly logicalBytes: number;
  readonly memberCount: number;
  readonly members: readonly DupesMemberView[];
  readonly membersOmitted: number;
  readonly potentialRecoveryLowerBytes: number;
  readonly potentialRecoveryUpperBytes: number;
}

export interface DupesReportView {
  readonly clusters: readonly DupesClusterView[];
  readonly clustersFound: number;
  readonly clustersOmitted: number;
  readonly filesInClusters: number;
  readonly potentialRecoveryUpperBytes: number;
  readonly contentVerified: boolean;
  readonly clusterLimit: number;
  readonly memberLimit: number;
  readonly emptyFilesSkipped: number;
  readonly hardLinkRepeatsSkipped: number;
  readonly filesUngrouped: number;
}

export async function duplicateCandidates(
  generation: number,
  node: number,
  maxClusters: number,
  maxMembers: number,
): Promise<DupesReportView> {
  const report = await unwrap(
    "duplicate_candidates",
    commands.duplicateCandidates(toWireU64(generation), node, maxClusters, maxMembers),
  );
  return {
    clusters: report.clusters.map((cluster) => ({
      logicalBytes: num(cluster.logical_bytes),
      memberCount: num(cluster.member_count),
      members: cluster.members.map((member) => ({
        node: num(member.node),
        path: member.path,
        allocated: num(member.allocated),
        mtime: num(member.mtime),
        category: num(member.category),
        hardLinked: member.hard_linked,
      })),
      membersOmitted: num(cluster.members_omitted),
      potentialRecoveryLowerBytes: num(cluster.potential_recovery_lower_bytes),
      potentialRecoveryUpperBytes: num(cluster.potential_recovery_upper_bytes),
    })),
    clustersFound: num(report.clusters_found),
    clustersOmitted: num(report.clusters_omitted),
    filesInClusters: num(report.files_in_clusters),
    potentialRecoveryUpperBytes: num(report.potential_recovery_upper_bytes),
    contentVerified: report.content_verified,
    clusterLimit: num(report.cluster_limit),
    memberLimit: num(report.member_limit),
    emptyFilesSkipped: num(report.empty_files_skipped),
    hardLinkRepeatsSkipped: num(report.hard_link_repeats_skipped),
    filesUngrouped: num(report.files_ungrouped),
  };
}

/** The Diff report: what changed between the previous scan and this one. */
export type DiffMetricKind = "logical" | "allocated";
export type DiffChangeKind = "added" | "removed" | "grown" | "shrunk";
export type DiffSideKind = "before" | "after";

export interface DiffScanRef {
  readonly root: string;
  readonly takenUnixMs: number | null;
  readonly nodes: number;
}

export interface DiffClassTotalsView {
  readonly entries: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
}

export interface DiffEntryView {
  readonly change: DiffChangeKind;
  readonly side: DiffSideKind;
  readonly node: number;
  readonly path: string;
  readonly kind: Kind;
  readonly kindChanged: boolean;
  readonly entries: number;
  readonly beforeLogical: number;
  readonly beforeAllocated: number;
  readonly afterLogical: number;
  readonly afterAllocated: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
  readonly beforeMtime: number | null;
  readonly afterMtime: number | null;
  readonly category: number;
}

export interface DiffSummaryView {
  readonly added: DiffClassTotalsView;
  readonly removed: DiffClassTotalsView;
  readonly grown: DiffClassTotalsView;
  readonly shrunk: DiffClassTotalsView;
  readonly metadataChanged: number;
  readonly kindChanged: number;
  readonly unchanged: number;
  readonly compared: number;
  readonly descended: number;
  readonly beforeLogical: number;
  readonly beforeAllocated: number;
  readonly afterLogical: number;
  readonly afterAllocated: number;
  readonly logicalDelta: number;
  readonly allocatedDelta: number;
  readonly truncated: boolean;
}

export interface DiffReportView {
  readonly before: DiffScanRef;
  readonly after: DiffScanRef;
  readonly metric: DiffMetricKind;
  readonly limit: number;
  readonly summary: DiffSummaryView;
  readonly added: readonly DiffEntryView[];
  readonly removed: readonly DiffEntryView[];
  readonly grown: readonly DiffEntryView[];
  readonly shrunk: readonly DiffEntryView[];
}

export async function scanDiff(
  generation: number,
  metric: DiffMetricKind,
  limit: number,
): Promise<DiffReportView> {
  const report = await unwrap("scan_diff", commands.scanDiff(toWireU64(generation), metric, limit));
  const scanRef = (side: { root: string; taken_unix_ms: number | null; nodes: number }): DiffScanRef => ({
    root: side.root,
    takenUnixMs: side.taken_unix_ms === null ? null : num(side.taken_unix_ms),
    nodes: num(side.nodes),
  });
  const totals = (value: {
    entries: number;
    logical_delta: number;
    allocated_delta: number;
  }): DiffClassTotalsView => ({
    entries: num(value.entries),
    logicalDelta: num(value.logical_delta),
    allocatedDelta: num(value.allocated_delta),
  });
  const entry = (row: (typeof report.added)[number]): DiffEntryView => ({
    change: row.change,
    side: row.side,
    node: num(row.node),
    path: row.path,
    kind: row.kind,
    kindChanged: row.kind_changed,
    entries: num(row.entries),
    beforeLogical: num(row.before_logical),
    beforeAllocated: num(row.before_allocated),
    afterLogical: num(row.after_logical),
    afterAllocated: num(row.after_allocated),
    logicalDelta: num(row.logical_delta),
    allocatedDelta: num(row.allocated_delta),
    beforeMtime: row.before_mtime === null ? null : num(row.before_mtime),
    afterMtime: row.after_mtime === null ? null : num(row.after_mtime),
    category: num(row.category),
  });
  return {
    before: scanRef(report.before),
    after: scanRef(report.after),
    metric: report.metric,
    limit: num(report.limit),
    summary: {
      added: totals(report.summary.added),
      removed: totals(report.summary.removed),
      grown: totals(report.summary.grown),
      shrunk: totals(report.summary.shrunk),
      metadataChanged: num(report.summary.metadata_changed),
      kindChanged: num(report.summary.kind_changed),
      unchanged: num(report.summary.unchanged),
      compared: num(report.summary.compared),
      descended: num(report.summary.descended),
      beforeLogical: num(report.summary.before_logical),
      beforeAllocated: num(report.summary.before_allocated),
      afterLogical: num(report.summary.after_logical),
      afterAllocated: num(report.summary.after_allocated),
      logicalDelta: num(report.summary.logical_delta),
      allocatedDelta: num(report.summary.allocated_delta),
      truncated: report.summary.truncated,
    },
    added: report.added.map(entry),
    removed: report.removed.map(entry),
    grown: report.grown.map(entry),
    shrunk: report.shrunk.map(entry),
  };
}

export async function sizeBands(generation: number, node: number): Promise<SizeBandView[]> {
  const rows = await unwrap("size_bands", commands.sizeBands(toWireU64(generation), node));
  return rows.map((row) => ({
    band: num(row.band),
    lowerBytes: num(row.lower_bytes),
    upperBytes: row.upper_bytes === null ? null : num(row.upper_bytes),
    files: num(row.files),
    logical: num(row.logical),
    allocated: num(row.allocated),
  }));
}

export async function nodeDetails(generation: number, node: number): Promise<DetailsView> {
  const details = await unwrap("node_details", commands.nodeDetails(toWireU64(generation), node));
  return {
    generation: num(details.generation),
    node: num(details.node),
    path: details.path,
    name: details.name,
    kind: details.kind,
    category: num(details.category),
    logical: num(details.logical),
    allocated: num(details.allocated),
    mtime: num(details.mtime),
    flags: num(details.flags),
    subtree: details.subtree === null ? null : toDirTotals(details.subtree),
    countedAt: details.counted_at,
    isPackage: details.is_package,
  };
}

export async function pathOf(generation: number, item: number): Promise<string> {
  return unwrap("path_of", commands.pathOf(toWireU64(generation), item));
}

export async function revealInFinder(generation: number, node: number): Promise<void> {
  await unwrap("reveal_in_finder", commands.revealInFinder(toWireU64(generation), node));
}

// ---------------------------------------------------------------------------
// Folder sync
// ---------------------------------------------------------------------------

export type CompareMode = "quick" | "verify";
export type OnDiffer = "skip" | "replace";
export type SyncReason = "missing" | "size_differs" | "content_differs";

export interface SyncEntryView {
  readonly relativePath: string;
  readonly bytes: number;
  readonly reason: SyncReason;
}

export interface SyncWarningView {
  readonly code: string;
  readonly message: string;
}

export interface SyncPlanView {
  /** `null` means there is nothing to copy, or no room to copy it. */
  readonly token: string | null;
  readonly source: string;
  readonly destination: string;
  readonly entries: readonly SyncEntryView[];
  /** True count, even when `entries` was truncated for display. */
  readonly totalToCopy: number;
  readonly bytesToCopy: number;
  readonly alreadyPresent: number;
  readonly differingSkipped: number;
  readonly specialSkipped: number;
  readonly unreadable: number;
  readonly destinationAvailable: number;
  readonly destinationFilesystem: string;
  readonly entriesTruncated: boolean;
  readonly warnings: readonly SyncWarningView[];
}

export interface SyncReportView {
  readonly source: string;
  readonly destination: string;
  readonly copied: number;
  readonly bytesCopied: number;
  readonly failures: readonly { readonly relativePath: string; readonly reason: string }[];
}

/** What a sync would copy. Never writes anything. */
export async function syncPlan(
  source: string,
  destination: string,
  compareMode: CompareMode,
  onDiffer: OnDiffer,
): Promise<SyncPlanView> {
  const plan = await unwrap("sync_plan", commands.syncPlan(source, destination, compareMode, onDiffer));
  return {
    token: plan.token,
    source: plan.source,
    destination: plan.destination,
    entries: plan.entries.map((entry) => ({
      relativePath: entry.relative_path,
      bytes: num(entry.bytes),
      reason: entry.reason,
    })),
    totalToCopy: num(plan.total_to_copy),
    bytesToCopy: num(plan.bytes_to_copy),
    alreadyPresent: num(plan.already_present),
    differingSkipped: num(plan.differing_skipped),
    specialSkipped: num(plan.special_skipped),
    unreadable: num(plan.unreadable),
    destinationAvailable: num(plan.destination_available),
    destinationFilesystem: plan.destination_filesystem,
    entriesTruncated: plan.entries_truncated,
    warnings: plan.warnings.map((warning) => ({ code: warning.code, message: warning.message })),
  };
}

/**
 * Runs a planned sync. Long-running.
 *
 * The backend re-plans and re-checks the token, so a source that changed since
 * the review fails closed rather than copying something never seen counted.
 */
export async function syncApply(
  source: string,
  destination: string,
  compareMode: CompareMode,
  onDiffer: OnDiffer,
  token: string,
): Promise<SyncReportView> {
  const report = await unwrap(
    "sync_apply",
    commands.syncApply(source, destination, compareMode, onDiffer, token),
  );
  return {
    source: report.source,
    destination: report.destination,
    copied: num(report.copied),
    bytesCopied: num(report.bytes_copied),
    failures: report.failures.map((failure) => ({
      relativePath: failure.relative_path,
      reason: failure.reason,
    })),
  };
}

// ---------------------------------------------------------------------------
// Choosing a folder
// ---------------------------------------------------------------------------

/**
 * Ask macOS for a folder, and return it — or null if the user cancelled.
 *
 * This lives here rather than in a component for the reason every other call
 * in this file does: it is the process boundary, and a component that reaches
 * across it directly is one the boundary cannot be changed underneath.
 *
 * It is also not merely a convenience over typing a path.
 * docs/03-MACOS.md#privacy-controls-and-full-disk-access names the native open
 * panel as the *explicit consent* path for files and folders: a directory the
 * user picked here is one macOS has authorised this process to read, and a
 * directory they typed is not. Two paths that look identical in a text field
 * can therefore behave differently, and picking is the one that works.
 *
 * `startingAt` is where the panel opens. Passing the field's current value
 * makes Browse a refinement of what is already there rather than a restart
 * from the home folder; a blank or bogus value is ignored by the platform,
 * which is why a half-typed path is not an error here.
 */
export async function pickFolder(title: string, startingAt?: string): Promise<string | null> {
  const chosen = await openDialog({
    title,
    directory: true,
    multiple: false,
    defaultPath: startingAt !== undefined && startingAt.trim() !== "" ? startingAt : undefined,
  });
  if (chosen === null) return null;
  // The plugin's return type widens to string[] when `multiple` is set. We
  // asked for one, so anything else is a contract break worth failing on
  // rather than silently coercing into a path.
  if (typeof chosen !== "string") {
    throw new IpcError("pick_folder", {
      kind: "internal",
      detail: "the folder chooser returned more than one path",
    });
  }
  return chosen;
}

// ---------------------------------------------------------------------------
// Stored data
// ---------------------------------------------------------------------------

export interface StoredSnapshotView {
  readonly path: string;
  readonly rootPath: string;
  readonly device: number;
  readonly takenUnixMs: number;
  readonly nodes: number;
  readonly directories: number;
  /** What the snapshot file costs on disk. */
  readonly bytes: number;
  /** What it is *about* — the volume it measured. A different quantity. */
  readonly logical: number;
  readonly allocated: number;
  readonly toolVersion: string;
}

export interface UnreadableSnapshotView {
  readonly path: string;
  readonly bytes: number;
  readonly reason: string;
}

/**
 * Whether the store directory is there and usable.
 *
 * Three states rather than two booleans, mirroring the backend: a directory
 * that exists and rejects writes — an unplugged disk's mount point, a folder
 * owned by another account — looks exactly like an empty store until a scan
 * finishes and cannot be saved.
 */
export type DirectoryStateView = "missing" | "readOnly" | "writable";

/** Which layer of the resolution order chose the store location. */
export type DirectorySourceView = "environment" | "setting" | "default";

export interface StorageReportView {
  readonly directory: string;
  readonly directoryState: DirectoryStateView;
  /**
   * True when the location came from `RDIRSTAT_DATA_DIR`. The panel disables
   * its editor and says why: an edit that saves a setting the environment is
   * about to override would look like a bug.
   */
  readonly directoryLocked: boolean;
  readonly directorySource: DirectorySourceView;
  /** What "reset to default" would select. */
  readonly defaultDirectory: string;
  /** The saved setting, shown even when the environment is overriding it. */
  readonly configuredDirectory: string | null;
  readonly snapshots: readonly StoredSnapshotView[];
  readonly unreadable: readonly UnreadableSnapshotView[];
  readonly totalBytes: number;
  readonly truncated: boolean;
  /**
   * False in every current build. The DuckDB/Parquet catalog in
   * docs/06-DATA.md is a documented future phase, and the UI says so rather
   * than rendering an empty database that reads as broken.
   */
  readonly catalogPresent: boolean;
}

/** Everything the app keeps on disk. Peeks headers only; never decodes an arena. */
export async function storageReport(): Promise<StorageReportView> {
  const report = await unwrap("storage_report", commands.storageReport());
  return toStorageReport(report);
}

/**
 * Point the store at `directory`, or at the default when it is null.
 *
 * The backend validates before it saves — an unwritable folder is rejected
 * here rather than at the end of the next scan — and returns the fresh report,
 * so the panel never has to guess what took effect.
 */
export async function setSnapshotDir(directory: string | null): Promise<StorageReportView> {
  return toStorageReport(await unwrap("set_snapshot_dir", commands.setSnapshotDir(directory)));
}

/** The one place the wire shape becomes the view model. */
function toStorageReport(report: StorageReport): StorageReportView {
  return {
    directory: report.directory,
    directoryState:
      report.directory_state === "read_only" ? "readOnly" : report.directory_state,
    directoryLocked: report.directory_source === "environment",
    directorySource: report.directory_source,
    defaultDirectory: report.default_directory,
    configuredDirectory: report.configured_directory,
    snapshots: report.snapshots.map((snapshot) => ({
      path: snapshot.path,
      rootPath: snapshot.root_path,
      device: num(snapshot.device),
      takenUnixMs: num(snapshot.taken_unix_ms),
      nodes: num(snapshot.nodes),
      directories: num(snapshot.directories),
      bytes: num(snapshot.bytes),
      logical: num(snapshot.logical),
      allocated: num(snapshot.allocated),
      toolVersion: snapshot.tool_version,
    })),
    unreadable: report.unreadable.map((entry) => ({
      path: entry.path,
      bytes: num(entry.bytes),
      reason: entry.reason,
    })),
    totalBytes: num(report.total_bytes),
    truncated: report.truncated,
    catalogPresent: report.catalog_present,
  };
}

/**
 * Copy one stored snapshot somewhere the user chose.
 *
 * Byte-for-byte, so the original checksum still verifies on a later restore.
 * The backend refuses a source outside the store and refuses to overwrite, and
 * resolves the destination itself — the webview has no business knowing where
 * the user's home is. Empty means Downloads. Returns the path written.
 */
export async function exportSnapshot(source: string, destinationDir = ""): Promise<string> {
  return unwrap("export_snapshot", commands.exportSnapshot(source, destinationDir));
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

export interface SnapshotOfferView {
  readonly mountPoint: string;
  readonly device: number;
  readonly hasSnapshot: boolean;
  /**
   * When the snapshot's scan finished, Unix ms. `null` when there is none.
   *
   * The UI must always show this rather than just offering "restore": a
   * snapshot can be stale by any amount, and a restore that does not say how
   * old it is lets a two-week-old tree pass for the state of the disk.
   */
  readonly takenUnixMs: number | null;
  readonly nodes: number | null;
  readonly bytes: number | null;
}

/**
 * Which volumes could be restored instead of rescanned.
 *
 * Header-and-metadata only — no arena is decoded — so this is safe to call
 * every time a menu opens.
 */
export async function snapshotOffers(): Promise<SnapshotOfferView[]> {
  const offers = await unwrap("snapshot_offers", commands.snapshotOffers());
  return offers.map((offer) => ({
    mountPoint: offer.mount_point,
    device: num(offer.device),
    hasSnapshot: offer.has_snapshot,
    takenUnixMs: offer.taken_unix_ms === null ? null : num(offer.taken_unix_ms),
    nodes: offer.nodes === null ? null : num(offer.nodes),
    bytes: offer.bytes === null ? null : num(offer.bytes),
  }));
}

/**
 * Publish a stored snapshot as the live tree, replacing what is on screen.
 *
 * Errors while a scan is running rather than silently doing nothing —
 * replacing the tree under a scan that is about to publish its own would leave
 * the app showing one volume and finalizing another.
 */
export async function restoreSnapshot(root: string, device: number): Promise<number> {
  const generation = await unwrap("restore_snapshot", commands.restoreSnapshot(root, device));
  return num(generation);
}

// ---------------------------------------------------------------------------
// Breadcrumb
// ---------------------------------------------------------------------------

export interface AncestorRow {
  readonly node: number;
  /** Basename, except the first row, which carries the scan root's full path. */
  readonly name: string;
  readonly kind: Kind;
  readonly logical: number;
  readonly allocated: number;
}

/**
 * The chain from the scan root down to `node`, root first.
 *
 * The breadcrumb is built from this rather than from the navigation history:
 * where you *are* is a property of the tree, not of how you got there.
 */
export async function ancestors(generation: number, node: number): Promise<AncestorRow[]> {
  const rows = await unwrap("ancestors", commands.ancestors(toWireU64(generation), node));
  return rows.map((row) => ({
    node: row.node,
    name: row.name,
    kind: row.kind,
    logical: num(row.logical),
    allocated: num(row.allocated),
  }));
}

// ---------------------------------------------------------------------------
// Relocate
// ---------------------------------------------------------------------------

export type RelocateMode = "migrate" | "repoint";
export type SourceDisposal = "trash" | "delete" | "keep";
export type RiskTier = "ordinary" | "risky" | "blocked";

export interface RelocateWarningView {
  readonly code: string;
  readonly message: string;
}

export interface RelocatePlanView {
  readonly generation: number;
  /** `null` means the relocation cannot proceed; the reasons are in `warnings`. */
  readonly token: string | null;
  readonly node: number;
  readonly source: string;
  readonly destination: string;
  readonly mode: RelocateMode;
  readonly disposal: SourceDisposal;
  readonly logical: number;
  readonly allocated: number;
  readonly retainedNodes: number;
  readonly unreadable: number;
  readonly sourceDevice: number;
  readonly destinationDevice: number;
  readonly destinationAvailable: number;
  /** `apfs`, `exfat`, `smbfs`, … Anything that cannot hold xattrs is refused. */
  readonly destinationFilesystem: string;
  readonly risk: RiskTier;
  readonly warnings: readonly RelocateWarningView[];
}

export interface RelocateReportView {
  readonly generation: number;
  readonly node: number;
  readonly source: string;
  readonly destination: string;
  readonly mode: RelocateMode;
  /** What was actually done, which is not always what was asked. */
  readonly disposal: SourceDisposal;
  readonly filesVerified: number;
  readonly bytesVerified: number;
  readonly specialFiles: number;
  readonly symlinkCreated: boolean;
}

function toRelocatePlan(plan: RelocatePlan): RelocatePlanView {
  return {
    generation: num(plan.generation),
    token: plan.token,
    node: plan.node,
    source: plan.source,
    destination: plan.destination,
    mode: plan.mode,
    disposal: plan.disposal,
    logical: num(plan.logical),
    allocated: num(plan.allocated),
    retainedNodes: num(plan.retained_nodes),
    unreadable: num(plan.unreadable),
    sourceDevice: num(plan.source_device),
    destinationDevice: num(plan.destination_device),
    destinationAvailable: num(plan.destination_available),
    destinationFilesystem: plan.destination_filesystem,
    risk: plan.risk,
    warnings: plan.warnings.map((warning) => ({ code: warning.code, message: warning.message })),
  };
}

/**
 * Describe a relocation without performing it.
 *
 * Always render the result, including when `token` is `null` — an unactionable
 * plan's warnings are the useful part, and a UI that only draws the happy path
 * shows the user nothing at the moment they most need an explanation.
 */
export async function relocatePlan(
  generation: number,
  node: number,
  destination: string,
  mode: RelocateMode,
  disposal: SourceDisposal,
): Promise<RelocatePlanView> {
  const plan = await unwrap(
    "relocate_plan",
    commands.relocatePlan(toWireU64(generation), node, destination, mode, disposal),
  );
  return toRelocatePlan(plan);
}

/**
 * Perform a planned relocation.
 *
 * Long-running: the whole subtree is copied and then read back to verify it.
 * The `token` must be the one `relocatePlan` returned.
 */
export async function relocateApply(
  generation: number,
  node: number,
  destination: string,
  mode: RelocateMode,
  disposal: SourceDisposal,
  token: string,
): Promise<RelocateReportView> {
  const report = await unwrap(
    "relocate_apply",
    commands.relocateApply(toWireU64(generation), node, destination, mode, disposal, token),
  );
  return {
    generation: num(report.generation),
    node: report.node,
    source: report.source,
    destination: report.destination,
    mode: report.mode,
    disposal: report.disposal,
    filesVerified: num(report.files_verified),
    bytesVerified: num(report.bytes_verified),
    specialFiles: num(report.special_files),
    symlinkCreated: report.symlink_created,
  };
}

// ---------------------------------------------------------------------------
// Progress events
// ---------------------------------------------------------------------------

/**
 * Subscribe to `scan:progress`.
 *
 * Two deliberate robustness choices:
 *
 * 1. **Both channels are attached.** The frozen contract pins the event name as
 *    `rdirstat_core::SCAN_PROGRESS_EVENT == "scan:progress"`, and `src-tauri`
 *    holds that line with an explicit
 *    `#[tauri_specta(event_name = "scan:progress")]` on its `ScanProgressEvent`
 *    wrapper. Without that attribute the derive would kebab-case the Rust
 *    identifier into `"scan-progress-event"` instead, and the strip would sit
 *    silent through an entire scan with nothing to debug. Listening on both
 *    names costs one no-op comparison per event at 10 Hz and removes that
 *    failure mode permanently. Duplicate delivery is handled by (2).
 * 2. **`(scanId, sequence)` de-duplication.** The payload carries absolute
 *    counters plus a monotonic sequence precisely so a dropped — or, here,
 *    doubled — event costs nothing. Out-of-order and repeated sequences are
 *    dropped, and a *lower* scan id is ignored outright: a late event from a
 *    cancelled scan must never overwrite the live one's counters.
 *
 * Completion is deliberately **not** inferred here. The contract is explicit
 * that completion is a state transition from the supervisor, not the absence
 * of a progress event.
 */
export function subscribeScanProgress(onProgress: (progress: ScanProgressView) => void): () => void {
  let disposed = false;
  const unlisteners: UnlistenFn[] = [];

  let lastScan = -1;
  let lastSequence = -1;

  const deliver = (raw: unknown) => {
    if (disposed || raw === null || typeof raw !== "object") return;
    // `ScanProgressEvent` is a newtype around `ScanProgress`; serde emits it
    // transparently, but a non-transparent wrapper would nest it once.
    const candidate = "scan_id" in raw ? raw : (raw as { 0?: unknown })[0];
    if (candidate === undefined || candidate === null || typeof candidate !== "object") return;
    if (!("scan_id" in candidate) || !("sequence" in candidate)) return;

    const progress = toProgress(candidate as Parameters<typeof toProgress>[0]);
    if (progress.scanId < lastScan) return;
    if (progress.scanId === lastScan && progress.sequence <= lastSequence) return;
    lastScan = progress.scanId;
    lastSequence = progress.sequence;
    onProgress(progress);
  };

  for (const name of [SCAN_PROGRESS_EVENT, "scan-progress-event"]) {
    void listen(name, (event) => deliver(event.payload)).then((unlisten) => {
      if (disposed) {
        unlisten();
        return;
      }
      unlisteners.push(unlisten);
    });
  }

  return () => {
    disposed = true;
    for (const unlisten of unlisteners) unlisten();
    unlisteners.length = 0;
  };
}

// ---------------------------------------------------------------------------
// Remote destinations and the transfer queue
//
// Two things the local sync does not have to model, and they are why this is
// not just `syncPlan` with a different string:
//
// - **There is no free-space figure for a bucket.** `destinationAvailable` is
//   `null`, not 0, and the UI must render "unknown" rather than "full".
// - **A transfer outlives the panel.** Jobs are fetched as a list and pushed
//   as events; nothing here awaits a completion.
// ---------------------------------------------------------------------------

export type RemoteKind = "s3" | "web_dav" | "sftp";
export type RemoteCompare = "quick" | "verify";
export type RemoteReason = "missing" | "size_differs" | "content_differs";
export type Comparison = "size" | "size_and_digest";
export type JobState = "queued" | "planning" | "running" | "paused" | "done" | "failed" | "cancelled";
export type ProfileField = "bucket" | "root" | "endpoint" | "region" | "user" | "secret";

/** A saved destination, as the editor round-trips it. Never carries a secret. */
export interface RemoteTargetView {
  readonly name: string;
  readonly kind: RemoteKind;
  readonly endpoint: string;
  readonly bucket: string;
  readonly region: string;
  readonly root: string;
  readonly user: string;
}

/** A saved destination plus what the UI needs that is not part of the target. */
export interface RemoteTargetRow extends RemoteTargetView {
  /** True when the Keychain holds something. Never what it holds. */
  readonly hasSecret: boolean;
  /** True for SFTP: authenticates through ssh-agent, so no password field. */
  readonly usesAmbientCredentials: boolean;
}

export interface RemoteProfileView {
  readonly id: string;
  readonly label: string;
  readonly kind: RemoteKind;
  readonly summary: string;
  readonly endpointTemplate: string;
  readonly region: string;
  readonly required: readonly ProfileField[];
}

export interface RemotePlanView {
  /** `null` means there is nothing to upload. */
  readonly token: string | null;
  readonly source: string;
  readonly destination: string;
  readonly compare: RemoteCompare;
  readonly onDiffer: OnDiffer;
  /** What the endpoint could actually prove, whatever was asked for. */
  readonly availableComparison: Comparison;
  readonly entries: readonly { readonly relativePath: string; readonly bytes: number; readonly reason: RemoteReason }[];
  readonly entriesTruncated: boolean;
  readonly totalToCopy: number;
  readonly bytesToCopy: number;
  readonly alreadyPresent: number;
  readonly differingSkipped: number;
  readonly specialSkipped: number;
  /** Files whose names are not valid text and cannot become a remote key. */
  readonly unnameableSkipped: number;
  readonly unreadable: number;
  /**
   * **Always `null`.** A bucket publishes no free-space figure. Render
   * "unknown", never 0 — the local plan's equivalent field is a real number and
   * a shared component must not confuse the two.
   */
  readonly destinationAvailable: null;
  readonly listingTruncated: boolean;
  readonly warnings: readonly { readonly code: string; readonly message: string }[];
}

export interface TransferJobView {
  readonly id: number;
  readonly source: string;
  readonly targetName: string;
  readonly destination: string;
  readonly compare: RemoteCompare;
  readonly onDiffer: OnDiffer;
  readonly state: JobState;
  readonly filesTotal: number;
  readonly bytesTotal: number;
  readonly filesDone: number;
  readonly bytesDone: number;
  readonly failures: readonly { readonly relativePath: string; readonly reason: string }[];
  readonly failuresTruncated: boolean;
  /** Why the job as a whole stopped, when it was not per-file. */
  readonly message: string | null;
  readonly createdUnixMs: number;
  readonly updatedUnixMs: number;
}

/** What the user wants uploaded. Shared by the plan and the enqueue, as in Rust. */
export interface TransferRequestInput {
  readonly source: string;
  readonly target: string;
  readonly compare: RemoteCompare;
  readonly onDiffer: OnDiffer;
}

/**
 * The secret half of a destination edit.
 *
 * `undefined` means *leave what is stored alone*; `""` means *remove it*. That
 * distinction is what lets a user change a bucket's folder without re-typing a
 * key the UI was never shown in the first place.
 */
export interface SecretInputView {
  readonly accessKey?: string;
  readonly secretKey?: string;
  readonly sessionToken?: string;
  readonly password?: string;
  readonly keyPath?: string;
}

/** True when a job is doing something right now. Mirrors `JobState::is_active`. */
export function isActiveJob(state: JobState): boolean {
  return state === "queued" || state === "planning" || state === "running";
}

/** True when the user can still start it. Mirrors `JobState::is_resumable`. */
export function isResumableJob(state: JobState): boolean {
  return state === "paused" || state === "failed";
}

/** The presets the destination editor offers. A constant; cannot fail. */
export async function remoteProfiles(): Promise<readonly RemoteProfileView[]> {
  const profiles = await commands.remoteProfiles();
  return profiles.map((profile) => ({
    id: profile.id,
    label: profile.label,
    kind: profile.kind,
    summary: profile.summary,
    endpointTemplate: profile.endpoint_template,
    region: profile.region,
    required: profile.required,
  }));
}

/** The saved destinations. */
export async function remoteTargets(): Promise<readonly RemoteTargetRow[]> {
  const rows = await unwrap("remote_targets", commands.remoteTargets());
  return rows.map((row) => ({
    name: row.name,
    kind: row.kind,
    endpoint: row.endpoint,
    bucket: row.bucket,
    region: row.region,
    root: row.root,
    user: row.user,
    hasSecret: row.has_secret,
    usesAmbientCredentials: row.uses_ambient_credentials,
  }));
}

/** Adds a destination, or updates the one named by `replacing`. */
export async function remoteSaveTarget(
  target: RemoteTargetView,
  secret: SecretInputView,
  replacing: string | null,
): Promise<RemoteTargetView> {
  const saved = await unwrap(
    "remote_save_target",
    commands.remoteSaveTarget(
      { ...target },
      {
        access_key: secret.accessKey ?? null,
        secret_key: secret.secretKey ?? null,
        session_token: secret.sessionToken ?? null,
        password: secret.password ?? null,
        key_path: secret.keyPath ?? null,
      },
      replacing,
    ),
  );
  return { ...saved };
}

/** Forgets a destination and its secret. Touches nothing at the destination. */
export async function remoteDeleteTarget(name: string): Promise<void> {
  await unwrap("remote_delete_target", commands.remoteDeleteTarget(name));
}

/** Confirms a destination is reachable and its credentials work. */
export async function remoteProbe(name: string): Promise<void> {
  await unwrap("remote_probe", commands.remoteProbe(name));
}

/** What uploading to a destination would do. Uploads nothing. Long-running. */
export async function remotePlan(request: TransferRequestInput): Promise<RemotePlanView> {
  const view = await unwrap("remote_plan", commands.remotePlan(toWireRequest(request)));
  const plan = view.plan;
  return {
    token: view.token,
    source: plan.source,
    destination: plan.destination,
    compare: plan.compare,
    onDiffer: plan.on_differ,
    availableComparison: plan.available_comparison,
    entries: plan.entries.map((entry) => ({
      relativePath: entry.relative_path,
      bytes: num(entry.bytes),
      reason: entry.reason,
    })),
    entriesTruncated: plan.entries_truncated,
    totalToCopy: num(plan.total_to_copy),
    bytesToCopy: num(plan.bytes_to_copy),
    alreadyPresent: num(plan.already_present),
    differingSkipped: num(plan.differing_skipped),
    specialSkipped: num(plan.special_skipped),
    unnameableSkipped: num(plan.unnameable_skipped),
    unreadable: num(plan.unreadable),
    // Not `num(...)`: that would turn the backend's honest "unknown" into 0.
    destinationAvailable: null,
    listingTruncated: plan.listing_truncated,
    warnings: plan.warnings.map((warning) => ({ code: warning.code, message: warning.message })),
  };
}

/** Every transfer, newest first. */
export async function transfers(): Promise<readonly TransferJobView[]> {
  const jobs = await unwrap("transfers", commands.transfers());
  return jobs.map(toTransferJobView);
}

/**
 * Queues an upload and starts it.
 *
 * Returns as soon as the job exists — it does not await the upload. Watch the
 * job's state, or the `transfer:progress` event.
 */
export async function transferEnqueue(
  request: TransferRequestInput,
  token: string,
): Promise<TransferJobView> {
  return toTransferJobView(
    await unwrap("transfer_enqueue", commands.transferEnqueue(toWireRequest(request), token)),
  );
}

/** Stops a transfer where it is. Resumable. */
export async function transferPause(id: number): Promise<TransferJobView> {
  return toTransferJobView(await unwrap("transfer_pause", commands.transferPause(id)));
}

/** Restarts a paused or failed transfer. Re-plans, so it skips what arrived. */
export async function transferResume(id: number): Promise<TransferJobView> {
  return toTransferJobView(await unwrap("transfer_resume", commands.transferResume(id)));
}

/** Stops a transfer for good. What already arrived stays where it is. */
export async function transferCancel(id: number): Promise<TransferJobView> {
  return toTransferJobView(await unwrap("transfer_cancel", commands.transferCancel(id)));
}

/** Removes finished transfers from the list. Running ones are left alone. */
export async function transfersClear(): Promise<number> {
  return num(await unwrap("transfers_clear", commands.transfersClear()));
}

function toWireRequest(request: TransferRequestInput) {
  return {
    source: request.source,
    target: request.target,
    compare: request.compare,
    on_differ: request.onDiffer,
  };
}

/**
 * The wire job as the UI sees it.
 *
 * Exported because the `transfer:progress` event delivers the same wire type
 * outside any command, and the event handler must not grow a second, drifting
 * copy of this mapping.
 */
export function toTransferJobView(job: TransferJob): TransferJobView {
  return {
    id: num(job.id),
    source: job.source,
    targetName: job.target_name,
    destination: job.destination,
    compare: job.compare,
    onDiffer: job.on_differ,
    state: job.state,
    filesTotal: num(job.files_total),
    bytesTotal: num(job.bytes_total),
    filesDone: num(job.files_done),
    bytesDone: num(job.bytes_done),
    failures: job.failures.map((failure) => ({
      relativePath: failure.relative_path,
      reason: failure.reason,
    })),
    failuresTruncated: job.failures_truncated,
    message: job.message,
    createdUnixMs: num(job.created_unix_ms),
    updatedUnixMs: num(job.updated_unix_ms),
  };
}
