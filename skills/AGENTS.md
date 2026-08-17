# skills/ — Agent Skills

## Purpose

The agent skills carried or installed for sessions working in this project: the
documentation pass, advisory-council gate, desktop screenshot capture, inactive
Beads workflow, and general Rust guidance.
A skill is a binding procedure an agent invokes by name (`$steward-stellar-docs`)
in place of improvising.

## Ownership

| Path | Owns | Applies here? |
| --- | --- | --- |
| `steward-stellar-docs/` | The documentation pass: re-scan, repair, and extend the `AGENTS.md` chain as a delta against a committed snapshot | Yes — this chain was authored under it |
| `consult-council-approval/` | The advisory-council gate: route a decision with real trade-offs to one of seven chartered councils, collect falsifiable seat positions, close with the chair's call and its reversal evidence | Partly — the method transfers, the CoCO wiring does not |
| `desktop-screenshot/` | Capturing desktop screenshots through a Playwright mock bridge and publishing them as immutable git-backed PR URLs | Not yet — see below |
| `issue-tracking/` | Beads workflow, deliberately self-disabled unless a root `.beads/` exists | No — this project has no `.beads/` |
| `rust-skills/` | Installed general Rust guidance (source/integrity pinned by root `skills-lock.json`) | Later — only while writing/reviewing Rust, subordinate to `docs/08-RUST-PRACTICES.md` |

Every skill folder has `SKILL.md`. `steward-stellar-docs/`,
`consult-council-approval/`, and `desktop-screenshot/` also have
`agents/openai.yaml`; `issue-tracking/` does not, and is invoked by name alone.
`consult-council-approval/` additionally owns seven council charters.
Installed/external skill shapes are not rewritten to match.

## Local Contracts

- **These skills were carried in from CoCO and still speak CoCO.** They name
  machinery this project does not have: Beads and workstreams as the place a
  call is recorded, primary skills like `$add-coco-nostr-capability` and
  `$verify-coco-change` as pairings, a product CLI and relay media endpoint, a
  Playwright mock bridge. Read those as *the shape of the procedure*, not as
  instructions to be followed literally. When a skill names a target that does
  not exist here, that is a porting gap to record, not a step to fake.
- **Do not paraphrase a `SKILL.md` into its `AGENTS.md`.** The skill file is the
  binding text; the `AGENTS.md` beside it says what the folder owns and points at
  it. Two copies of a procedure drift, and the copy that drifts is the one the
  reader happens to open.
- **A skill's Verification is the gate it actually closes on.** All three of
  these close on repo commands (`just check-stellar`, `just check-skills`) that
  do not exist in this project. Leave that stated, not silently dropped — an
  aspirational gate reads as one that runs.
- **Changing a skill's frontmatter `description` changes when it fires.** It is
  the routing surface, not documentation. Treat an edit to it as a behaviour
  change.
- **`issue-tracking` is inactive here.** Its own activation contract says not to
  run `bd init` when `.beads/` is absent. Carrying a skill is not authorization
  to introduce its state system.
- **Project Rust law wins.** `rust-skills` is broad external guidance;
  `docs/08-RUST-PRACTICES.md` is the narrower binding contract for this project.
  A conflict is resolved in favour of `08` and recorded there if recurring.

## Work Guidance

Invoke a skill by name rather than reimplementing it: `$steward-stellar-docs` to
close out a change that moved structure, contracts, ownership, or workflow;
`$consult-council-approval` before committing to a decision with a real
trade-off and an expensive reversal.

When porting one of these skills properly to this project, the work is: replace
CoCO-specific targets with this project's, and either build the gate its
Verification section names or empty that section. Doing one without the other
leaves the skill describing a repository that is not this one.

## Verification

None — no skills linter exists in this repository. The carried skill files name
`just check-skills`, but no `Justfile` exists. Until phase 0, check by hand that
every folder has `SKILL.md` frontmatter with matching `name`, and that every
installed skill has a matching lock entry.

```bash
for d in skills/*/; do
  test -f "$d/SKILL.md" || echo "missing SKILL.md: $d"
done
```

## Child STELLAR Index

| Child | Covers |
| --- | --- |
| [steward-stellar-docs/AGENTS.md](steward-stellar-docs/AGENTS.md) | The documentation pass |
| [consult-council-approval/AGENTS.md](consult-council-approval/AGENTS.md) | The advisory-council gate and its seven charters |
| [desktop-screenshot/AGENTS.md](desktop-screenshot/AGENTS.md) | Desktop screenshot capture and hosting |
| [issue-tracking/AGENTS.md](issue-tracking/AGENTS.md) | The beads workflow, its activation gate, and its divergence from `bd`'s own worktree guidance |

`rust-skills/` has no row on purpose: it is a symlink into `.agents/skills/`, and
the `AGENTS.md` it carries is the upstream project's, not ours. Index it here
through the Ownership table and `skills-lock.json`; do not adopt it into the
chain.

Parent: [`../AGENTS.md`](../AGENTS.md).
