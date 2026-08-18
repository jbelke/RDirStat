#!/usr/bin/env bash
#
# The Linux-runnable half of the Rust quality gate.
#
# `src-tauri` links AppKit and Security.framework and cannot compile here, so
# every command below names the portable crates explicitly rather than using
# `--workspace` and excluding one member. Naming them is louder: a new crate is
# invisible to this gate until someone adds it, which is a review comment
# rather than a silent gap.
#
# The real gate is `just check` on a Mac. This is the part that can run in CI
# on a Linux runner, and it is honest about being a subset.
set -euo pipefail

CRATES=(
  rdirstat-core
  rdirstat-classify
  rdirstat-scan
  rdirstat-treemap
  rdirstat-remote
  rdirstat-cli
)

selector=()
for crate in "${CRATES[@]}"; do
  selector+=(--package "$crate")
done

echo "==> cargo fmt --check (whole workspace; formatting is platform-neutral)"
cargo fmt --all --check

echo "==> cargo clippy (${#CRATES[@]} portable crates; src-tauri is macOS-only)"
cargo clippy "${selector[@]}" --all-targets --all-features -- -D warnings

echo "==> cargo test (${#CRATES[@]} portable crates)"
cargo test "${selector[@]}" --all-targets --all-features

echo
echo "OK — portable crates pass. src-tauri and the .dmg still need a Mac:"
echo "     ./rush.sh check && ./rush.sh dmg"
