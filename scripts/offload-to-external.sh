#!/usr/bin/env bash
#
# offload-to-external.sh — move bulky, relocatable data off the internal drive.
#
# Dry-run by default. Every move is copied with `ditto` and then verified — the
# structure, the sizes, the extended attributes and the ACLs are all compared
# before the source is removed — and every move is reversible with --revert.
#
#   ./scripts/offload-to-external.sh                 # show the plan
#   ./scripts/offload-to-external.sh --apply
#   ./scripts/offload-to-external.sh kaspad --apply  # just one target
#   ./scripts/offload-to-external.sh --revert --apply
#
# Two relocation mechanisms are used, chosen per target rather than uniformly:
#
#   symlink  the tool finds its data by a fixed path and has no reliable
#            config knob (kaspad). A symlink is invocation-independent.
#   config   the tool owns the location and will honour a setting (npm, pnpm).
#            Symlinking a cache dir that the tool deletes and recreates is
#            fragile; telling it where to write is not.
#
# What is deliberately NOT here: the pnpm store. See refuse_pnpm_store below.

set -euo pipefail

APPLY=0
ASSUME_YES=0
REVERT=0
FRESH=0
DEST_ROOT="${OFFLOAD_DEST:-/Volumes/tuf8tb/.offload}"
TARGETS=()

usage() {
  cat <<'USAGE'
offload-to-external.sh [options] [target ...]

Targets (default: all)
  kaspad       kaspad mainnet block data      (symlink)
  npm          npm _cacache                   (npm config set cache)
  pnpm-cache   pnpm cache-dir, NOT the store  (pnpm config set cache-dir)

Options
  --dest DIR     destination root (default: /Volumes/tuf8tb/.offload,
                 override with $OFFLOAD_DEST)
  --apply        actually move (default: dry run)
  --revert       move data back and undo the config/symlink change
  --fresh        for cache targets, discard instead of copying, then point the
                 tool at the new location so it refills there. Frees the space
                 immediately and permanently without an hours-long copy of
                 files that regenerate anyway. Refused for kaspad, which holds
                 real data, not a cache.
  -y, --yes      skip the confirmation prompt
  -h, --help     this

The pnpm store is intentionally not a target. It is hardlinked into every
node_modules on its own volume, so relocating it across volumes silently
downgrades pnpm to copying and costs more space than it saves. Prune it
instead: ./scripts/reclaim-space.sh pkg
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)   APPLY=1; shift ;;
    --revert)  REVERT=1; shift ;;
    --fresh)   FRESH=1; shift ;;
    -y|--yes)  ASSUME_YES=1; shift ;;
    --dest)    DEST_ROOT="${2:?--dest needs a path}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*)        echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)         TARGETS+=("$1"); shift ;;
  esac
done

