---
name: issue-tracking
description: Beads (`bd`) issue tracking for a project that carries a `.beads/` directory — finding ready work, filing tasks and epics with real dependencies, claiming and closing them, and landing a session so no work is stranded locally. Use when the user says "bd", "beads", "what's next", "add task", or "add epic", or asks what is in flight and what to pick up next. Do not use in a project with no `.beads/` directory: the skill does not apply there and every command answers "no beads database found".
---

# Beads Issue Tracking

Workflow for projects that track work in beads. Every command below is verified
against `bd version 1.0.4`; the ones that fail outright are listed under
[Command Traps](#command-traps) rather than silently omitted.

## Activation

**Only apply these instructions when a `.beads/` directory exists in the project
root.** If it does not, this skill does not apply — and do not run `bd init` to
make it apply. When a subdirectory or worktree makes the binding unclear, `bd
where` prints the resolved workspace and its issue prefix.

## Session Start

Run `bd prime` for current workflow context and the command reference.

`bd init` installs `SessionStart` and `PreCompact` hooks that call `bd prime`
automatically. If the session already opened with beads context, it has already
run — do not run it a second time. Do re-run it by hand after a compaction that
dropped it.

`bd prime` also promotes `bd remember` / `bd memories` for cross-session
knowledge. This project already carries that through claude-mem; record an
insight in one store, not both.

## Checking What's Next

"What's next?" takes two commands, because **`bd ready` deliberately excludes
in-progress and blocked issues** — it answers "what can I claim", not "what is in
flight":

```bash
bd list --status in_progress   # in flight
bd ready                       # claimable: open, unblocked, not deferred
bd blocked                     # optional: what is waiting, and on what
```

Propose the top items without asking for confirmation:

```
In-progress tasks:
- <id>: <oneliner>

Ready tasks:
- <id>: <oneliner>
```

Tree output is already the default for both commands; passing `--pretty` changes
nothing.

## Creating Issues

Treat "add task" and "add epic" as instructions to run `bd create` with the
matching `--type`.

**Let bd assign the ID.** IDs derive from the database prefix (`nato-a3f2dd`),
and a child inherits its parent's stem (`nato-lag` → `nato-lag.1`,
`nato-lag.2`) — the `<area>-<epic>-<task>` shape by construction, without
hand-picked IDs that collide. An explicit `--id` whose first segment is not the
database prefix is rejected outright:

```
Error: prefix mismatch: database uses 'nato-' but ID 'infra-blueprint-planonly'
doesn't match (use --force to override)
```

`--force` overrides that, at the price of an ID that no longer sorts, filters, or
routes with the rest of the database. Reach for `--id` only to reserve a
well-known ID *within* the project prefix. Carry the domain area in a label
instead, where `bd list --label` can actually use it.

Priority is `0`–`4` or `P0`–`P4` (0 = highest), never `high`/`medium`/`low` —
words are rejected.

```bash
# A task (--type task is the default)
bd create "Allow blueprints to run in plan-only mode" -p 1 --labels infra \
  --description "Why this issue exists and what done looks like"

# An epic
bd create "Expose Terraform errors to conditions" --type epic --labels infra

# A task under that epic — inherits the parent's ID stem and labels
bd create "Phase 1: enable log subresource" --parent nato-tferrors

# Quick capture: creates the issue, prints only the ID
bd q "Fix flaky scanner test"
```

## Dependencies

Dependencies are what beads is for: an issue with an open blocker stays out of
`bd ready` until that blocker closes.

```bash
bd dep add <issue> <depends-on>   # <issue> is blocked by <depends-on>
bd blocked                        # everything currently waiting
bd show <id>                      # what this blocks, and what blocks it
```

Prefer a dependency over a note that says "do X first". A note does not gate `bd
ready`, so the next agent claims the work out of order.

## Task State Management

Claim before the first edit, not after — an unclaimed issue is claimable by a
parallel agent.

```bash
bd update <id> --claim              # atomic: assignee = you, status = in_progress
bd close <id1> <id2> ...            # close several at once
bd close <id> --reason "..." --suggest-next
```

**Always put the MR/PR URL on the task as a label**, and find it again with `bd
list --label "<URL>"`. Set `--external-ref` as well — it is the semantic field,
rendered by `bd show` as `External:` — but the label is what makes the task
findable, because `bd search` does not index external refs.

```bash
bd tag <id> "https://github.com/<org>/<repo>/pull/42"
bd update <id> --external-ref "https://github.com/<org>/<repo>/pull/42"
bd list --label "https://github.com/<org>/<repo>/pull/42"
```

## Epics

```bash
bd epic status          # per-epic child completion
bd children <epic-id>   # the tree
bd epic close-eligible  # epics whose children have all closed
```

