# STELLAR-RDIRSTAT — Root

## Purpose

Plan and build a native macOS disk-usage and file-inventory app — a Tauri v2 +
Rust rewrite of [QDirStat](reference-code/qdirstat) — that answers "where did my
disk go, and what kind of files is it?" over volumes with tens of millions of
files.

**There is no application code in this repository yet.** The root holds the
design docs, six third-party reference checkouts, project skills, and the
installer-managed aliases/lock for one external skill. Anyone expecting a Rust
workspace will not find one; `docs/01-ARCHITECTURE.md` describes a decided future
layout, not directories that already exist.

## Ownership

| Path | Owns |
| --- | --- |
| `docs/` | The design contract — the numbered `00`–`08` series and its index. The only place where a decision is binding. |
| `reference-code/` | Six third-party checkouts, read for reference and never adopted: `qdirstat`, `duckdb`, `dirstat-rs`, `parallel-disk-usage`, `squirreldisk`, `rust-skills`. Chain stops here. |
| `skills/` | Five project-carried/installed agent skills, indexed by `skills/AGENTS.md`. |
| `skills-lock.json` | Source and integrity lock for installed skills; currently pins `rust-skills` |
| `.agents/` | Installer-managed canonical payload for `rust-skills`; external skill content, not project source |
| `.claude/` | Installer-managed compatibility link to the canonical `rust-skills` payload |

No application workspace exists at root — no `Cargo.toml`, no `src-tauri/`, no
`src/`, no build or CI configuration, no `.gitignore`, and no git repository.
The six
checkouts under `reference-code/` carry their own `.git` directories; the
project itself is currently an untracked working directory.

## Local Contracts

- **The docs describe a future tree; do not cite it as present.** `crates/`,
  `src-tauri/`, and `src/` appear in `docs/01-ARCHITECTURE.md` as the planned
  layout. A claim that a file under them exists is false today and will be
  caught by any `grep`. Write "planned" until the path is real.
- **`reference-code/` is indexed, never adopted.** Its code is read, quoted, and
  ported deliberately; it is not built, not vendored, and not edited. See
  `reference-code/AGENTS.md` for the one sanctioned exception.
- **69 million inodes on one volume is the design driver.** Every structural
  decision in `docs/01-ARCHITECTURE.md` — the 48-byte arena node, the interned
  name blob, the single-writer builder, the "node count never appears in an IPC
  payload" rule — exists to survive that number. A proposal that is comfortable
  at 2M files and unbounded at 69M has not cleared the bar.
- **Licence asymmetry is load-bearing.** The six checkouts carry four different
  licences and they are not interchangeable. `squirreldisk/` is **AGPL-3.0** and
  is a direct Tauri + React peer — its code would drop straight in, and doing so
  attempts to relicense this project; take design decisions from it and
  implement them from scratch. `qdirstat/` is GPL-2.0: port *behaviour* you
  re-implement from reading, never source text. `parallel-disk-usage/` is
  Apache-2.0; `duckdb/`, `dirstat-rs/`, and `rust-skills/` are MIT. The table is in
  `reference-code/AGENTS.md`; check it before you copy anything.
- **Installed skill payloads are a chain boundary.** `skills/rust-skills` and
  `.claude/skills/rust-skills` point at `.agents/skills/rust-skills`. Its bundled
  `AGENTS.md` is upstream skill content, not a child STELLAR contract. Do not edit
  generated payloads by hand; update through the installer and its lock.

## Work Guidance

Read `docs/README.md` first — it is the ordered index, and the order is real.
`00-OVERVIEW.md` fixes scope, `01-ARCHITECTURE.md` fixes structure, and the rest
depend on both. A session that intends to start the build goes to
`docs/07-BUILD-PHASES.md`, which carries phases 0–7 with acceptance gates and an
ordering rule that is binding on the other docs.

When a change alters structure, contracts, ownership, or workflow, close it out
with `$steward-stellar-docs` rather than free-writing an `AGENTS.md` from memory.

## Verification

None — no build system, test suite, linter, or gate exists in this repository
yet. When a `Cargo.toml` and a task runner land, this section names the command
that runs them, and not before.

## Child STELLAR Index

| Child | Covers |
| --- | --- |
| [docs/AGENTS.md](docs/AGENTS.md) | The design and build-plan series |
| [reference-code/AGENTS.md](reference-code/AGENTS.md) | The reference-checkout index, and the chain boundary |
| [skills/AGENTS.md](skills/AGENTS.md) | The agent skills carried into this project |

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:7510c1e2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
