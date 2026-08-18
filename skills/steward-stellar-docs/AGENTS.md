# skills/steward-stellar-docs/ — Documentation Pass Skill

## Purpose

The skill that re-scans, repairs, and extends the STELLAR/DOX `AGENTS.md`
hierarchy from a committed snapshot, so a documentation pass is a delta against
known state rather than a rewrite from memory.

## Ownership

| Path | Owns |
| --- | --- |
| `SKILL.md` | The binding procedure: scan first, work the buckets in order, never lose snapshot content without a decision, never adopt upstream clones |
| `agents/openai.yaml` | UI metadata and the default prompt that names this skill |

This folder owns no code. In the repository this skill came from, the scan it
calls lives in `scripts/` and is invoked through the root `Justfile`.

**Neither exists in this project.** There is no `Justfile`, no `scripts/`, and
no `.settings/stellar/snapshot.json`, so `just stellar-scan`,
`just check-stellar`, and `just stellar-snapshot` are unavailable here and every
pass is a cold start by definition: no snapshot means no loss can be detected,
and the judgment half of "never lose existing data" is the whole of it. Read the
chain by hand, and merge rather than rewrite.

## Local Contracts

- Follow `SKILL.md`. Do not paraphrase it here.
- The skill owns every tracked `AGENTS.md`, their Child STELLAR Index rows, and
  `.settings/stellar/`. It does not own application code and does not fix code
  found during a pass.
- A pass that rewrites a doc whose subtree did not move is how durable
  contracts get paraphrased into nothing.

## Work Guidance

When the user asks to document, run `$steward-stellar-docs` rather than
free-writing an `AGENTS.md` from memory.

## Verification

The skill's closeout is the repo gate:

```bash
just check-stellar
```

## Child STELLAR Index

None — leaf. Parent: `AGENTS.md`.
