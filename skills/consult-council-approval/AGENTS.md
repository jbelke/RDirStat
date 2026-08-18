# skills/consult-council-approval/ — Advisory Council Gate Skill

## Purpose

The skill that routes a decision with real trade-offs to one chartered advisory
council, collects falsifiable seat positions, and closes with the chair's call
and the evidence that would reverse it.

## Ownership

| Path | Owns |
| --- | --- |
| `SKILL.md` | The binding procedure: when to convene, routing to exactly one council, the consultation protocol, convening modes, approval semantics, how the call is recorded, and how the roster is maintained |
| `councils/` | Seven charters — `protocol.md`, `security-identity.md`, `architecture-scale.md`, `experience.md`, `release-operations.md`, `agent-automation.md`, `general.md`. Each names its chair, seats, and the failure mode each seat owns. |
| `agents/openai.yaml` | Display name, short description, and the default prompt that names this skill |

This folder owns no code, and councils advise and gate — they never implement.
Implementation stays with the matched primary skill.

## Local Contracts

- Follow `SKILL.md`. Do not paraphrase it here.
- **One council per decision.** Cross-domain decisions take the chair from the
  dominant domain plus at most two borrowed seats. Convening two full councils
  is the failure mode the routing table exists to prevent.
- **A seat earns its place by owning a failure mode, not a personality.** A seat
  with nothing falsifiable to say passes silently; roleplay filler is worse than
  an empty seat because it reads as review.
- **The call is not closed without its reversal evidence.** "What would change
  this" is the deliverable, not a courtesy at the end.
- Do not convene for settled convention, a trivially reversible change, or a
  question an existing design law already answers — cite the law instead.

## Work Guidance

The routing table and the pairings in `SKILL.md` name CoCO primary skills
(`$add-coco-nostr-capability`, `$secure-coco-boundaries`, `$evolve-coco-relay`,
`$verify-coco-change`, `$build-coco-client`, `$extend-coco-agent-surface`) and
CoCO recording surfaces (Beads, workstreams, PRs). None of those exist in this
project.

Until they are ported, read the "Decision touches" column literally — it maps
cleanly onto this project's real decisions — and ignore the "Pairs with" column.
The Architecture & Scale council is the one this project will actually use:
stores, systems of record, scaling moves, and capacity claims is a precise
description of `.settings/docs/01-ARCHITECTURE.md` and `.settings/docs/06-DATA.md`. Record the
call in the doc the decision lives in, since there is no Bead to put it in.

## Verification

None — the skill's closeout names a repo gate (`just check-skills`) that does
not exist in this project. A council call is verified by its record: the doc it
landed in states the call, the seats consulted, and the reversal evidence.

## Child STELLAR Index

None — leaf. Parent: [`../AGENTS.md`](../AGENTS.md).
