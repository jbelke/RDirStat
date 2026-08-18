# STELLAR-RDIRSTAT — Root

## Purpose

Plan and build a native macOS disk-usage and file-inventory app — a Tauri v2 +
Rust rewrite of QDirStat — that answers "where did my disk go, and what kind of
files is it?" over volumes with tens of millions of files.

Phase 0 created the Rust workspace, Tauri shell, React frontend, task runner,
and CI. A manifest, contract type, or template screen is not evidence that the
scanner or product behaviour exists. Cite the exact file for landed code and
label unimplemented behaviour as planned.

The binding design contract lives locally at `.settings/docs/` and is
gitignored. It is not part of the public repository.

## Ownership

| Path | Owns |
| --- | --- |
| `.settings/` | Local developer material, including the binding design contract. Gitignored. Never committed. |
| `skills/` | Five project-carried/installed agent skills, indexed by `skills/AGENTS.md`. |
| `skills-lock.json` | Source and integrity lock for installed skills; currently pins `rust-skills` |
| `.agents/` | Installer-managed canonical payload for `rust-skills`; external skill content, not project source |
| `.claude/` | Installer-managed compatibility link to the canonical `rust-skills` payload |
| `.beads/` | Beads (`bd`) issue database, config, and git hooks. The backlog is tracked here, not in a markdown checklist. |
| `Cargo.toml`, `rust-toolchain.toml` | Workspace members, shared dependency/lint/profile policy, and pinned compiler |
| `crates/` | Rust libraries and CLI, indexed by `crates/AGENTS.md` |
| `src-tauri/` | Tauri v2 application shell and native command boundary |
| `src/` | React + TypeScript presentation |
| `package.json`, `pnpm-lock.yaml` | Frontend commands and exact dependency graph |
| `Justfile`, `scripts/` | Local quality/build commands and repository validators |
| `rush.sh` | Environment-aware entry point for running, packaging, and the container profiles |
| `Dockerfile`, `docker-compose.yml`, `docker/` | Container profiles. They cannot build the app or the `.dmg`; see the Dockerfile header |
| `.env.*.example` | Tracked per-environment templates. The copies without `.example` are git-ignored |
| `src-tauri/tauri.*.conf.json` | Per-profile Tauri config merged over the base config |
| `.github/` | macOS CI workflows |

The project is a git repository on `main`, with a root `.gitignore`. `.agents/`
is excluded as an installed payload, which leaves `skills/rust-skills` and
`.claude/skills/rust-skills` as tracked symlinks that dangle in a fresh clone
until the skills are installed.

## Local Contracts

- **Present is not implemented.** `crates/`, `src-tauri/`, and `src/` exist, but
  a scaffold is not the scanner. Cite the exact file for landed code.
- **69 million inodes on one volume is the design driver.** Every structural
  decision in `.settings/docs/01-ARCHITECTURE.md` — the 48-byte arena node, the
  interned name blob, the single-writer builder, the "node count never appears
  in an IPC payload" rule — exists to survive that number.
- **Never paste third-party copyleft into this repository.** Re-implement
  behaviour from reading and tests. A single copied function can poison the
  commercial license track.
- **Installed skill payloads are a chain boundary.** `skills/rust-skills` and
  `.claude/skills/rust-skills` point at `.agents/skills/rust-skills`. Do not
  edit generated payloads by hand.

## Work Guidance

On a developer machine, read `.settings/docs/README.md` first — it is the
ordered index. A session that changes implementation starts with
`.settings/docs/07-BUILD-PHASES.md` and the matching Beads issue.

When a change alters structure, contracts, ownership, or workflow, close it out
with `$steward-stellar-docs` rather than free-writing an `AGENTS.md` from memory.

## Verification

Run `just check` for the deterministic local gate and `just audit` for dependency
license/advisory reporting. `just ci` runs both. `just check-docs` validates
tracked Markdown links, skill manifests, and rejects known stale claims.

`just check-icons` is part of `just check`. It re-renders every brand asset from
`scripts/generate-icons.mjs` and compares the result against what is committed,
because a reverted icon is a silent failure: nothing about a successful build
reports that the app went back to the Tauri v2 template. Regenerate with
`just icons`, never by editing a PNG and never with `tauri icon`.

The comparison decodes both sides and compares pixels. It cannot compare file
bytes: `deflateSync` output depends on the zlib the running Node was linked
against, so the same artwork compresses differently under Node 24 and Node 25,
and a byte check would fail by accident on half the machines that run it.

Packaging is `./rush.sh dmg` and only runs on macOS. An environment name selects
`.env.<environment>` and `src-tauri/tauri.<profile>.conf.json`; staging carries
its own bundle identifier so it installs beside a release build rather than
sharing its settings and snapshot store.

## Child STELLAR Index

| Child | Covers |
| --- | --- |
| [skills/AGENTS.md](skills/AGENTS.md) | The agent skills carried into this project |
| [crates/AGENTS.md](crates/AGENTS.md) | Rust workspace crates and dependency boundaries |
| [src-tauri/AGENTS.md](src-tauri/AGENTS.md) | Tauri shell and native command boundary |
| [src/AGENTS.md](src/AGENTS.md) | React presentation and frontend boundaries |

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
