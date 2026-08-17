---
name: consult-council-approval
description: Advisory councils for decisions with real trade-offs in CoCO. Use before recommending or approving a course of action on protocol wire contracts, security and identity boundaries, architecture and scaling, client experience, release and operations, or agent automation. Routes the named decision to the matching council charter, collects falsifiable seat positions, and closes with the chair's call and its reversal evidence, recorded in the Bead, workstream, or PR. Do not use for settled convention, a trivially reversible change, or a question a design law already answers — cite the law instead.
---

# Consult Council Approval

Councils advise and gate; they never implement. Implementation stays with the
matched primary skill, and the council's recorded call travels with it. A seat
earns its place by owning a failure mode, not a personality — every seat below
maps to a design law, a measured ceiling, or an incident this repo has already
paid for once.

## When to convene

Convene a council when a decision:

- touches a Design Law in `AGENTS.md` (wire vocabulary, standing, revocation,
  systems of record, scaling axes, explicit denial);
- introduces a new mechanism — store, endpoint, policy surface, tool grant,
  background runtime — rather than a new attribute;
- is expensive to reverse (schema, published contract, release, federation);
- reverses or contradicts a previously recorded call.

Skip the council (and say so) when a design law already answers the question —
cite the law — or when the change is one reversible slice with no contract
surface. Do not convene ceremonially.

**Search for a prior call before convening.** A decision this repo has already
made, whose stated reversal condition has not fired, is settled: cite it and
skip. Re-opening it costs a council session and risks reversing a call whose
reasoning nobody re-read.

Recorded calls are **not only in beads and PRs**. The most load-bearing ones sit
in the header comment of the module that implemented them, where they are read
by the next person to touch the code — `features/apps/lib/zip.ts` carries the
whole jszip-versus-hand-write trade, including the condition that would reopen
it. So search the code, not just the tracker:

```bash
# the tracker
bd list --json | grep -i <topic>
# the code and docs — headers, AGENTS.md, CIP registry
rg -n -i "<topic>" --glob '!node_modules' --glob '!target'
```

If a prior call exists, quote its reversal condition and say whether it fired.
"It fired" is a reason to convene; "it did not" is a reason to skip.

## Route to one council

| Decision touches | Council | Charter | Pairs with |
| --- | --- | --- | --- |
| Event kinds, tags, content schemas, replaceability, NIPs, query/HTTP contract | Protocol | `councils/protocol.md` | `$add-coco-nostr-capability` |
| Standing, grants, membership, moderation, keys, tenant boundaries, edge trust | Security & Identity | `councils/security-identity.md` | `$secure-coco-boundaries` |
| Stores, systems of record, scaling moves, federation-adjacent work, capacity claims | Architecture & Scale | `councils/architecture-scale.md` | `$evolve-coco-relay`, `$measure-coco-relay` |
| Desktop, web, or mobile experience, parity, accessibility, interaction performance | Experience | `councils/experience.md` | `$build-coco-client` |
| Releases, migrations, deploy topology, config, incidents, backup/restore | Release & Operations | `councils/release-operations.md` | `$verify-coco-change` |
| CLI, ACP, MCP tools, workflows, personas, tool grants, approval gates | Agent & Automation | `councils/agent-automation.md` | `$extend-coco-agent-surface` |
| Anything else with real trade-offs | General | `councils/general.md` | any |

Cross-domain decisions get the chair from the dominant domain plus at most two
borrowed seats from adjacent charters. Never convene two full councils; pick
the dominant one and borrow.

## Consultation protocol

1. State the decision as one falsifiable sentence, with the live options and
   the evidence already in hand. A council cannot review a vibe.
2. **Verify every load-bearing claim before seating anyone.** Run the check —
   read the file, run the grep, confirm the gate exists. Then mark each claim
   `[verified]` with what confirmed it, or `[unverified]` explicitly. A claim
   that decides the answer and was never checked is the most expensive thing a
   council can be handed: seats reason from it, the chair closes on it, and the
   record inherits it.

   This is not ceremony. In the thread-export session, both load-bearing claims
   were unverified and checking them decided most of the question — one turned
   out to be a *recorded prior call* that made the question moot, the other
   turned a cost trade into a block. Deliberation added little; verification
   added everything. Budget accordingly.

   A seat must **block** on any `[unverified]` claim its position depends on,
   rather than assuming it either way.
