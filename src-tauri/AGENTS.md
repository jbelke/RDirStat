# src-tauri/ — Tauri application shell

## Purpose

The desktop composition and native macOS boundary. This crate owns application
state, typed commands/events, permission-facing operations, and bundling. It
adapts library APIs; it does not own scan algorithms or arena structures.

## Ownership

| Path | Owns |
| --- | --- |
| `Cargo.toml`, `build.rs` | Tauri-only dependencies and build integration |
| `tauri.conf.json` | Stable bundle identity, window policy, minimum macOS version, and packaging metadata |
| `capabilities/` | Tauri capability grants; these are not macOS privacy authorization |
| `src/` | Command/event registration, application state, native actions, and serializable error boundary |
| `icons/` | Bundle icon assets |

## Local Contracts

- Blocking scan/query work never runs on the async command executor.
- Commands accept IDs and validated options, not JavaScript-provided paths as
  filesystem authority. Reveal/Trash reconstruct and revalidate identity.
- Small DTOs are generated/checked from Rust types; bulk tables and tiles use
  versioned Arrow IPC. Every response remains bounded.
- Tauri scopes and macOS consent are distinct. Do not claim Full Disk Access or
  bookmark authority from a capability entry.
- Product logic belongs in `crates/`; keep this crate a thin adapter.
- Do not enable private macOS APIs or App Sandbox without updating
  `.settings/docs/03-MACOS.md` and its distribution decision.

## Work Guidance

The relevant contracts are `.settings/docs/01-ARCHITECTURE.md` (state/IPC),
`.settings/docs/03-MACOS.md` (permissions and actions),
`.settings/docs/05-UI.md` (consumer contract), and
`.settings/docs/08-RUST-PRACTICES.md` (typed errors and blocking work).

## Verification

Run `just lint`, `just test`, and `just build`. Use `just dev` for the manual
window and macOS integration check.

## Child STELLAR Index

None. Parent: [`../AGENTS.md`](../AGENTS.md).