[[ ${#TARGETS[@]} -eq 0 ]] && TARGETS=(kaspad npm pnpm-cache)

want() { local t; for t in "${TARGETS[@]}"; do [[ "$t" == "$1" ]] && return 0; done; return 1; }

BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; RESET=''
if [[ -t 1 ]]; then
  BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'
  GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
fi

say() { printf '%s\n' "$*"; }
human() {
  awk -v k="$1" 'BEGIN{
    split("KB MB GB TB", u, " "); i = 1
    while (k >= 1024 && i < 4) { k /= 1024; i++ }
    printf (k >= 100 || i == 1) ? "%.0f %s" : "%.1f %s", k, u[i]
  }'
}
size_kb() { du -sk "$1" 2>/dev/null | awk 'NR==1{print $1+0}' || echo 0; }

PLANNED_KB=0
FAILED=0

# ------------------------------------------------------------ destination check

DEST_VOLUME="$(df "$(dirname "$DEST_ROOT")" 2>/dev/null | awk 'NR==2{print $NF}')"

preflight() {
  if [[ ! -d "$(dirname "$DEST_ROOT")" ]]; then
    say "${RED}destination volume is not mounted: $(dirname "$DEST_ROOT")${RESET}"
    exit 1
  fi
  # A cross-volume move is the entire point; if dest resolves to the same
  # volume as $HOME nothing is gained and we should not pretend otherwise.
  local home_vol; home_vol="$(df "$HOME" | awk 'NR==2{print $NF}')"
  if [[ "$DEST_VOLUME" == "$home_vol" ]]; then
    say "${RED}destination is on the same volume as \$HOME ($DEST_VOLUME) — nothing would be freed${RESET}"
    exit 1
  fi
}

# require_space KB — refuse to start a move the destination cannot hold.
require_space() {
  local need="$1" avail
  avail="$(df -k "$(dirname "$DEST_ROOT")" | awk 'NR==2{print $4+0}')"
  if [[ "$need" -gt "$avail" ]]; then
    say "  ${RED}not enough room: need $(human "$need"), $(human "$avail") free on $DEST_VOLUME${RESET}"
    return 1
  fi
  return 0
}

# ------------------------------------------------------------------- move core

# manifest_of DIR — a comparable description of a tree, including the metadata a
# size-and-mtime diff cannot see.
#
# One NUL-terminated record per entry, sorted. The walk is NUL-delimited and so
# are the records: a newline in a filename would otherwise end a record early,
# and this manifest is what authorises `rm -rf` on the source. For each entry it
# records the kind, the relative path, and then:
#   file     size, extended-attribute names, ACL
#   dir      extended-attribute names, ACL
#   symlink  its target — never followed, so a link is compared as a link
#
# Known limit, and it is narrower than the record framing above: the ACL field
# comes from `ls -lde`, which is line-oriented, so for a name containing a
# newline the ACL column absorbs a fragment of the name. Both sides derive it
# the same way from the same names, so a faithful copy still matches and an
# unfaithful one still differs — the comparison is less specific for those
# entries, not wrong. Fixing it needs an ACL source that is not `ls`.
#
# com.apple.provenance is filtered out deliberately: macOS applies and reapplies
# it on its own, so it differs across a faithful copy and would produce false
# failures. Everything else is compared, including com.apple.ResourceFork, whose
# presence is how a resource fork shows up here.
#
# What this does NOT do is compare file CONTENTS byte for byte. It compares the
# shape and the metadata. `ditto` reports a non-zero exit if a copy fails, and
# that is checked separately; this manifest exists to catch the silent case
# where the copy "succeeded" but did not carry everything.
manifest_of() {
  local root="$1"
  ( cd "$root" 2>/dev/null || return 1
    find . -mindepth 1 \( -type f -o -type d -o -type l \) -print0 2>/dev/null \
      | LC_ALL=C sort -z \
      | while IFS= read -r -d '' p; do
          if [[ -L "$p" ]]; then
            printf 'l\t%s\t%s\0' "$p" "$(readlink -- "$p")"
          else
            local xa acl kind size
            xa="$(xattr -- "$p" 2>/dev/null | grep -v '^com\.apple\.provenance$' | LC_ALL=C sort | tr '\n' ',')"
            acl="$(ls -lde -- "$p" 2>/dev/null | sed -n '2,$p' | tr -d ' ' | tr '\n' ',')"
            if [[ -d "$p" ]]; then
              printf 'd\t%s\t%s\t%s\0' "$p" "$xa" "$acl"
            else
              size="$(stat -f %z -- "$p" 2>/dev/null)"
              printf 'f\t%s\t%s\t%s\t%s\0' "$p" "$size" "$xa" "$acl"
            fi
          fi
        done )
}

# move_verified SRC DST — copy, prove the copy carried everything, then drop the
# source.
#
# A bare `mv` across volumes that is interrupted leaves a partial destination and
# a deleted source with no way to tell, so the copy is proved before the source
# goes away.
#
# Two decisions here were paid for once already (nato-b6h.7, nato-b6h.8):
#
#   ditto, not rsync. `/usr/bin/rsync` on macOS 15 is openrsync, which rejects
#   --xattrs and --acls outright, so `rsync -a` silently drops every extended
#   attribute, ACL and resource fork. A modern rsync can carry them, but only if
#   the caller passes -aHAX --fileflags, and this script cannot assume which
#   rsync is on PATH. `ditto` ships with the OS and carries all of it. This is
#   the same call src-tauri/src/relocate.rs made, for the same reason.
#
#   The verification compares metadata, not just size and mtime. The previous
#   version proved the copy with `rsync -an --delete --itemize-changes`, which
#   compares size and mtime and is therefore blind to exactly the loss the
#   previous copy was causing: a destination stripped of every xattr reported as
#   identical, and the source was then deleted. A verifier that cannot see the
#   damage it is standing in front of is worse than no verifier, because it is
#   trusted.
move_verified() {
  local src="$1" dst="$2"
  mkdir -p "$(dirname "$dst")"

  say "  ${DIM}copying...${RESET}"
  if ! ditto --rsrc --extattr --acl -- "$src" "$dst"; then
    say "  ${RED}copy failed — source left intact${RESET}"
    return 1
  fi

  say "  ${DIM}verifying (structure, sizes, xattrs, ACLs)...${RESET}"

  # The manifests go to files, not variables. A bash variable cannot hold a NUL
  # byte, so "$(manifest_of ...)" would strip the very delimiter that makes a
  # record unambiguous, and command substitution would eat trailing ones too.
  local src_manifest dst_manifest empty=0
  src_manifest="$(mktemp -t offload-src)" || return 1
  dst_manifest="$(mktemp -t offload-dst)" || { rm -f -- "$src_manifest"; return 1; }

  if ! manifest_of "$src" > "$src_manifest"; then
    say "  ${RED}could not read source to verify${RESET}"
    rm -f -- "$src_manifest" "$dst_manifest"
    return 1
  fi
  if ! manifest_of "$dst" > "$dst_manifest"; then
    say "  ${RED}could not read destination to verify${RESET}"
    rm -f -- "$src_manifest" "$dst_manifest"
    return 1
  fi

  # Byte-for-byte on the raw streams. Comparing the rendered text would reopen
  # the hole the NUL delimiters close.
  if ! cmp -s "$src_manifest" "$dst_manifest"; then
    say "  ${RED}verification failed — destination does not match source; source left intact${RESET}"
    # Rendered for reading only, never for deciding.
    diff <(tr '\0' '\n' < "$src_manifest") <(tr '\0' '\n' < "$dst_manifest") \
      | head -20 | sed 's/^/    /'
    rm -f -- "$src_manifest" "$dst_manifest"
    return 1
  fi

  [[ -s "$src_manifest" ]] || empty=1
  rm -f -- "$src_manifest" "$dst_manifest"

  # An empty manifest on both sides means the walk found nothing. That is a
  # legitimate result for an empty source, but it is also what a silently failed
  # copy looks like, so say which one it was rather than deleting on an
  # unexamined match.
  if [[ $empty -eq 1 ]]; then
    say "  ${DIM}source was empty; nothing to verify, nothing removed${RESET}"
    rmdir "$src" 2>/dev/null || true
    return 0
  fi

  rm -rf -- "$src"
  return 0
}

# relocate_cache SRC DST — honour --fresh for a directory whose only cost of
# loss is re-downloading it later.
relocate_cache() {
  local src="$1" dst="$2"
  if [[ $FRESH -eq 1 ]]; then
    rm -rf -- "$src"
    mkdir -p "$dst"
    return 0
  fi
  move_verified "$src" "$dst"
}

confirm() {
  [[ $ASSUME_YES -eq 1 ]] && return 0
  printf '\n%sProceed?%s [y/N] ' "$BOLD" "$RESET"
  read -r reply
  case "$reply" in [yY]*) return 0 ;; *) say "aborted"; exit 1 ;; esac
}