**Epic work happens in a detached worktree, and never on a new branch.** Do not
use `bd worktree create`: it creates a *named branch* (`Branch: <name>`) and it
slips past the global no-branch guard, which only matches the literal `git`
token. It is the one bd command this project does not use. The sanctioned form:

```bash
git worktree add --detach /tmp/mw/<task> HEAD
```

A detached worktree resolves the same beads database as the main checkout through
git's common directory — an issue filed there is visible everywhere immediately,
with no redirect file and no `.gitignore` entry to maintain. Stay in that
worktree for the life of the epic; leave it only for another epic or a task that
already has one, and never touch the main checkout unless the user asks.

Land it, then remove it:

```bash
git fetch origin && git rebase origin/main
git push origin HEAD:main
git worktree remove /tmp/mw/<task>
```

## Landing the Plane (Session Completion)

**When ending a work session**, complete every step below. Work is NOT complete
until `git push` succeeds.

1. **File issues for remaining work** — everything discovered and not finished
2. **Run quality gates** (if code changed) — tests, linters, builds
3. **Update issue status** — close what is genuinely done (see the merge rule
   below); leave the rest claimed and in progress
4. **Label** — PR/MR URL onto the task *before* pushing, so the record survives a
   push that fails
5. **Push** — mandatory:
   ```bash
   git add -- <explicit paths>   # never `git add -A` in a shared tree
   git commit -s -m "..."
   git pull --rebase
   git push                      # detached worktree: git push origin HEAD:main
   ```
   There is no `bd sync` step — `bd sync` is not a command in bd 1.0.4. Issues
   live in a local Dolt database, ride the git remote on `refs/dolt/data`, and
   are carried by bd's own git hooks; `.beads/issues.jsonl` is a passive export,
   not the system of record. If a push seems to carry no issue changes, check the
   hooks with `bd hooks list`.
6. **Clean up** — clear stashes, prune remote branches, remove a finished epic's
   worktree
7. **Verify the push landed** — on a tracking branch, `git rev-list --count
   @{u}..HEAD` must print `0`; from a detached worktree, `git fetch origin && git
   branch -r --contains HEAD` must list `origin/main`. Do **not** read `git
   status` for the phrase "up to date with origin": contextzip reformats that
   command to short `-sb` output, where the phrase never appears.
8. **Hand off** — `bd comment <id> "HANDOFF: ..."` telling the next agent what to
   do (check MR status, address feedback, close the task)

### Critical Rules

- Work is NOT complete until `git push` succeeds. Never stop before pushing —
  that strands the work locally. Never say "ready to push when you are."
- If the push is rejected, `git pull --rebase` and retry until it succeeds.
- **Only close a task once its MR is merged.** If that is uncertain, defer it
  rather than closing it: `bd defer <id> --until "+1d"`.
- **Always leave a handoff comment before deferring** — `bd comment <id>
  "HANDOFF: ..."` — not after.
- Never create a branch. `git worktree add --detach` is the only isolation this
  project sanctions; if that is genuinely not enough, stop and ask the user.
- Never leave the worktree unless asked. The only exception is switching to
  another epic, or to a task that already has a worktree.
- Track work in beads, not in `TodoWrite` or a markdown checklist — a parallel
  agent can see a bead and cannot see either of those.

## Command Traps

| Command | What actually happens | Use instead |
| --- | --- | --- |
| `bd sync` | `unknown command "sync" for "bd"` | nothing — git hooks carry the data |
| `bd edit <id>` | opens `$EDITOR` and blocks the agent until killed | `bd update <id> --notes/--description/...` |
| `bd doctor` | `not yet supported in embedded mode` (the default backend) | `bd where`, `bd info`, `bd hooks list` |
| `bd create --id <other-prefix>-...` | prefix-mismatch error | let bd generate the ID |
| `bd ready` to see what is in flight | excludes in-progress and blocked issues | `bd list --status in_progress` |
| `bd worktree create <name>` | creates a named branch; the no-branch guard does not catch it | `git worktree add --detach <path> HEAD` |
| `bd create -p high` | `invalid priority "high"` | `0`–`4` or `P0`–`P4` |
| `bd preflight` | prints beads' own Go/Nix checklist, not this project's | this project's gates |

## Verification

No repository linter checks this skill; the gate is the sequence below, and it
should hold at the end of any session that touched issues.

```bash
bd where                          # resolves a workspace → the skill applies here
bd ready                          # exits 0 and lists claimable work
bd hooks list                     # beads git hooks installed → sync path is live
bd list --status in_progress      # no issue left claimed but abandoned
git rev-list --count @{u}..HEAD   # 0 → nothing stranded locally
```
