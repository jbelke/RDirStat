/* =============================================================================
 * Types the canvas exchanges with the shell.
 *
 * These are LOCAL to `src/components/canvas` on purpose. The generated
 * `tauri-specta` bindings own the command signatures; this file owns the props
 * surface, which is a React concern and must not wait on codegen. Where a name
 * matches a Rust type (`LayoutKind`, `Viewport`) the shape is identical, so a
 * generated value can be passed straight in.
 * ========================================================================== */

/**
 * `rdirstat_core::LayoutKind`. Lower-case because `#[serde(rename_all =
 * "snake_case")]` is the house style for the wire enums; `serializeLayoutKind`
 * in `layoutSource.ts` owns the actual on-the-wire spelling and is the single
 * place to change if the backend disagrees.
 */
export type LayoutKind = "treemap" | "icicle" | "sunburst";

export const LAYOUT_KINDS: readonly LayoutKind[] = ["treemap", "icicle", "sunburst"];

/**
 * `TreeGeneration` is a `u64` newtype. Depending on how `specta` is configured
 * for big integers it arrives as a `number`, a `bigint`, or a decimal string, so
 * every entry point accepts all three and normalises with `normalizeGeneration`.
 */
export type Generation = number | bigint | string;

/** `rdirstat_core::Viewport`. */
export interface Viewport {
  readonly width: number;
  readonly height: number;
  readonly devicePixelRatio: number;
}

/** Arguments of the `layout` command, in frontend spelling. */
export interface LayoutRequest {
  readonly generation: Generation;
  readonly root: number;
  readonly kind: LayoutKind;
  readonly viewport: Viewport;
  readonly minPx: number;
}

/**
 * Fetches the Arrow IPC bytes for one `layout` call. Injectable so the shell can
 * substitute the generated binding, a snapshot fixture, or a test double.
 */
export type LayoutFetcher = (request: LayoutRequest, signal: AbortSignal) => Promise<ArrayBuffer | Uint8Array | number[]>;

/**
 * One decoded `layout` response. The columns are the *typed arrays out of the
 * Arrow buffers* — hit-testing and painting read them directly, never a row
 * object, so neither loop allocates.
 */
export interface LayoutBatch {
  /** Normalised decimal-string generation this batch answers. */
  readonly generation: string;
  readonly protocolVersion: number;
  readonly schemaVersion: number;
  /** The layout kind that was requested; the schema does not carry it. */
  readonly kind: LayoutKind;
  /** The subtree root this batch was laid out for. */
  readonly root: number;
  readonly count: number;
  readonly node: Uint32Array;
  readonly depth: Uint32Array;
  readonly x: Float32Array;
  readonly y: Float32Array;
  readonly w: Float32Array;
  readonly h: Float32Array;
  readonly category: Uint8Array;
}

/** What the shell can tell us about a node id. All fields optional. */
export interface NodeDescription {
  /** Percent-escaped display path (`rdirstat_core::DisplayPath`). Never action authority. */
  readonly path?: string;
  readonly name?: string;
  readonly logical?: number;
  readonly allocated?: number;
  readonly categoryLabel?: string;
}

/**
 * Resolves a node id to human text. Async because `path_of` / `node_details` are
 * IPC commands; results are cached per generation by the canvas.
 */
export type NodeDescriber = (node: number, signal: AbortSignal) => Promise<NodeDescription | undefined>;

/** Why the selection changed. Lets the shell avoid an echo loop. */
export type SelectionSource = "canvas" | "canvas-list" | "context-menu";

export interface SelectionChange {
  /** The full selection after the interaction, in click order. */
  readonly nodes: readonly number[];
  /** The node the user actually acted on, or `null` when the selection cleared. */
  readonly primary: number | null;
  /** True when ⌘/⇧ extended an existing selection rather than replacing it. */
  readonly additive: boolean;
  readonly source: SelectionSource;
}

/** Context-menu verbs. `zoom` is handled internally *and* reported. */
export type CanvasContextAction = "reveal" | "trash" | "copy-path" | "zoom";

export interface ContextActionRequest {
  readonly action: CanvasContextAction;
  readonly nodes: readonly number[];
  readonly primary: number;
}

/** Imperative handle so a breadcrumb or keyboard shortcut can drive navigation. */
export interface HierarchyCanvasHandle {
  /** Pop one level off the navigation stack. No-op at the bottom. */
  back(): void;
  /** Return to the stack's original root. */
  reset(): void;
  /** Push a node onto the navigation stack (the double-click path). */
  zoomTo(node: number): void;
  /** Current navigation stack, oldest first. */
  readonly stack: readonly number[];
  /** Force a repaint. Use after a theme swap the observer could not see. */
  redraw(): void;
  /** Re-issue the `layout` command for the current state. */
  refetch(): void;
}
