# crates/ — Rust workspace libraries and CLI

## Purpose

Project-owned Rust code below the Tauri boundary. The arena and wire contracts
already live in `rdirstat-core`; the scanner, classifier, treemap, and CLI are
still phase-owned implementation surfaces. Nothing here may depend on Tauri.

## Ownership

| Path | Owns |
| --- | --- |
| `rdirstat-core/` | Packed identifiers, exact node/name representation, immutable arena, directory totals, scan facts, and bounded wire DTOs |
| `rdirstat-scan/` | Directory readers, traversal scheduling, single-writer builder integration, cancellation, errors, and progress |
| `rdirstat-classify/` | Clean-room byte-oriented categories and contextual tags |
| `rdirstat-treemap/` | Treemap/icicle/sunburst geometry and bounded Arrow tile output |
| `rdirstat-cli/` | Supported scanner diagnostics, correctness fixtures, profiling, and import/export commands |
| `rdirstat-catalog/` | Future phase-6 Parquet + DuckDB boundary; the directory does not exist until that phase |

## Local Contracts

- Every manifest inherits workspace package fields, dependencies, and lints.
- Safe library crates forbid `unsafe`; only the audited future
  `rdirstat-scan::sys::bulk` module may lower the workspace deny lint.
- `rdirstat-core` has no filesystem I/O. The scan path does not depend on the
  optional catalog or bundled DuckDB.
- Filesystem identity remains bytes/components, not lossy `String` paths.
- No command/report returns a node-count-sized payload. Page and tile limits are
  enforced in Rust and covered by tests.
- Keep the 48-byte `Node` and related static size assertions intact. A field or
  side table that scales with nodes changes the architecture memory table first.

## Work Guidance

Follow `.settings/docs/07-BUILD-PHASES.md` in order and the crate-specific
contracts in `.settings/docs/01-ARCHITECTURE.md`, `02-SCANNER.md`,
`04-CLASSIFICATION.md`, and `08-RUST-PRACTICES.md`. General `rust-skills`
guidance is subordinate to those project documents.

## Verification

Run `just fmt`, `just lint`, and `just test` from the repository root.

## Child STELLAR Index

None yet. Add a child only when a crate needs ownership finer than the table
above. Parent: [`../AGENTS.md`](../AGENTS.md).
