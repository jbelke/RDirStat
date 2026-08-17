# Release & Operations Council

Owns releases, migrations, deploy topology, configuration, incidents, and
backup/restore. Its job is to make every release retreatable, every deploy
provable, and every failure legible to whoever is on call.

Convene when: cutting a release; adding or changing a migration; changing
deploy topology, images, charts, or config surface; responding to an incident;
touching backup or restore paths.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| Operator Chair (chair) | Illegible 3am failure | How does this fail, what does the on-call see, and which probe or metric says so? | A failure mode has no observable signature |
| Migration Warden | Irreversible schema damage | Is the migration forward-only and append-ordered? Are frozen checksums untouched? Do partition and fence invariants hold? | A checksum-frozen migration is edited, or ordering is rewritten |
| Provenance Auditor | The green-CI illusion | Which live relay ran this exact code, and what re-probe confirmed the behavior? Could a stale image make a flag a silent no-op? | "Resolved" is claimed from CI alone, with no live probe |
| Rollback Planner | Unretreatable releases | What is the undo path? Is the tag annotated, at the verified deployed commit, with the version string in lockstep? | A published tag would be re-pointed — broken releases get the next patch, never a moved tag |
| Data Guardian | Unrestorable backups | When was restore last exercised, and what gates it? Does signed-set integrity hold? | A backup change lands without restore-gate evidence |

## Red lines

- Nothing is resolved until deployed to a live relay and re-probed.
- Tags are immutable; version strings ship in lockstep with the tag in the
  deployed commit.
- Core services never move behind a compose profile, and a service removed
  from the stack leaves the health gate in the same change.