# ---------------------------------------------------------------------- kaspad

KASPAD_SRC="$HOME/Library/Application Support/Kaspad/kaspa-mainnet"
KASPAD_DST="$DEST_ROOT/kaspad/kaspa-mainnet"

do_kaspad() {
  printf '\n'; say "${BOLD}kaspad mainnet data${RESET} ${DIM}(symlink)${RESET}"

  if pgrep -qf 'kaspad' 2>/dev/null; then
    say "  ${RED}kaspad is running — stop it first${RESET}"; FAILED=1; return 0
  fi
  if [[ $FRESH -eq 1 ]]; then
    say "  ${YELLOW}--fresh refused: this is block data, not a cache. Discarding it"
    say "  costs a full resync from the network. Copying it.${RESET}"
  fi

  if [[ $REVERT -eq 1 ]]; then
    if [[ ! -L "$KASPAD_SRC" ]]; then say "  ${DIM}not offloaded${RESET}"; return 0; fi
    say "  restore $(human "$(size_kb "$KASPAD_DST")") -> $KASPAD_SRC"
    [[ $APPLY -eq 0 ]] && return 0
    rm -f "$KASPAD_SRC"
    move_verified "$KASPAD_DST" "$KASPAD_SRC" || { FAILED=1; ln -s "$KASPAD_DST" "$KASPAD_SRC"; return 0; }
    say "  ${GREEN}restored${RESET}"
    return 0
  fi

  if [[ -L "$KASPAD_SRC" ]]; then say "  ${DIM}already offloaded -> $(readlink "$KASPAD_SRC")${RESET}"; return 0; fi
  if [[ ! -d "$KASPAD_SRC" ]]; then say "  ${DIM}nothing to move${RESET}"; return 0; fi

  local kb; kb=$(size_kb "$KASPAD_SRC")
  PLANNED_KB=$((PLANNED_KB + kb))
  say "  $(human "$kb")  $KASPAD_SRC"
  say "         -> $KASPAD_DST"
  say "  ${DIM}kaspad.conf stays on the internal drive so kaspad still finds it${RESET}"
  [[ $APPLY -eq 0 ]] && return 0

  require_space "$kb" || { FAILED=1; return 0; }
  move_verified "$KASPAD_SRC" "$KASPAD_DST" || { FAILED=1; return 0; }
  ln -s "$KASPAD_DST" "$KASPAD_SRC"
  say "  ${GREEN}moved and symlinked${RESET}"
}

