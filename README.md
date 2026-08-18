# Stellar RDIRSTAT

A native macOS disk-usage and file-inventory app. A Tauri v2 + Rust rewrite of
[QDirStat](https://github.com/shundhammer/qdirstat) built to answer two questions
over volumes with tens of millions of entries:

1. Where is the selected tree's logical and allocated space concentrated?
2. What kinds of files account for it, and what changed since the previous scan?

The design volume is a 7.3 TiB APFS disk with **69 million inodes**. A full cold
scan of it should be a coffee break, not an overnight job, and the app must stay
honest when macOS denies access or when APFS makes "bytes attributed to files"
differ from "bytes physically reclaimable."

[![CI](https://github.com/jbelke/RDirStat/actions/workflows/ci.yml/badge.svg)](https://github.com/jbelke/RDirStat/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)

## Status

**v0.1.0 — first release.** Scanning, the virtualized tree, the treemap,
icicle and sunburst views, the Types/Ages/Diff/Dupes reports, snapshot save and
restore, folder sync and remote transfers all work and are reachable from the
UI.

What is *not* in this release, so you do not go looking for it:

- **Move to Trash refuses.** The control is present and explains itself: it
  needs a confirmation step that shows exactly what would move, and it will not
  move anything without showing you that first.
- **No Parquet/DuckDB catalog.** Reports run on the live scanned tree, in
  memory. There is no durable query store in this build.
- **The disk image is unsigned** and built for the host architecture only. See
  [Packaging](#packaging).
- **Sync cannot be cancelled** once applied.

Private local developer material under `.settings/` is gitignored and is not
part of this repository.

## Requirements

- macOS 14+ to build or run the app (`bundle.macOS.minimumSystemVersion`
  and CI agree on 14; see [Containers](#containers))
- Rust 1.90 (MSRV); the pinned toolchain is in `rust-toolchain.toml` (1.97.1)
- Node.js ≥ 22.12
- pnpm 10.30.1 (`packageManager` in `package.json`)
- Docker (optional; only the container profiles use it)

## Quick start

```bash
just bootstrap     # pnpm install --frozen-lockfile
just check         # formatting, lints, tests, frontend build
./rush.sh dev      # the development app
./rush.sh dmg      # the installable, branded .dmg
./rush.sh doctor   # what is installed, and what each environment resolves to
```

`just ci` adds dependency license/advisory reports (`cargo-deny`, `pnpm audit`).
GitHub Actions runs it on macOS for pushes to `main` and pull requests.

`just dev` still exists and runs `pnpm tauri dev` unadorned. `./rush.sh dev` is
the same thing with an environment applied.

## Environments

`./rush.sh` resolves an environment name into exactly two files, and everything
else follows from that:

| | |
| --- | --- |
| `.env.<environment>` | exported into the child process before it starts |
| `src-tauri/tauri.<profile>.conf.json` | merged over `src-tauri/tauri.conf.json` |

The environment file is looked up in order: `--env-file`, then
`.env.<environment>`, then the tracked `.env.<environment>.example` template
(with a warning), then `.env`. Only the templates are committed; your copies are
git-ignored. The app treats a real environment variable as higher precedence
than the `.env` it reads on its own, so this wins without deleting anything.

| Environment | Bundle identifier | Purpose |
| --- | --- | --- |
| `development` | `jbelke.rdirstat.dev` | Debug build, verbose logs, disposable snapshot store |
| `staging` | `jbelke.rdirstat.staging` | Release build that installs **beside** production |
| `production` | `jbelke.rdirstat` | The build that ships |

The separate identifiers are the point of the split: settings, the snapshot
store and Keychain items are keyed by bundle identifier, so staging cannot
evict the data a release install depends on, and an upgrade can be rehearsed
with both versions on one machine.

`--env` and `--profile` are separate, so values and build shape can be mixed:

```bash
./rush.sh dmg -e production -p staging   # production values, staging bundle id
./rush.sh run -e demo                    # .env.demo + tauri.demo.conf.json
```

A new environment needs nothing but those two files. `./rush.sh --help` has the
full flag set.

## Packaging

```bash
./rush.sh dmg                # production
./rush.sh dmg -e staging     # installs alongside it
```

### Installing the v0.1.0 image

The image is built for **the architecture of the machine that built it** —
there is no `--target universal-apple-darwin` in the build, so an arm64 image
will not launch on an Intel Mac and vice versa. Build it on the machine you
intend to run it on.

It is also **unsigned**, which is the part that surprises people. macOS
attaches a quarantine flag to anything that arrives through a browser, AirDrop
or a file share, and Gatekeeper refuses a quarantined bundle that carries no
Developer ID. The message it chooses is *"Stellar RDIRSTAT is damaged and can't
be opened"* — a signing failure wearing a corruption message. The build is not
damaged.

On the machine that built it, no quarantine flag is set and the app opens
normally. If you moved the image to another Mac, clear the flag deliberately —
having first satisfied yourself that you trust the source, because this is
exactly the check you are switching off:

```bash
xattr -dr com.apple.quarantine "/Applications/Stellar RDIRSTAT.app"
```

The real fix is a Developer ID signature and notarization, tracked as an open
issue. Until then this image is for local installs, not distribution.

> The `.dmg` carries a click-through licence agreement, so `hdiutil attach`
> waits for input. In a script, pipe it: `printf 'Y\n' | hdiutil attach ...`.

The disk image is branded end to end: window size and icon positions come from
`bundle.macOS.dmg` in `src-tauri/tauri.conf.json`, and the backdrop is generated
alongside the icons.

Every brand asset — the `.icns`, the `.ico`, the Windows tiles, the menu-bar
template, the DMG backdrop and the favicon — is rendered from one source file,
`scripts/generate-icons.mjs`, with no image dependencies: shapes are
signed-distance fields, PNG/ICO output is written on top of `node:zlib`, and
Tauri packages the macOS ICNS from the same rendered source. It is
byte-reproducible, which is what lets
`just check-icons` assert that the committed icons are still the generated ones.
That check is in `just check` because the failure it catches is otherwise
silent — nothing about a successful build tells you the icon reverted to the
Tauri v2 template.

```bash
just icons          # regenerate
just check-icons    # fail if the committed assets drifted
```

`./rush.sh dmg` refuses to package when that check fails, and reports whether
`APPLE_SIGNING_IDENTITY` is set. An unsigned image opens on the machine that
built it and is quarantined everywhere else, usually reported as "damaged" —
which is a signing failure wearing a corruption message.

## Containers

**Docker does not build the app or the `.dmg`, and no Dockerfile can:** the
shell links AppKit and `Security.framework`, and `hdiutil` is part of macOS
rather than a package. The container profiles cover the parts that are portable.

```bash
./rush.sh up dev                   # Vite dev server        http://localhost:1420
./rush.sh up staging -d            # built bundle via nginx http://localhost:4174
./rush.sh compose run --build --rm check   # typecheck, tests, docs
./rush.sh compose run --build --rm rust    # fmt/clippy/test for the portable crates
./rush.sh compose run --build --rm assets  # regenerate the brand assets
```

`--build` is not optional on those one-shot jobs: `docker compose run` reuses an
existing image rather than rebuilding it, and a stale image reports on a tree
that is no longer there.

The webview served by the `dev`, `staging` and `prod` profiles has no Tauri
backend, so the shell renders and nothing scans. Those profiles are for
frontend work, not QA. The `rust` profile names the portable crates explicitly
and excludes `src-tauri`, which cannot compile off macOS; the real gate is
`just check` on a Mac.

## Stack

| Layer | Choice |
| --- | --- |
| Backend | Rust workspace, arena tree (48-byte node + interned name blob), single-writer builder |
| Desktop shell | Tauri v2 |
| Reporting store | None in this release — reports run on the live in-memory tree |
| IPC | Arrow IPC bytes for bulk data; JSON only for small DTOs |
| Typed client | `tauri-specta` generated bindings, verified by test |
| Frontend | React 19 + TypeScript + Vite |
| Styling | Tailwind CSS v4 (`@theme`) + shadcn/ui |
| Tables | TanStack Table v8 + TanStack Virtual v3 |
| Charts | Hand-written: canvas for treemap, icicle and sunburst; plain elements for the report bars |

## Acceptance targets

| Measure | Target |
| --- | --- |
| Cold full-detail CLI scan, ~69M entries | < 12 min, < 5.0 GiB peak RSS |
| Warm scan, same root | < 4 min |
| ~2M-entry local fixture | < 25 s cold |
| Correctness vs `du` on a quiescent fixture | allocated within 1%; exact logical total and entry/error counts |
| Treemap navigation | p95 input-to-paint < 50 ms |
| Cancel | UI acknowledges < 100 ms; workers stop p95 < 200 ms |

These are the targets the design is held to. They are measured by hand on a
quiescent host — `just check` runs no benchmark, so **CI does not enforce
them**, and none should be read as a verified property of v0.1.0.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/` | Rust libraries and the supported diagnostic CLI |
| `src-tauri/` | Tauri v2 desktop shell and native command boundary |
| `src/` | React + TypeScript frontend |
| `skills/` | Project-carried agent skills |
| `Justfile`, `scripts/` | Local task surface and repository validators |
| `rush.sh` | Environment-aware entry point: run, build, package, containers |
| `Dockerfile`, `docker-compose.yml`, `docker/` | Container profiles for the portable work |
| `.env.*.example` | Per-environment templates; copy, drop the suffix, edit |
| `src-tauri/tauri.*.conf.json` | Per-profile config merged over the base |
| `.github/` | macOS CI |
| `LICENSE`, `NOTICE`, `LICENSING.md` | AGPL-3.0-only text, attribution, dual-licensing policy |

`.agents/` holds the installer-managed `rust-skills` payload. It is downloaded
rather than authored here, so it is untracked; `skills/rust-skills` and
`.claude/skills/rust-skills` are tracked symlinks into it and dangle in a fresh
clone until the skills are installed. Nothing in the build reads them.

`.settings/` is gitignored local developer material and is never cloned
with this repository.

## License

Stellar RDIRSTAT is dual-licensed.

- **Open source:** [GNU AGPL-3.0-only](LICENSE). Free to use, modify, and
  redistribute, provided derivatives stay AGPL-3.0-only, the attribution in
  [NOTICE](NOTICE) survives, and users you serve over a network can get the
  complete source.
- **Commercial:** for closed-source or proprietary products, hosted services that
  do not publish source, or relaxed attribution, buy a commercial license.
  Contact **Joshua Belke — joshbelke@gmail.com** ([@jbelke](https://github.com/jbelke)).

[LICENSING.md](LICENSING.md) explains both tracks, the contributor CLA, and why
third-party copyleft code is never pasted into this repository.

```
SPDX-License-Identifier: AGPL-3.0-only
Copyright (C) 2026 Joshua Belke
```
