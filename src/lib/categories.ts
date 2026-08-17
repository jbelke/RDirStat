/**
 * Category index -> key -> CSS custom property.
 *
 * docs/05-UI.md is explicit that **Rust sends category indices and the frontend
 * resolves colours**, so this module is the only place in the app that knows a
 * `CategoryId` means anything, and `src/styles/categories.css` is the only
 * place that knows what colour it is. No RGB literal crosses IPC and no colour
 * is hard-coded in a component.
 *
 * ---------------------------------------------------------------------------
 * The index assignment below is VERIFIED against the compiled table
 * ---------------------------------------------------------------------------
 * It was not always. The order was originally transcribed from
 * docs/04-CLASSIFICATION.md's "Initial taxonomy" table while
 * `rdirstat-classify` was still a 4-line placeholder, and it stopped at 18 —
 * so the six macOS additions the real table appends (ids 19..24: package,
 * media-library, build-junk, cache, font, database) fell through to
 * `--cat-unknown`, and a `node_modules` tree painted the same grey as a
 * version skew.
 *
 * The list is now checked against `crates/rdirstat-classify/src/defaults.rs`
 * by `categories.test.ts`, which parses the `CategorySpec::new(...)` calls out
 * of that file in source order and asserts key-for-key equality. That test is
 * the thing keeping the two in step; if you append to the Rust table, it fails
 * until you append here too.
 *
 * The failure mode if it drifts is mislabelled chips and wrong colours — never
 * a wrong number, since nothing downstream computes with a category. The
 * stronger fix remains a `categories()` command returning the compiled table
 * with its `ConfigHash`, so the UI stops mirroring a list at all.
 *
 * Until then: `categoryOf()` never throws on an unknown index. It returns a
 * synthesised entry pointing at `--cat-unknown`, which is visually distinct
 * from `Uncategorized`, so a version skew shows up on screen instead of
 * silently colouring VM disks as audio.
 */

/** Stable key, used to build the CSS custom property name. */
export type CategoryKey =
  | "uncategorized"
  | "symlink"
  | "executable"
  | "apple-metadata"
  | "compressed-archive"
  | "uncompressed-archive"
  | "compressed-stream"
  | "disk-image"
  | "image"
  | "raw-photo"
  | "uncompressed-image"
  | "video"
  | "audio"
  | "document"
  | "source"
  | "object-generated"
  | "library"
  | "vm-disk"
  | "container-disk"
  // macOS additions (id >= 19). Append-only, in agreement with the
  // `// ===== macOS additions (id >= 19) =====` block of
  // `crates/rdirstat-classify/src/defaults.rs`.
  | "package"
  | "media-library"
  | "build-junk"
  | "cache"
  | "font"
  | "database"
  | "unknown";

/** The family headings from docs/04, used to group the legend. */
export type CategoryFamily = "System" | "Archives" | "Media" | "Documents and code" | "Large runtime data";

export interface Category {
  /** The `u8` the backend puts in `Node.category`. */
  readonly id: number;
  readonly key: CategoryKey;
  readonly label: string;
  readonly family: CategoryFamily;
}

/**
 * Index-ordered. Position in this array **is** the `CategoryId`; do not sort it,
 * do not insert in the middle, and append only in agreement with the compiled
 * table in `rdirstat-classify`.
 */
