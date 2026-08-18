# src/ — React presentation

## Purpose

The React + TypeScript presentation for bounded backend queries. The current
screen is phase-0 scaffold; tree navigation, hierarchy views, and reports land
in later phases under `.settings/docs/05-UI.md`.

## Ownership

| Path | Owns |
| --- | --- |
| `main.tsx` | React entry point and global providers |
| `App.tsx` | Application-state composition and primary layout |
| `index.css`, `App.css` | Tailwind theme tokens, global accessibility rules, and component styling |
| `lib/` | Frontend utilities and future generated bindings/schema adapters |
| `assets/` | Project-owned frontend assets |

## Local Contracts

- The frontend never receives or caches the complete node arena. Tree rows,
  report rows, and drawn tiles come through bounded requests.
- Reject stale `TreeGeneration` responses and unknown Arrow schema versions.
- TanStack tables use backend/manual sorting; client-side sorting may not force
  full materialization.
- Canvas hierarchy views have a keyboard/VoiceOver-equivalent adjacent list.
- Logical and allocated bytes remain distinct; potential recovery is labelled
  as an estimate, never a promise.
- Filesystem actions require backend-issued authority and confirmation. A path
  string from the DOM is not sufficient.

## Work Guidance

Use `.settings/docs/05-UI.md` as the interaction contract and
`.settings/docs/01-ARCHITECTURE.md` as the IPC/state contract. Keep generated
bindings checked rather than hand-copying Rust interfaces.

## Verification

Run `just lint` and `just frontend`. Later interaction work also adds the
accessibility and p95 performance evidence required by phase 3/4.

## Child STELLAR Index

None. Parent: [`../AGENTS.md`](../AGENTS.md).
