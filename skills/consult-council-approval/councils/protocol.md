# Protocol Council

Owns the Nostr wire contract: event kinds, tags, content schemas,
replaceability rules, NIP contracts, and the narrow HTTP surface. Its job is to
keep the vocabulary small, enforced, interoperable, and consumable.

Convene when: adding or changing a kind, tag, or content schema; touching
replaceability or query contracts; proposing any new HTTP endpoint; drafting or
amending a NIP.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| Protocol Steward (chair) | Wire-contract incoherence | Is this an event kind first? What existing kind, tag, or filter already expresses it? | A new HTTP endpoint is proposed where an event kind would do |
| Interop Skeptic | Divergence from upstream Nostr | Does the NIP/CIP constituency rule say this belongs upstream? Does the kind number collide with an upstream assignment? | A colliding or upstream-owned kind ships without a recorded rationale |
| Enforcement Auditor | Dead vocabulary | What reads this kind or tag, and which check fails if nothing does? | Vocabulary lands with no enforcement point — it must fail `just check-kinds`, not wait for audit |
| Compat Warden | Silent breakage of the deployed fleet | How do existing clients, relays, and stored events behave against the new shape, and in what order does the rollout land? | An old reader would misparse the new shape silently |
| Consumer Advocate | Contracts consumers must guess at | How do the CLI, clients, and agents consume this — exit codes, `h`-tag scoping, compact format, explicit `kinds` filters? | The contract forces a consumer to guess or to bypass the p-gate |

## Red lines

- Names never encode ownership — no `org/team/channel` paths in keys, topics,
  or namespaces.
- New capability is a new attribute plus a new event kind, never a new policy
  engine.
- Channel scoping is `h` tags; a contract that scopes by `e` tags is wrong by
  construction.
