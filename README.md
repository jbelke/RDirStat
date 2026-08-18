# RDirStat

**STELLAR-RDIRSTAT** — a native macOS disk-usage and file-inventory app. A
Tauri v2 + Rust rewrite of [QDirStat](https://github.com/shundhammer/qdirstat)
built to answer two questions over volumes with tens of millions of entries:

1. Where is the selected tree's logical and allocated space concentrated?
2. What kinds of files account for it, and what changed between saved scans?

The design volume is a 7.3 TiB APFS disk with **69 million inodes**. A full cold
scan of it should be a coffee break, not an overnight job, and the app must stay
honest when macOS denies access or when APFS makes "bytes attributed to files"
differ from "bytes physically reclaimable."

[![CI](https://github.com/jbelke/RDirStat/actions/workflows/ci.yml/badge.svg)](https://github.com/jbelke/RDirStat/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)

## Status

In development. The Cargo workspace, Tauri v2 desktop shell, and React 19 +
Vite + Tailwind v4 frontend are in place. The detailed design contract and
upstream reference checkouts are **local developer material** under `.settings/`
and are not part of this repository.

## Requirements

- macOS 14+
- Rust 1.90 (MSRV); the pinned toolchain is in `rust-toolchain.toml` (1.97.1)
- Node.js ≥ 22.12
- pnpm 10.30.1 (`packageManager` in `package.json`)

## Quick start

```bash
just bootstrap     # pnpm install --frozen-lockfile
just check         # formatting, lints, tests, frontend build
just dev           # Tauri development app
```

`just ci` adds dependency license/advisory reports (`cargo-deny`, `pnpm audit`).
GitHub Actions runs it on macOS for pushes to `main` and pull requests.

## Stack

| Layer | Choice |
| --- | --- |
| Backend | Rust workspace, arena tree (48-byte node + interned name blob), single-writer builder |
| Desktop shell | Tauri v2 |
| Reporting store | Parquet + DuckDB `v1.5.5` |
| IPC | Arrow IPC bytes for bulk data; JSON only for small DTOs |
| Typed client | `tauri-specta` generated bindings, verified by test |
| Frontend | React 19 + TypeScript + Vite |
| Styling | Tailwind CSS v4 (`@theme`) + shadcn/ui |
| Tables | TanStack Table v8 + TanStack Virtual v3 |
| Charts | shadcn `Chart` / Recharts for reports; hand-written canvas for treemap, icicle, and sunburst |

## Acceptance targets

| Measure | Target |
| --- | --- |
| Cold full-detail CLI scan, ~69M entries | < 12 min, < 5.0 GiB peak RSS |
| Warm scan, same root | < 4 min |
| ~2M-entry local fixture | < 25 s cold |
| Correctness vs `du` on a quiescent fixture | allocated within 1%; exact logical total and entry/error counts |
| Treemap navigation | p95 input-to-paint < 50 ms |
| Cancel | UI acknowledges < 100 ms; workers stop p95 < 200 ms |

These are gates, not estimates.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/` | Rust libraries and the supported diagnostic CLI |
| `src-tauri/` | Tauri v2 desktop shell and native command boundary |
| `src/` | React + TypeScript frontend |
| `skills/` | Project-carried agent skills |
| `Justfile`, `scripts/` | Local task surface and repository validators |
| `.github/` | macOS CI |
| `LICENSE`, `NOTICE`, `LICENSING.md` | AGPL-3.0-only text, attribution, dual-licensing policy |

`.agents/` holds the installer-managed `rust-skills` payload. It is downloaded
rather than authored here, so it is untracked; `skills/rust-skills` and
`.claude/skills/rust-skills` are tracked symlinks into it and dangle in a fresh
clone until the skills are installed. Nothing in the build reads them.

`.settings/` is gitignored local developer material: the numbered design
contract (`.settings/docs/`) and upstream reference checkouts
(`.settings/reference-code/`). They are never cloned with this repository.

## License

RDirStat (STELLAR-RDIRSTAT) is dual-licensed.

- **Open source:** [GNU AGPL-3.0-only](LICENSE). Free to use, modify, and
  redistribute, provided derivatives stay AGPL-3.0-only, the attribution in
  [NOTICE](NOTICE) survives, and users you serve over a network can get the
  complete source.
- **Commercial:** for closed-source or proprietary products, hosted services that
  do not publish source, or relaxed attribution, buy a commercial license.
  Contact **Joshua Belke — joshbelke@gmail.com** ([@jbelke](https://github.com/jbelke)).

[LICENSING.md](LICENSING.md) explains both tracks, the contributor CLA, and why
third-party copyleft code is never pasted into this repository.

```
SPDX-License-Identifier: AGPL-3.0-only
Copyright (C) 2026 Joshua Belke
```
