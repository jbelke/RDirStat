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
  type Sort,
  type SortDirection,
  type SortKey,
} from "@/lib/bindings";
import { MAX_CHILD_PAGE, num, SCAN_PROGRESS_EVENT, unwrap, type NumLike } from "@/lib/wire";

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