3. Read the matched charter. Seat the chair plus the seats whose "Owns" column
   the decision actually touches — usually three to five voices total.
4. Each seated voice returns: a position, its single strongest falsifiable
   objection or named risk, and what evidence would change its position. A
   seat with nothing concrete passes silently. No roleplay filler.
5. The chair closes with: the call, the strongest objection overruled and why,
   and the reversal evidence — what observation would reopen this decision.

## Convening modes

- **Inline (default):** one context, voice by voice against the charter rows.
  Cheap, and sufficient for almost every decision.
- **Parallel:** run each seat as its own subagent with the seat prompt below,
  launched together rather than one after another, then have the chair
  synthesize. A seat receives its charter row, the decision sentence, and the
  evidence — never another seat's output, and never the convener's draft
  recommendation, which is the anchoring this mode exists to remove. Keep it
  to five seats or fewer.

**Parallel is required, not preferred, when the convener authored the proposal
under review.** An author steering every seat from inside one context is the
inline mode's quiet failure, and it is the common case — most councils are
convened by whoever just wrote the thing. "Reach for it" was too weak: it reads
as a preference and loses to whatever is cheaper in the moment.

### When parallel is unavailable

Subagents are not always available — a session may forbid spawning them, or the
runtime may not offer them. Parallel being impossible does not make inline
adequate; it makes inline **degraded**, and a degraded review that presents
itself as a clean one is worse than no review. So:

1. **Say so in the output and in the minutes.** Name the mode, and why it was
   not parallel. A reader of the record must be able to tell how much
   independence the positions actually had.
2. **Seat the Skeptic (General) and point it at the convener's own claims**,
   by name, as its first job. It is the seat that owns "plausible-but-wrong
   reasoning" and it is the only available substitute for independence.
3. **Argue against the framing, not just within it.** Ask what the proposal's
   own structure excludes — a convener who lists three options has usually
   already discarded a fourth.
4. **Treat the call as provisional when it is close.** If the decision did not
   fall out of verified evidence, record it as `provisional — inline, authored
   by convener` and re-run it in parallel before anything expensive depends on
   it.

Degraded mode is sufficient when verification decided the question and the
seats mostly confirmed it. It is not sufficient when the call rests on
judgement between live options.

Seat prompt template for parallel mode:

```
You hold the <seat> seat on the <council> council of the CoCO
project. Your charter row — Owns: <owns>. Asks: <asks>. Blocks when:
<blocks>. Decision under review: <one falsifiable sentence, options,
evidence>. Return exactly: your position (approve, approve-with-conditions,
block, or pass), your single strongest falsifiable objection or named risk,
and what evidence would change your position. No filler.
```

## Approval semantics

A council returns exactly one of:

- **Approve** — proceed with the matched implementation skill.
- **Approve with conditions** — every condition must be testable (a check, a
  probe, a measurement), and the conditions travel into the implementation
  slice as acceptance criteria.
- **Block** — name the record, law, or missing evidence that decides. A block
  from a seat's red line stands until the underlying evidence changes;
  escalate to the human owner rather than re-litigating in-session.

Councils never approve on a human's behalf anything irreversible or
outward-facing that the root contract reserves for humans — they prepare the
recommendation and its record.

## Record the call

Write minutes into the Bead (`bd update <id> --notes`) or the PR description:
the decision sentence, the call, dissenting objections, conditions, and the
reversal evidence. A call without a written record did not happen — the
decision-record twin of "explicit denial, never a silent empty result."

Minutes must also carry two things a later reader cannot reconstruct:

- **The convening mode**, and whether the convener authored the proposal. This
  is how someone judges the weight of the record rather than assuming it.
- **Which claims were verified, and by what.** A conclusion is only as good as
  the claim under it, and the check that confirmed it is usually one line.

**Put the call where the next reader will be**, not only in the tracker. If the
decision constrains one module, its reversal condition belongs in that module's
header comment — that is where someone about to violate it is looking, and a
bead they never open cannot stop them. `features/apps/lib/zip.ts` is the model:
the trade, the rejected alternatives, and the condition that would reopen it,
in the file the decision governs. The bead holds the full minutes; the header
holds the constraint and its escape clause.

## Maintain the roster

Charters are contracts, not lore. When a design law changes, update the seat
that owns it in the same change. A seat that no decision has touched in living
memory gets cut — an unused voice is inventory, and this repo deletes
inventory rather than storing it.
