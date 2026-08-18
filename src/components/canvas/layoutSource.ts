/* =============================================================================
 * The default `layout` transport.
 *
 * docs/01-ARCHITECTURE.md is explicit that `tauri-specta` generates the typed
 * client and that a hand-written wrapper is how the IPC contract drifts. That
 * applies to the JSON commands, and `src/lib/bindings.ts` says so in its own
 * header: "`layout` and `report` are deliberately absent from `commands`. They
 * are registered through a separate `tauri::generate_handler!` because they
 * return `tauri::ipc::Response` (raw bytes), not a serde value. … the caller is
 * expected to `invoke<ArrayBuffer>("layout", …)` directly. That caller is the
 * canvas renderer."
 *
 * This is that caller. It is kept tiny and INJECTABLE — `HierarchyCanvas` takes
 * a `fetchLayout` prop and only falls back here — and it is pinned to the
 * generated types below so an enum rename in Rust is a compile error rather
 * than a runtime deserialisation failure the user sees as a blank panel.
 * ========================================================================== */

import { invoke } from "@tauri-apps/api/core";

import type { LayoutKind as GeneratedLayoutKind, Viewport as GeneratedViewport } from "@/lib/bindings";

import { LayoutError, describeTransportFailure } from "./errors.ts";
import { normalizeGeneration } from "./generation.ts";
import type { LayoutFetcher, LayoutKind, LayoutRequest } from "./types.ts";

/** `true` only when two types are mutually assignable. */
type Exact<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;

/**
 * Compile-time tripwire. If `rdirstat_core::LayoutKind` gains a variant or
 * changes its serde casing, `bindings.ts` is regenerated, this stops being
 * `true`, and `tsc` fails — instead of the canvas sending a string the backend
 * rejects at runtime.
 */
export const LAYOUT_KIND_MATCHES_BINDINGS: Exact<LayoutKind, GeneratedLayoutKind> = true;

/**
 * `Viewport`'s wire shape. No struct in `rdirstat-core` carries
 * `#[serde(rename_all)]`, so its fields are snake_case; the generated type is
 * the authority and this alias makes that explicit at the call site.
 */
type WireViewport = GeneratedViewport;

/** The on-the-wire spelling of a `LayoutKind`. Identity, and asserted so above. */
export function serializeLayoutKind(kind: LayoutKind): GeneratedLayoutKind {
  return kind;
}

function toBinary(value: unknown): ArrayBuffer | Uint8Array | number[] {
  if (value instanceof ArrayBuffer) return value;
  if (value instanceof Uint8Array) return value;
  if (Array.isArray(value) && value.every((entry) => typeof entry === "number")) return value as number[];
  if (ArrayBuffer.isView(value)) {
    const view = value as ArrayBufferView;
    return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  }
  throw new LayoutError(
    "transport_failure",
    "The backend returned a layout response this build cannot read.",
    `expected binary bytes, got ${Object.prototype.toString.call(value)}`,
  );
}

/**
 * `layout` over Tauri IPC. Always rejects with `LayoutError`, so the canvas has
 * exactly one error type to render.
 *
 * `AbortSignal` cannot cancel an in-flight `invoke`; the caller drops the result
 * of a superseded request. Aborting early only saves the round trip.
 */
export const invokeLayout: LayoutFetcher = async (request: LayoutRequest, signal: AbortSignal) => {
  if (signal.aborted) {
    throw signal.reason instanceof Error ? signal.reason : new DOMException("aborted", "AbortError");
  }

  // `bindings.ts` types `TreeGeneration` as `number`, so specta was configured
  // to export `u64` that way. Anything past 2^53 would silently lose precision
  // on the wire, and a generation that quietly becomes a different generation
  // is exactly the failure the stale-batch gate exists to prevent.
  const generationText = normalizeGeneration(request.generation);
  const generation = Number(generationText);
  if (!Number.isSafeInteger(generation)) {
    throw new LayoutError(
      "transport_failure",
      "This scan's generation is too large to send over IPC as a number.",
      `generation ${generationText} exceeds Number.MAX_SAFE_INTEGER`,
    );
  }

  const viewport: WireViewport = {
    width: request.viewport.width,
    height: request.viewport.height,
    device_pixel_ratio: request.viewport.devicePixelRatio,
  };

  try {
    const raw = await invoke("layout", {
      // Tauri v2 renames a command's own arguments to camelCase; nested structs
      // keep their Rust field names, which is why `viewport` above is snake_case
      // while `minPx` here is not.
      generation,
      root: request.root,
      kind: serializeLayoutKind(request.kind),
      viewport,
      minPx: request.minPx,
      // Explicitly null rather than omitted when unfiltered. A Tauri command
      // deserialises its arguments as a struct, and a missing field is not the
      // same thing as a present null.
      categories: request.categories === null ? null : [...request.categories],
      // Same reasoning as `categories`: explicitly null when unset, because the
      // command deserialises its arguments as a struct.
      metric: request.metric ?? null,
    });
    return toBinary(raw);
  } catch (cause) {
    if (cause instanceof LayoutError) throw cause;
    throw new LayoutError("transport_failure", "The layout request failed.", describeTransportFailure(cause), { cause });
  }
};
