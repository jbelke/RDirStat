# Agent & Automation Council

Owns the agent surface: the CLI, ACP harness, MCP tools, workflow engine,
personas, tool grants, and approval gates. Its job is to keep automation
operable by agents, bounded by grants, deterministic under retry, and
overridable by humans.

Convene when: adding or changing a CLI subcommand, ACP behavior, or MCP tool;
granting an agent a new capability; adding workflow triggers, actions, or
conditions; changing any approval or review gate.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| Agent Ergonomist (chair) | Surface unusable by agents | Can an agent operate this end to end from the docs alone — exit codes, structured output, global flags, deep links? | Success and failure are indistinguishable from output plus exit code |
| Grant Skeptic | Over-broad tools | What is the least privilege that does the job, and whose authority does the tool call borrow? | A new capability ships without an owner-review path |
| Determinism Warden | Flaky automation | Is the operation idempotent under retry? What does a NIP-33 write conflict (exit 5) do to the run? | A retried step double-applies, or a conflict is swallowed |
| Workflow Auditor | Untestable conditions | Does the condition stay a simple, testable evalexpr expression? | A policy DSL is smuggled in as workflow conditions — add attributes, not mechanisms |
| Human-Override Advocate | Runaway automation | Where does a human approve, pause, or reverse this? | An autonomous path crosses an irreversible action with no human gate |

## Red lines

- Every automated write is attributable to a key, and that key's standing is
  registry-derived, never assumed.
- Owner-reviewed drafts stay owner-reviewed; no automation quietly removes a
  review gate that existed before it.
- Reads return sig-stripped JSON, writes return their acceptance record — an
  agent must never need to scrape prose to learn what happened.
