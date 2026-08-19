# Changelog

All notable changes to Stellar RDIRSTAT are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries say what a user can now do, and — just as deliberately — what a release
still refuses to do. A changelog that only lists wins is a marketing document.

## [0.1.0] — 2026-08-18

First release. A native macOS disk-usage and file-inventory app: a Tauri v2 +
Rust rewrite of QDirStat, built for volumes with tens of millions of entries.

### Added

**Scanning**

- Parallel scanner over an arena tree (48-byte node, interned name blob,
  single-writer builder), with cancel, progress counters and a coverage bar.
- Several folders scanned at once, each with its own tree, switchable from the
  UI.
- Per-path failures are recorded and the scan continues: a partial scan is a
  success with a payload, never an error. Unreadable directories, exclusions,
  mutations and tree-versus-volume discrepancies surface as dismissible chips,
  and Full Disk Access guidance appears only when the recorded error classes
  actually support it.
- Drive picker: select a volume, scan it from its row, and switch drives from
  the title bar mid-scan.
- `*.rdstat` snapshots — save a scan, restore it at launch, or restore a drive
  from its snapshot instead of rescanning it. The snapshot store's location is
  configurable and can be moved off the boot volume.

**Views**

- Virtualized tree table with cursor-paged children, breadcrumbs built from the
  ancestor chain, and on-demand details.
- Hand-written canvas treemap, icicle and sunburst, with a depth cap and a
  minimum tile size tuned so a large scan renders as readable tiles rather than
  mush.
- Category legend and colour-by switch; clicking a swatch filters the tree and
  the treemap to that category and re-proportions the layout.
- Size bands (>50 GB, 5 GB, 500 MB, 50 MB, 5 MB) in both unit systems, each
  expandable to the files inside it.
- Reports: Types, Ages, Diff and Dupes, all running on the live scanned tree.
  Duplicates are confirmed by reading the bytes, with SHA-256.
- A details panel that slides in and pins open, giving a digest for a file and
  a shape for a directory.
- Menu-bar tray panel — a second webview onto the same frontend and the same
  commands, so it cannot drift into disagreeing with the main window.

**Moving data**

- Folder sync: compare two folders side by side and copy either way, with a
  destination pane and scheduled syncs that run unattended.
- Remote destinations over S3, WebDAV and SFTP, with credentials in the
  Keychain and a transfer queue that outlives the window.
- Subtree relocation, and multi-select with bulk actions across the lists.

**Packaging**

- `./rush.sh` — one entry point for run, build, package and containers. An
  environment name resolves to exactly two files, and `development`, `staging`
  and `production` carry separate bundle identifiers so they install beside
  each other instead of sharing settings, snapshot store and Keychain items.
- Every brand asset is generated from one dependency-free source file and is
  byte-reproducible, so `just check` can assert the committed icons have not
  silently reverted to the Tauri template.
- A branded, click-through-licensed `.dmg`.

### Known limitations

These are deliberate, and each is tracked:

- **The disk image is unsigned** and built for the host architecture only. It
  opens on the machine that built it; anywhere else Gatekeeper reports it as
  "damaged", which is a signing failure wearing a corruption message. See
  [Installing the v0.1.0 image](README.md#installing-the-v010-image).
- **Move to Trash refuses.** The control explains itself rather than acting: it
  needs a confirmation step showing exactly what would move, and it will not
  move anything without showing that first.
- **No Parquet/DuckDB catalog.** Reports run on the live in-memory tree; there
  is no durable query store in this build.
- **Sync cannot be cancelled** once applied.
- **The tree table is not fully keyboard-operable.**
- **The performance targets in the README are not enforced by CI.** They are
  measured by hand on a quiescent host and should not be read as verified
  properties of this release.

### Fixed

Selected correctness work that predates the release and would otherwise be
invisible:

- Snapshot checksums are independent of how the byte stream is chunked. An
  earlier version folded a per-call length and zero-padded a ragged tail, which
  made every snapshot over 8192 nodes fail its own checksum — invisible because
  every fixture was about eleven nodes.
- The treemap walk pushes children in reverse so the LIFO stack pops
  largest-first; it was spending its tile budget on the least significant
  subtrees.
- Icons no longer ship a dark fringe on every antialiased edge: the canvas
  accumulates premultiplied alpha and PNG stores straight alpha, so the encoder
  divides it back out.
- A render-phase throw no longer leaves a grey window with no way out.

[0.1.0]: https://github.com/jbelke/RDirStat/releases/tag/v0.1.0
