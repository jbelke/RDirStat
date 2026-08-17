---
name: desktop-screenshot
description: Capture CoCO desktop screenshots with the repository's Playwright mock bridge and publish PR images through immutable git-backed URLs. Use when an agent needs a focused desktop visual, interaction-state capture, cropped UI evidence, or GitHub-ready screenshot set.
---

# Desktop Screenshot Skill

## CRITICAL: How to Host Screenshots for PRs

**NEVER use the product CLI upload command, the relay media endpoint, or any third-party image
host (imgur, imgbb, etc.) for PR screenshots.** Relay media URLs fail through
GitHub's camo proxy (`Non-Image content-type returned`). External hosts are
unreliable and may expose content.

**ALWAYS use `scripts/post-screenshots.sh`** — it hosts PNGs on a per-developer
git branch with immutable commit-SHA URLs that render correctly on GitHub.
If you manually compose or edit PR markdown, run
`scripts/check-pr-image-urls.sh <markdown-file>` before posting. The checker
fails on CoCO relay media URLs so broken images are caught locally.

This hosting rule applies to any PNG you want in a PR, including mobile
simulator screenshots captured outside the desktop Playwright helper.

## Step 1 — Capture Screenshots

`just desktop-screenshot` builds the frontend, starts a preview server, and
runs Playwright with the mock bridge (no relay needed).

```bash
just desktop-screenshot --name home
just desktop-screenshot --name channel --route /channels/general
just desktop-screenshot --name ctx-menu --right-click channel-random --clip 0,200,320,300
just desktop-screenshot --name sidebar --active-channel general --messages /tmp/msgs.json --clip 0,0,256,720
```

**Flags:** `--name` (filename, required), `--route` (client route), `--active-channel`
(channel to view), `--click`/`--right-click`/`--hover` (interact before capture),
`--clip` (crop as `x,y,w,h`), `--messages` (JSON file), `--wait` (ms, default 2000),
`--viewport` (WxH, default 1280x720), `--outdir` (default `test-results/screenshots`).

Output: PNG path on stdout.

### Injecting Messages

Write a JSON array to a temp file. `channelName` and `content` are required:

```json
[
  {"channelName": "random", "content": "Hey check this out", "kind": 40002},
  {"channelName": "random", "content": "Another message"}
]
```

Without `--active-channel`, the helper navigates to the message channel (for
showing content). With `--active-channel`, messages can target other channels
while the camera stays put (for unread indicators, badges).

### Available Mock Channels

`general`, `random`, `design`, `sales`, `engineering`, `agents`, `watercooler`,
`announcements`, `alice-tyler`, `bob-tyler`.

`general` has pre-seeded messages (always shows `hasUnread`). Use `engineering`
for "no unread" visual states.

## Step 2 — Post to a PR

```bash
./scripts/post-screenshots.sh <PR-number> test-results/screenshots
./scripts/post-screenshots.sh <PR-number> test-results/screenshots body.md
```

The script pushes images to `agent-screenshots/<github-username>` and posts a
PR comment with `## Screenshots` and all images. Re-runs overwrite that PR's
image blobs but **append a new comment** — after reposting, delete the
superseded comment (find it with `gh pr view <pr> --json comments`, remove it
with `gh api -X DELETE repos/<org>/<repo>/issues/comments/<id>`) so reviewers
do not keep reading the stale set.

### Body Templates

The optional third argument is a markdown file with `{{filename}}` placeholders
(without `.png`). Images not referenced by a placeholder are appended at the end.

```markdown
### Context menu
Right-click shows "Star channel".

{{01-ctx-menu}}

### Starred section
`engineering` appears under Starred.

{{02-starred}}
```

## Gotchas

1. **Stale server** — a prior build's preview server keeps serving old code.
   Kill port 4173 after code changes; the next `just desktop-screenshot` rebuilds
   for you. Any manual rebuild must be `pnpm build:e2e` — a plain
   `pnpm run build` strips the mock Tauri bridge, and every capture after it
   renders "Relay connection failed" instead of the UI.
2. **Clip for readability** — full 1280x720 screenshots are hard to read for sidebar
   features. Sidebar = 256px wide; context menus ~450px.
3. **`post-screenshots.sh` requires `gh` auth** — the script uses `gh api` and
   `gh pr comment`. Ensure `gh auth status` succeeds.
