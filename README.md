# STELLAR-RDIRSTAT

A native macOS disk-usage and file-inventory app — a Tauri v2 + Rust rewrite of
[QDirStat](https://github.com/shundhammer/qdirstat) built to answer two questions
over volumes with tens of millions of entries:

1. Where is the selected tree's logical and allocated space concentrated?
2. What kinds of files account for it, and what changed between saved scans?

The design volume is a 7.3 TiB APFS disk with **69 million inodes**. A full cold
scan of it should be a coffee break, not an overnight job, and the app must stay
honest when macOS denies access or when APFS makes "bytes attributed to files"
differ from "bytes physically reclaimable."

## Status

**Design complete; phase 0 scaffold in place, no application logic yet.**

Alongside the design contract (`docs/`), the agent skills carried into the
project (`skills/`), and an index of six third-party reference checkouts
(`reference-code/`), the repository now has the phase-0 skeleton: the Cargo
workspace (`Cargo.toml`, `rust-toolchain.toml`, `crates/`), the Tauri v2 desktop
shell (`src-tauri/`), and the React 19 + Vite + Tailwind v4 frontend (`src/`).
Nothing scans a disk yet — the crates are contract stubs and the window renders
the generator's placeholder view. Phase 0 in
[docs/07-BUILD-PHASES.md](docs/07-BUILD-PHASES.md) is the entry point.

```bash
pnpm install        # frontend dependencies (pnpm 10.30.1)
pnpm build          # typecheck + production frontend bundle
cargo tauri dev     # run the desktop shell
```

## Start here

Read [docs/README.md](docs/README.md) — it is the ordered index, and the order is
binding. Later documents may refine an earlier contract but may not silently
contradict it.

| Doc | Binding question |
| --- | --- |
| [00-OVERVIEW.md](docs/00-OVERVIEW.md) | What ships, what does not, and how success is measured |
| [01-ARCHITECTURE.md](docs/01-ARCHITECTURE.md) | Where data lives and how the 69M-entry ceiling is enforced |
| [02-SCANNER.md](docs/02-SCANNER.md) | How a scan stays correct, bounded, cancellable, and comparable |
| [03-MACOS.md](docs/03-MACOS.md) | Which macOS permissions and APFS semantics affect correctness |
| [04-CLASSIFICATION.md](docs/04-CLASSIFICATION.md) | How names and path context become categories |
| [05-UI.md](docs/05-UI.md) | How bounded backend queries become an accessible tree, hierarchy canvas, and report set |
| [06-DATA.md](docs/06-DATA.md) | How completed scans become durable, queryable history |
| [07-BUILD-PHASES.md](docs/07-BUILD-PHASES.md) | In what order the contracts become runnable software |
| [08-RUST-PRACTICES.md](docs/08-RUST-PRACTICES.md) | Lints, error policy, `unsafe` quarantine, allocation and concurrency rules |

Agents should read [AGENTS.md](AGENTS.md) first instead — it carries the
ownership table and the local contracts that constrain edits.

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

These are gates, not estimates — see
[00-OVERVIEW.md](docs/00-OVERVIEW.md#success-criteria) for the full table and the
hardware-profile rules that make a result comparable.


## Repository layout

| Path | Contents |
| --- | --- |
| `docs/` | The design contract — the numbered `00`–`08` series and its index. The only place a decision is binding. |
| `reference-code/` | Index of the six third-party checkouts (clones themselves untracked). |
| `skills/` | Project-carried agent skills, indexed by `skills/AGENTS.md`. |
| `.claude/` | Agent workflow definitions. |
| `skills-lock.json` | Source and integrity lock for installed skills; currently pins `rust-skills`. |

`.agents/` holds the installer-managed `rust-skills` payload. It is downloaded
rather than authored here, so it is untracked; `skills/rust-skills` and
`.claude/skills/rust-skills` are tracked symlinks into it and dangle in a fresh
clone until the skills are installed. Nothing in the build reads them.
| `AGENTS.md` | Root STELLAR contract — ownership, local constraints, work guidance. |

## Building

Nothing to build yet. No `Cargo.toml`, task runner, test suite, or CI exists in
this repository. When phase 0 lands them, this section names the commands that
run them — and not before.
