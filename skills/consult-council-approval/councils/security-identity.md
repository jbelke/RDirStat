# Security & Identity Council

Owns standing, grants, membership, revocation, moderation, key handling,
tenant boundaries, and edge trust. Its job is to keep every access decision
answerable and every revocation permanent.

Convene when: changing who can do what — grants, roles, membership, bans,
ACLs; handling keys or encrypted payloads; touching the community boundary,
proxy headers, or anything an unauthenticated party can reach.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| Threat Modeler (chair) | The unconsidered adversary | Who benefits from abusing this, along which path, and what does it cost them? | No abuse path was considered at all |
| Revocation Guardian | Resurrection by timestamp | Under partition or dual writers, how does a revoke beat a concurrent grant? Is there an epoch over an append-only log, resolved fail-closed? | Any revoke, end-date, or removal resolves by wall-clock last-write-wins |
| Boundary Auditor | Cross-tenant leakage | Which record answers this access decision, at each layer? | A denial surfaces as a silent empty result instead of an answerable record |
| Registry Steward | Standing frozen into credentials | Is standing derived from the registry, effective-dated as `[since, until)`, rather than stamped into a profile or claim? | Org or role is baked into a credential, or a name encodes ownership |
| Edge Skeptic | Forgeable inputs | Which header or claim can an attacker set, and what strips it at the edge? | Trust is placed in transport metadata nothing verifies |

## Red lines

- Monotonic revocation is absorbing: once revoked at epoch *n*, no grant at
  epoch ≤ *n* reinstates — and this must land before any federation or second
  writer, never after.
- One grant store; every enforcement point reads it; a mapping table or sync
  job between two enforcement points is a design violation.
- Green CI is not evidence an enforcement path runs in production — a live
  re-probe is.
