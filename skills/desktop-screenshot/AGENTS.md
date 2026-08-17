# skills/desktop-screenshot/ — Screenshot Capture Skill

## Purpose

The skill that captures desktop UI screenshots through a repository's Playwright
mock bridge and publishes them for pull requests through immutable, git-backed
URLs.

## Ownership

| Path | Owns |
| --- | --- |
| `SKILL.md` | The binding procedure: the hosting rule, capture (step 1), posting to a PR (step 2), and the gotchas |
| `agents/openai.yaml` | Display name, short description, and the default prompt that names this skill |

This folder owns no code and no captured images.

## Local Contracts

- Follow `SKILL.md`. Do not paraphrase it here.
- **Screenshots are hosted through immutable git-backed URLs only.** `SKILL.md`
  opens with this as its one CRITICAL rule and names the alternatives it forbids
  — a product CLI upload command, a relay media endpoint, third-party image
  hosts. A URL that can change or expire turns a PR's visual evidence into a
  broken image, which is worse than no image because the review already passed.
- Capture is a deliberate state, not a screen grab: the skill exists to produce
  a focused visual of a named interaction state, cropped to the evidence.

## Work Guidance

**This skill does not apply to this project yet, and cannot.** It targets the
CoCO desktop client through that repository's Playwright mock bridge. This
project has no frontend, no Playwright, no mock bridge, and no GitHub remote —
`docs/01-ARCHITECTURE.md` places React + Vite and a `<canvas>` treemap in the
*planned* tree, not an existing one.

It becomes relevant at the phase that builds the UI (`docs/05-UI.md`). Porting
it then means replacing the CoCO bridge with this project's harness and
re-deciding the hosting rule against whatever remote exists by that point; the
hosting rule itself — immutable URLs, never a third-party host — is the part
worth carrying over unchanged.

Until then, treat it as a reference for how UI evidence gets attached to a
change, and do not invoke it.

## Verification

None — the skill's closeout names a repo gate (`just check-skills`) that does
not exist in this project, and the capture path it drives is absent here.

## Child STELLAR Index

None — leaf. Parent: [`../AGENTS.md`](../AGENTS.md).
