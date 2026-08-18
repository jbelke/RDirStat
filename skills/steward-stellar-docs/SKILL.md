---
name: steward-stellar-docs
description: Re-scan, repair, and extend the STELLAR/DOX `AGENTS.md` hierarchy from a committed snapshot, so a documentation pass is a delta against known state rather than a rewrite from memory. Use to run a scheduled or post-change documentation pass, to bring the chain back after it has drifted, to place a new `AGENTS.md` when a folder becomes a durable boundary, to repair orphaned or dangling child-index rows, and to keep the `.settings/reference-code/` index pointing at the third-party checkouts so their code stays findable. Use it as the closeout step of any change that altered structure, contracts, ownership, or workflow. Do not use it to make the code change itself, and do not use it to rewrite a doc whose subtree has not moved.
---

# Steward STELLAR Docs

The documentation pass, run as a **delta against a committed snapshot** rather
than a rewrite from memory. STELLAR and DOX name the same contract; this skill
implements it, and the tooling accepts both spellings of the index heading.

`.settings/stellar/snapshot.json` is the state. It records, per `AGENTS.md`,
what the doc *said* (its sections and their sizes) and what it *owned* (the
tracked paths beneath it that no nearer doc claims). Because it is tracked, the
delta survives a cold context and shows up in review as a diff.

```bash
just stellar-scan       # report the delta — read this before touching a doc
just check-stellar      # gate: chain errors + losses, plus the core tests
just stellar-snapshot   # accept the current state; refreshes the reference index
```

## What this owns

Every `AGENTS.md` in the tracked tree, their Child STELLAR Index rows, and
`.settings/stellar/` (the snapshot and the reference-code index). It owns no
code. A doc claim that a `grep` cannot reproduce is the defect this skill
exists to remove, so it reads the subtree before it writes the doc.

It does **not** own the guards that pin specific documents to specific
artifacts — `just check-architecture-map` (the root crate map, the README crate
table, the Rust badge), `just check-gauntlet`, `just check-alert-runbooks`,
`just check-skills`. Those are narrower and authoritative; when one disagrees
with this scan, it wins.

## Run a pass

1. **Read the delta first.** `just stellar-scan`. Never open a doc before the
   scan says its subtree moved — a pass that rewrites unchanged docs is how
   durable contracts get paraphrased into nothing.
2. **Take the buckets in this order**, because each one changes the next:
   - `CHAIN ERRORS` — an orphaned doc or a dangling index row. Always fix; they
     rot with no file changing, since a *different* commit can delete the target.
   - `LOSSES` — content the snapshot had and the tree does not. Restore it or
     justify it (below). Never refresh the snapshot to make one disappear
     without deciding it first.
   - `New docs` / `Docs deleted` — wire or unwire the parent's Child index.
   - `Scope changed` — the subtree gained or lost files. Read what moved
     (`git diff --stat`) and ask whether Ownership, Local Contracts, or
     Verification in the owning doc is now wrong. Usually one of them is.
   - `Doc text changed` — someone already edited it; check it still agrees with
     its parent and children.
   - `Warnings` — a doc with no Child index. Add one; "None — leaf" is a
     complete answer and is what makes the leaf legible.
3. **Fix the docs, then `just check-stellar`** — read its exit code, not its
   output. Piping a gate into `head`/`grep` reports the filter's status.
4. **`just stellar-snapshot`** to accept, and commit the snapshot *in the same
   commit* as the doc edits. A snapshot committed alone is an unreviewed claim
   that nothing was lost.

## Modes

| Snapshot | What the pass is |
| --- | --- |
| **Missing** | Cold start. Every doc lands in `New docs` and no loss can be detected, because nothing is known. Do not treat this as "clean" — walk the chain, fix what the scan flags, then snapshot to establish the floor. |
| **Stale** (drift, or old by `git log`) | The normal case. Work the buckets above. Age alone is not a defect; drift is. The scan prints both. |
| **Current** | Nothing to do. Say so and stop. Refreshing a current snapshot produces an empty diff and trains reviewers to skip it. |

## Never lose existing data

This is the property the whole design serves, and it has two halves.

**The tool half.** `diffSnapshots` reports a `dropped-section`, a
`gutted-section` (over half its lines gone), a `dropped-child-row`, a
`shrunk-doc` (under 60% of its bytes) and a `deleted-doc`. `just check-stellar`
exits non-zero on any of them. So a regeneration cannot quietly replace a
17 KB contract with a paraphrase — it has to pass through a failing gate and a
visible snapshot diff.