# ------------------------------------------------------------------------- npm

NPM_SRC="$HOME/.npm/_cacache"
NPM_DST="$DEST_ROOT/npm/_cacache"

do_npm() {
  printf '\n'; say "${BOLD}npm cache${RESET} ${DIM}(npm config set cache)${RESET}"
  command -v npm >/dev/null 2>&1 || { say "  ${DIM}npm not installed${RESET}"; return 0; }

  local configured; configured="$(npm config get cache 2>/dev/null)"

  if [[ $REVERT -eq 1 ]]; then
    if [[ "$configured" != "$DEST_ROOT/npm" ]]; then say "  ${DIM}not offloaded${RESET}"; return 0; fi
    say "  restore $(human "$(size_kb "$NPM_DST")") -> $NPM_SRC"
    [[ $APPLY -eq 0 ]] && return 0
    move_verified "$NPM_DST" "$NPM_SRC" || { FAILED=1; return 0; }
    npm config delete cache >/dev/null 2>&1 || true
    say "  ${GREEN}restored${RESET}"
    return 0
  fi

  if [[ "$configured" == "$DEST_ROOT/npm" ]]; then say "  ${DIM}already offloaded -> $configured${RESET}"; return 0; fi
  [[ -d "$NPM_SRC" ]] || { say "  ${DIM}nothing to move${RESET}"; return 0; }

  local kb; kb=$(size_kb "$NPM_SRC")
  PLANNED_KB=$((PLANNED_KB + kb))
  say "  $(human "$kb")  $NPM_SRC"
  say "         -> $NPM_DST  ${DIM}(npm config set cache $DEST_ROOT/npm)${RESET}"
  [[ $FRESH -eq 1 ]] && say "  ${YELLOW}--fresh: discarded, not copied; refills on the external drive${RESET}"
  [[ $APPLY -eq 0 ]] && return 0

  [[ $FRESH -eq 1 ]] || require_space "$kb" || { FAILED=1; return 0; }
  relocate_cache "$NPM_SRC" "$NPM_DST" || { FAILED=1; return 0; }
  npm config set cache "$DEST_ROOT/npm"
  say "  ${GREEN}moved; npm cache now $DEST_ROOT/npm${RESET}"
}

# ------------------------------------------------------------------ pnpm cache

PNPM_SRC="$HOME/Library/Caches/pnpm"
PNPM_DST="$DEST_ROOT/pnpm-cache"

refuse_pnpm_store() {
  # Load-bearing comment: pnpm keeps one content-addressed store PER VOLUME
  # because node_modules entries are hardlinks into it, and hardlinks cannot
  # cross a filesystem boundary. Point store-dir at another volume and pnpm
  # silently degrades to copying every package into every project — the disk
  # usage goes UP. ~/Library/pnpm/store is live here: ~/huyang-apps,
  # ~/dyad-apps and ~/lobot-apps all link into it. Prune, never relocate.
  local store="$HOME/Library/pnpm/store"
  [[ -d "$store" ]] || return 0
  say "  ${YELLOW}not moving $store ($(human "$(size_kb "$store")")) — it is hardlinked"
  say "  into node_modules on this volume. Relocating it would make pnpm copy"
  say "  instead of link and cost more space. Prune it instead:"
  say "    ./scripts/reclaim-space.sh pkg --apply${RESET}"
}

