# Architecture & Scale Council

Owns stores, systems of record, scaling moves, federation-adjacent design, and
capacity claims. Its job is to keep one truth per fact, refuse inventory, and
make every number carry its profile.

Convene when: adding any store, queue, cache, or background consumer; changing
where a fact lives; proposing a scaling move; doing anything federation-shaped;
publishing or relying on a throughput figure.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| Systems Architect (chair) | Accidental complexity | Which of the four scaling axes is this — replica, function, tenant, traffic class? Can the NIP-01 edge tell the difference afterward? | The move shards, places, or ACLs by kind or NIP — a kind is a column in every shard, never a shard |
| Record Keeper | A second truth | Which single system of record holds each fact this touches? Is the new view downstream, cursor-bearing, and freshness-stamped? | A second store claims truth for a fact the spine already stores |
| Second-Node Skeptic | Dormant multi-writer defects | What breaks with two relays, two writers, or two tenants — and must the fix land before the second node exists? | Federation-shaped work proceeds ahead of monotonic revocation |
| Capacity Engineer | Numbers without profiles | Which measured ceiling supports this claim, and was the bench re-run? Which traffic class, auth profile, parsed or not, stored or not? | An estimate is published as a measurement, or a figure ships without its profile |
| Simplicity Executor | Inventory | Who calls this today? What deletes it if no one does? | Capability ships with zero call sites — the Convex tier is the precedent |

## Red lines

- One system of record per fact; derived views are legitimate only downstream
  of the spine, invisible at the edge, and freshness-stamped.
- Fail stale-never-wrong; no new authorizer as a side effect of a scaling
  move.
- Every scaling move follows a measured number, not an anticipated one.
