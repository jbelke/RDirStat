# syntax=docker/dockerfile:1.7
#
#                     Stellar RDIRSTAT — container images
#
# WHAT THIS FILE DOES NOT DO: build the macOS application.
#
# Stellar RDIRSTAT is a Tauri desktop app whose shell links AppKit, Security
# .framework and NSFileManager. `src-tauri` does not compile off macOS, and the
# `.dmg` is assembled by `hdiutil`, which is part of macOS rather than a tool
# that can be installed. Any Dockerfile claiming to produce the installer is
# lying, so this one does not try — `./rush.sh dmg` runs natively on a Mac.
#
# What containers ARE good for here, and what each stage below is:
#
#   deps       the pnpm dependency graph, cached on the lockfile alone
#   dev        the Vite dev server, for working on the UI without a Mac
#   build      the production frontend bundle (`dist/`)
#   web        that bundle served by nginx — the `staging` and `prod` profiles
#   test       the frontend test suite plus the documentation checker
#   assets     regenerates the brand icons and fails if they drifted
#   rust       fmt, clippy and tests for the portable crates (NOT src-tauri)
#
# The webview in `dev`/`web` runs without a Tauri backend: every IPC command is
# unavailable, so the shell renders and nothing scans. That is the honest limit
# of the browser target and is why these profiles are for UI work, not QA.

ARG NODE_VERSION=24
ARG RUST_VERSION=1.97.1
ARG NGINX_VERSION=1.27

# ---------------------------------------------------------------------------
# Dependencies
# ---------------------------------------------------------------------------

FROM node:${NODE_VERSION}-bookworm-slim AS deps
WORKDIR /app

# Corepack reads `packageManager` from package.json, so the pnpm version in the
# image is the one the lockfile was written by rather than a second pin that
# can drift away from it.
COPY package.json pnpm-lock.yaml ./
RUN corepack enable && corepack prepare --activate

# A cache mount rather than a copied store: the image keeps only what
# node_modules needs, and a lockfile change re-resolves instead of re-fetching.
RUN --mount=type=cache,id=pnpm-store,target=/pnpm-store \
    pnpm config set store-dir /pnpm-store && \
    pnpm install --frozen-lockfile

# ---------------------------------------------------------------------------
# Vite dev server
# ---------------------------------------------------------------------------

FROM deps AS dev
WORKDIR /app
ENV NODE_ENV=development
# `--host` is required: bound to localhost the server is unreachable from
# outside the container's network namespace, which looks exactly like a crash.
EXPOSE 1420
CMD ["pnpm", "exec", "vite", "--host", "0.0.0.0", "--port", "1420"]

# ---------------------------------------------------------------------------
# Production frontend bundle
# ---------------------------------------------------------------------------

FROM deps AS build
WORKDIR /app
COPY tsconfig.json tsconfig.node.json vite.config.ts components.json index.html ./
COPY public ./public
COPY src ./src
RUN pnpm build

# ---------------------------------------------------------------------------
# The bundle, served
# ---------------------------------------------------------------------------

FROM nginx:${NGINX_VERSION}-alpine AS web
COPY docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY docker/security-headers.conf /etc/nginx/security-headers.conf
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 80
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD wget --spider -q http://127.0.0.1/ || exit 1

# ---------------------------------------------------------------------------
# Frontend tests and the documentation checker
# ---------------------------------------------------------------------------

FROM deps AS test
WORKDIR /app
COPY tsconfig.json tsconfig.node.json vite.config.ts components.json index.html ./
COPY scripts ./scripts
COPY public ./public
COPY src ./src
COPY README.md AGENTS.md CLAUDE.md LICENSING.md ./
CMD ["sh", "-c", "pnpm typecheck && pnpm test && node scripts/check-docs.mjs"]

# ---------------------------------------------------------------------------
# Brand assets
#
# The generator has no dependencies beyond node:zlib precisely so this stage
# can prove the committed icons are the ones the source describes — including
# the .icns, whose container is written in-process rather than by `iconutil`.
# Run `node scripts/generate-icons.mjs --check` here and drift fails the build.
# ---------------------------------------------------------------------------

FROM node:${NODE_VERSION}-bookworm-slim AS assets
WORKDIR /app
COPY scripts/generate-icons.mjs ./scripts/
COPY src-tauri/icons ./src-tauri/icons
COPY public ./public
CMD ["node", "scripts/generate-icons.mjs"]

# ---------------------------------------------------------------------------
# The portable Rust crates
#
# `crates/*` is platform-independent: the two macOS-specific paths in
# rdirstat-scan are behind `cfg(target_os = "macos")`. `src-tauri` is excluded
# from every command here, deliberately and by name, because it cannot build.
# ---------------------------------------------------------------------------

FROM rust:${RUST_VERSION}-slim-bookworm AS rust
WORKDIR /app

# Overrides rust-toolchain.toml, which pins the two apple-darwin targets. They
# are correct on a Mac and 300MB of unusable standard library here.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
ENV CARGO_TERM_COLOR=always

RUN rustup component add rustfmt clippy

COPY docker/rust-check.sh /usr/local/bin/rust-check
RUN chmod +x /usr/local/bin/rust-check
CMD ["rust-check"]