**The judgment half**, which the tool cannot do:

- **Merge, never replace.** Read the whole doc, then add and amend. Rewriting
  from an outline drops the sentence that recorded why a rule exists, and that
  sentence is the doc's entire value — the rule alone gets deleted by the next
  person who finds it inconvenient.
- **Deleting stale text is required; deleting text you did not verify is not.**
  The framework says to remove contradictory notes. It does not license
  dropping a contract because this session lacks the context to see its point.
  If you cannot tell, keep it and say in the commit message that you kept it.
- **A loss you intend is still a loss.** Justify it in the commit message that
  carries the snapshot diff. "Removed § Convex tier — the tier was removed in
  <sha>" is a justification; refreshing the snapshot silently is not.
- **Preserved prose survives regeneration by construction** in the reference
  index: generated regions sit between `<!-- stellar-ref:NAME:start -->` markers
  and everything else is copied byte-for-byte. Put the *why* outside the
  markers. Never hand-edit inside them; the next `--write` overwrites it.

## Reference code is indexed, never adopted

`.settings/reference-code/` holds third-party checkouts and is gitignored, so
the clones are invisible to git and their own `AGENTS.md` files are upstream
documents. `.settings/AGENTS.md` stops the STELLAR chain at itself for exactly
that reason: `coco-cli/` alone carries ~30 nested `AGENTS.md`, and step 4 of
Read Before Editing walks an agent straight into them.

`.settings/stellar/reference-index.md` resolves the tension. It is the tracked
record — status, path, entry points, upstream docs, and which tracked files cite
the checkout — so `cloudflare-os`'s `packages/gatekeeper-*` is findable in one
grep without any of it becoming binding. Three rules:

- **A row is never deleted when a clone goes missing.** An absent clone is the
  normal state of a gitignored checkout; the row flips to `Status: absent` and
  keeps its prose. Deleting it would lose the only tracked pointer to the source.
- **Every present checkout carries its own `AGENTS.md` map.** The index row's
  `Own docs` line is how a reader decides whether a checkout is legible before
  opening it, so a checkout with no `AGENTS.md` is not properly indexed. When a
  pass finds one missing, **author it inside the checkout**: a short
  upstream-style map (purpose, load-bearing paths, license constraints, what to
  read first) that opens by declaring itself outside the CoCo STELLAR chain and
  non-binding. It is gitignored like the rest of the clone — the tracked record
  stays the index row, whose `Own docs` line picks it up on the next
  `stellar-snapshot`. `compass-ts` is the worked example.
- **Never edit *upstream* files under `.settings/reference-code/`.** Those edits are
  invisible to git, unreviewable, and lost on the next clone. Quote what
  matters into a tracked doc instead. The one exception is the `AGENTS.md` map
  above, which is ours, additive, and cheap to re-author if a re-clone drops it.

## Placing a new doc

Create an `AGENTS.md` when a folder is a durable boundary with its own purpose,
rules, or quality standards — not merely when it is large. The scan cannot make
this call; it only reports scope growth as the signal to consider it.

Section order is `Purpose`, `Ownership`, `Local Contracts`, `Work Guidance`,
`Verification`, `Child STELLAR Index`. Leave `Work Guidance` empty when the
project has no standard yet, and leave `Verification` empty until a check
exists — an aspirational Verification block is worse than an absent one,
because it reads as a gate that runs.

Creating a doc is always at least two files: the doc, and the parent's Child
index row. The scan fails an orphan for exactly this reason.

## Refuse

- Do not rewrite a doc whose subtree did not move. Absence of change is a result.
- Do not refresh the snapshot to clear a failing check without deciding the loss.
- Do not add a Verification block naming a command that does not exist.
- Do not pull `.settings/reference-code/` docs into the chain, or edit upstream files
  under it. Authoring a missing checkout `AGENTS.md` map (above) is the one
  sanctioned write.
- Do not fix code found during a pass. File it; route it to a primary skill.
- Do not restate a parent's rule in a child. Duplication drifts; the chain is
  read root-first by contract.

## Closeout

```bash
just check-stellar        # chain errors, losses, and the core tests
just check-architecture-map   # if a crate or top-level path moved
just check-skills         # if any skill changed
```

Then commit the doc edits and `.settings/stellar/snapshot.json` together, and
say in the message which docs you deliberately left unchanged and why. Route
any code defect found during the pass through `$drive-coco-maturity` rather
than fixing it here.