export const CATEGORIES: readonly Category[] = [
  { id: 0, key: "uncategorized", label: "Uncategorized", family: "System" },
  { id: 1, key: "symlink", label: "Symlink", family: "System" },
  { id: 2, key: "executable", label: "Executable", family: "System" },
  { id: 3, key: "apple-metadata", label: "Apple Metadata", family: "System" },
  { id: 4, key: "compressed-archive", label: "Compressed Archive", family: "Archives" },
  { id: 5, key: "uncompressed-archive", label: "Uncompressed Archive", family: "Archives" },
  { id: 6, key: "compressed-stream", label: "Compressed Stream", family: "Archives" },
  { id: 7, key: "disk-image", label: "Disk Image / Installer", family: "Archives" },
  { id: 8, key: "image", label: "Image", family: "Media" },
  { id: 9, key: "raw-photo", label: "RAW Photo", family: "Media" },
  { id: 10, key: "uncompressed-image", label: "Uncompressed Image", family: "Media" },
  { id: 11, key: "video", label: "Video", family: "Media" },
  { id: 12, key: "audio", label: "Audio", family: "Media" },
  { id: 13, key: "document", label: "Document", family: "Documents and code" },
  { id: 14, key: "source", label: "Source", family: "Documents and code" },
  { id: 15, key: "object-generated", label: "Object / Generated", family: "Documents and code" },
  { id: 16, key: "library", label: "Library", family: "Documents and code" },
  { id: 17, key: "vm-disk", label: "Virtual Machine Disk", family: "Large runtime data" },
  { id: 18, key: "container-disk", label: "Container Disk", family: "Large runtime data" },
  // --- macOS additions, ids 19..24 -----------------------------------------
  // These exist in the compiled table and were missing here, which rendered
  // Caches, DerivedData and node_modules as an unlabelled "Category 22" in the
  // `--cat-unknown` grey — the single most useful thing to see on a disk that
  // is running out of room. Families reuse the existing five headings on
  // purpose so that colouring by family stays down to five hues.
  { id: 19, key: "package", label: "macOS Bundle", family: "System" },
  { id: 20, key: "media-library", label: "Photo & Media Library", family: "Media" },
  { id: 21, key: "build-junk", label: "Xcode & Build Junk", family: "Documents and code" },
  { id: 22, key: "cache", label: "Cache", family: "Large runtime data" },
  { id: 23, key: "font", label: "Font", family: "Documents and code" },
  { id: 24, key: "database", label: "Database", family: "Large runtime data" },
];

/**
 * The five family headings, in the order docs/04 lists them.
 *
 * Twenty-five categories is more hues than a treemap can carry: past roughly a
 * dozen, adjacent tiles stop being tellable apart and the colour stops being
 * information. Colouring by family instead gives five well-separated hues, and
 * is the default the shell offers for a whole-volume scan. The per-category
 * palette stays available for a drilled-in subtree, where the category count
 * actually on screen is small.
 */
export const FAMILIES: readonly CategoryFamily[] = [
  "System",
  "Archives",
  "Media",
  "Documents and code",
  "Large runtime data",
];

/** Stable key for a family, used to build `--fam-<key>`. */
export function familyKey(family: CategoryFamily): string {
  return family.toLowerCase().replace(/ /g, "-");
}

const UNKNOWN_FAMILY: CategoryFamily = "System";

/** Never throws. An index this build does not know becomes an explicit "unknown". */
export function categoryOf(id: number): Category {
  const entry = CATEGORIES[id];
  if (entry !== undefined) return entry;
  return { id, key: "unknown", label: `Category ${id}`, family: UNKNOWN_FAMILY };
}

/**
 * The CSS colour reference for a category, suitable for a `style` prop or a
 * canvas `fillStyle` read via `getComputedStyle`.
 *
 * Returned as `var(--cat-key)` rather than a resolved colour on purpose: the
 * value then follows the light/dark switch with no JS involvement.
 */
export function categoryColorVar(id: number): string {
  return `var(--cat-${categoryOf(id).key})`;
}

/**
 * Resolve a category to a concrete colour string. Only for `<canvas>`, which
 * cannot consume `var()`. Reads from the document element so it picks up the
 * active theme; callers must re-read on a theme change.
 */
export function resolveCategoryColor(id: number, element?: Element): string {
  const target = element ?? document.documentElement;
  const value = getComputedStyle(target).getPropertyValue(`--cat-${categoryOf(id).key}`).trim();
  return value.length > 0 ? value : "oklch(0.5 0.02 300)";
}
