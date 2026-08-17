# skills/issue-tracking/ — Beads Workflow Skill

## Purpose

The skill that governs how a session uses the beads issue tracker (`bd`) in a
project that carries one: finding claimable work, filing tasks and epics with
real dependencies, claiming and closing them, and landing a session so no work
is stranded locally.

## Ownership

| Path | Owns |
| --- | --- |
| `SKILL.md` | The binding procedure: the activation gate, session start, what's next, issue and dependency creation, task state, epics, the landing sequence, the command traps, and the verification block |

This folder owns no code, no issue data, and no `.beads/` directory anywhere in
this repository. Unlike the other three project-carried skills it has no
`agents/openai.yaml`, so it is invoked by name and carries no display metadata.

## Local Contracts

- Follow `SKILL.md`. Do not paraphrase it here.
- **The binary is installed; the workspace is not.** `bd version 1.0.4` resolves
  on `PATH`, so every command in `SKILL.md` is runnable — what is missing is a
  workspace, not a tool. This project also is not a git repository yet, and
  beads needs one: its data rides the git remote on `refs/dolt/data` and its
  sync path is git hooks. Activation is a phase-0 question, not a today one.
- **The procedure is pinned to a `bd` version, and that is its maintenance
  burden.** Every command in `SKILL.md` was verified against 1.0.4, including
  the ones recorded as failing — `bd sync` is named there because an earlier
  revision of this skill required it and 1.0.4 rejects it as an unknown command.
  An upgrade can invalidate the Command Traps table in either direction, so
  re-verify a trap against the installed binary before trusting it, and record
  the version checked.
- **Where beads guidance and the root git policy disagree, the root policy
  wins.** `bd worktree create` creates a *named branch* and is not caught by the
  global no-branch guard, which anchors on the literal `git` token. `SKILL.md`
  therefore forbids that one bd command and routes epic isolation through
  `git worktree add --detach`. This is a deliberate divergence from upstream
  beads guidance rather than a porting gap: a detached worktree resolves the
  same beads database through git's common directory, so the isolation is kept
  and only the branch is given up.

## Work Guidance

The skill self-disables here and the parent records why. What would make it live
is a `.beads/` directory, which belongs to the phase that starts tracking work in
beads instead of in prose — see [`../../docs/07-BUILD-PHASES.md`](../../docs/07-BUILD-PHASES.md)
for that ordering. Until then, read `SKILL.md` for the shape of the workflow and
leave it unrun.

If it does become live, two things need re-deciding before the first session
leans on it: the `bd` version the Command Traps table was verified against, and
whether this project has gates yet to fill step 2 of the landing sequence
("run quality gates"), which today names no command because none exists.

## Verification

The gate `SKILL.md` closes on needs a live beads workspace, which this project
does not have. What is checkable here is that the skill stays correctly dormant,
correctly named, and pinned to the version it was written against.

```bash
test -d .beads && echo "beads workspace now exists — re-read Activation in SKILL.md"
grep -q '^name: issue-tracking$' skills/issue-tracking/SKILL.md \
  || echo "frontmatter name no longer matches the directory"
bd version   # not 1.0.4 → re-verify the Command Traps table before trusting it
```

## Child STELLAR Index

None — leaf. Parent: [`../AGENTS.md`](../AGENTS.md).