do_pnpm_cache() {
  printf '\n'; say "${BOLD}pnpm cache-dir${RESET} ${DIM}(pnpm config set cache-dir)${RESET}"
  command -v pnpm >/dev/null 2>&1 || { say "  ${DIM}pnpm not installed${RESET}"; return 0; }

  local configured; configured="$(pnpm config get cache-dir 2>/dev/null)"

  if [[ $REVERT -eq 1 ]]; then
    if [[ "$configured" != "$PNPM_DST" ]]; then say "  ${DIM}not offloaded${RESET}"; return 0; fi
    say "  restore $(human "$(size_kb "$PNPM_DST")") -> $PNPM_SRC"
    [[ $APPLY -eq 0 ]] && return 0
    move_verified "$PNPM_DST" "$PNPM_SRC" || { FAILED=1; return 0; }
    pnpm config delete cache-dir >/dev/null 2>&1 || true
    say "  ${GREEN}restored${RESET}"
    return 0
  fi

  if [[ "$configured" == "$PNPM_DST" ]]; then
    say "  ${DIM}already offloaded -> $configured${RESET}"; refuse_pnpm_store; return 0
  fi
  [[ -d "$PNPM_SRC" ]] || { say "  ${DIM}nothing to move${RESET}"; refuse_pnpm_store; return 0; }

  local kb; kb=$(size_kb "$PNPM_SRC")
  PLANNED_KB=$((PLANNED_KB + kb))
  say "  $(human "$kb")  $PNPM_SRC"
  say "         -> $PNPM_DST  ${DIM}(tarball + metadata cache; no hardlinks)${RESET}"
  [[ $FRESH -eq 1 ]] && say "  ${YELLOW}--fresh: discarded, not copied; refills on the external drive${RESET}"
  refuse_pnpm_store
  [[ $APPLY -eq 0 ]] && return 0

  [[ $FRESH -eq 1 ]] || require_space "$kb" || { FAILED=1; return 0; }
  relocate_cache "$PNPM_SRC" "$PNPM_DST" || { FAILED=1; return 0; }
  pnpm config set cache-dir "$PNPM_DST"
  say "  ${GREEN}moved; pnpm cache-dir now $PNPM_DST${RESET}"
}

# ---------------------------------------------------------------------- driver

preflight

free_kb() { df -k "$HOME" | awk 'NR==2{print $4+0}'; }
BEFORE=$(free_kb)

say "${BOLD}offload-to-external${RESET}  ${DIM}targets: ${TARGETS[*]}${RESET}"
say "${DIM}dest: $DEST_ROOT  (volume $DEST_VOLUME, $(human "$(df -k "$(dirname "$DEST_ROOT")" | awk 'NR==2{print $4+0}')") free)${RESET}"
say "${DIM}internal free: $(human "$BEFORE")${RESET}"
[[ $REVERT -eq 1 ]] && say "${YELLOW}mode: revert${RESET}"

if [[ $APPLY -eq 1 && $ASSUME_YES -eq 0 ]]; then
  # Show the plan before asking, by re-running the read-only half.
  APPLY=0
  want kaspad     && do_kaspad
  want npm        && do_npm
  want pnpm-cache && do_pnpm_cache
  say ""
  [[ $PLANNED_KB -eq 0 ]] && { say "${GREEN}Nothing to do.${RESET}"; exit 0; }
  say "${BOLD}$(human "$PLANNED_KB") would move to $DEST_VOLUME${RESET}"
  confirm
  APPLY=1
  PLANNED_KB=0
fi

want kaspad     && do_kaspad
want npm        && do_npm
want pnpm-cache && do_pnpm_cache

say ""
if [[ $APPLY -eq 0 ]]; then
  if [[ $PLANNED_KB -eq 0 ]]; then
    say "${GREEN}Nothing to do.${RESET}"
  else
    say "${BOLD}$(human "$PLANNED_KB") would move to $DEST_VOLUME${RESET}  ${DIM}(dry run)${RESET}"
    say "${DIM}Re-run with --apply.${RESET}"
  fi
  exit 0
fi

AFTER=$(free_kb)
say "${GREEN}internal free: $(human "$BEFORE") -> $(human "$AFTER")${RESET}"
if [[ $FAILED -eq 1 ]]; then
  say "${RED}one or more targets did not complete — see above${RESET}"
  exit 1
fi
