set shell := ["bash", "-euo", "pipefail", "-c"]

# Show the supported project commands.
default:
    @just --list

# Install the exact frontend dependency graph.
bootstrap:
    pnpm install --frozen-lockfile

# Check Rust formatting without modifying the worktree.
fmt:
    cargo fmt --all --check

# Apply Rust formatting.
fmt-fix:
    cargo fmt --all

# Run compiler lints and the TypeScript checker.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    pnpm typecheck

# Run Rust tests. nextest is preferred; Cargo is a deliberate local fallback.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo nextest --version >/dev/null 2>&1; then
      cargo nextest run --workspace --all-features
    else
      echo "cargo-nextest is not installed; falling back to cargo test" >&2
      cargo test --workspace --all-targets --all-features
    fi

# Run the frontend tests. Node's own runner, deliberately: adding vitest would
# pull a second test framework in for ten files that already pass under
# `node --test`. Requires `pnpm install` first, for apache-arrow.
test-js:
    pnpm test

# Validate the documentation index, local links, anchors, and project skill map.
check-docs:
    node scripts/check-docs.mjs

# Regenerate every brand asset from scripts/generate-icons.mjs.
icons:
    node scripts/generate-icons.mjs

# In the gate because the failure it catches is silent: nothing about a build
# tells you the Dock icon reverted to the Tauri v2 template.
#
# Assert the committed icons are still the generated, branded ones.
check-icons:
    node scripts/generate-icons.mjs --check

# Compatibility names used by the repository's documentation stewardship skill.
stellar-scan: check-docs

check-stellar: check-docs

check-skills: check-docs

# Typecheck and produce the frontend bundle.
frontend:
    pnpm build

# Run the deterministic local quality gate (no network required after bootstrap).
check: check-docs check-icons fmt lint test test-js frontend

# Report Rust and JavaScript dependency advisories/licenses. Requires cargo-deny.
audit:
    cargo deny check
    pnpm audit --audit-level high

# CI includes dependency reports in addition to the local deterministic gate.
ci: check audit

# Build the Rust workspace and production frontend.
build:
    cargo build --workspace --all-features
    pnpm build

# `./rush.sh dev` is the same thing with an environment file and the
# development bundle identifier applied; this stays as the unadorned form.
#
# Open the Tauri development application.
dev:
    pnpm tauri dev

# Build the branded, installable macOS disk image (production by default).
dmg:
    ./rush.sh dmg

# Build the staging disk image, which installs beside the release build.
dmg-staging:
    ./rush.sh dmg -e staging
