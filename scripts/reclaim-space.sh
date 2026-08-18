#!/usr/bin/env bash
#
# reclaim-space.sh — re-runnable disk reclaim for a Rust-heavy dev machine.
#
# Dry-run by default. Nothing is deleted until you pass --apply.
#
#   ./scripts/reclaim-space.sh                    # show what a default run would free
#   ./scripts/reclaim-space.sh --apply            # do it
#   ./scripts/reclaim-space.sh rust --rust-level stale --apply
#   ./scripts/reclaim-space.sh vms                # dormant container VMs (opt-in)
#
# The design rule: never make the next build cold if a cheaper option exists.
# That is why `rust` defaults to sweeping only incremental caches, and why
# whole target/ dirs are only removed once they have gone untouched for a while.

set -euo pipefail

# ---------------------------------------------------------------- configuration

APPLY=0
ASSUME_YES=0
QUIET=0
RUST_AGE=14
RUST_LEVEL=incremental       # incremental | stale | nuke
SCAN_DEPTH=7
SELECTED=()
ROOTS=()

REPO_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || pwd)"

usage() {
  cat <<'USAGE'
reclaim-space.sh [options] [group ...]

Groups (default: pkg rust browsers apps)
  pkg        package-manager download caches (npm, pnpm, yarn, bun, uv, go,
             gradle, maven, cargo registry sources)
  rust       Rust build artifacts — see --rust-level
  browsers   downloaded test browsers (playwright, puppeteer, cypress)
  apps       application caches (Chrome, hermit, JetBrains, Codex, electron)
  all        all of the above
  vms        dormant container-runtime disk images (Docker/colima/OrbStack).
             Opt-in only — never included in `all`. Refuses to touch a
             runtime that is currently running.

Options
  --apply              actually delete (default: dry run)
  -y, --yes            skip the confirmation prompt when applying
  --rust-level LEVEL   incremental | stale | nuke        (default: incremental)
                         incremental — only target/*/incremental. Costs one
                                       non-incremental rebuild; dependency
                                       artifacts survive. Cheapest real win.
                         stale       — the above, plus whole target/ roots
                                       untouched for --rust-age days, plus
                                       unused rustup toolchains.
                         nuke        — every target/ root found, including the
                                       one you are sitting in.
  --rust-age DAYS      staleness threshold for target/ roots (default: 14)
  --roots PATH[:PATH]  where to scan for Cargo target dirs
                       (default: this repo — scanning a whole volume is slow)
  --scan-depth N       max find depth under each root (default: 7)
  -q, --quiet          only print the summary
  -h, --help           this

Exit status is 0 on success even when nothing was found.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)       APPLY=1; shift ;;
    -y|--yes)      ASSUME_YES=1; shift ;;
    -q|--quiet)    QUIET=1; shift ;;
    --rust-age)    RUST_AGE="${2:?--rust-age needs a number}"; shift 2 ;;
    --rust-level)  RUST_LEVEL="${2:?--rust-level needs a value}"; shift 2 ;;
    --scan-depth)  SCAN_DEPTH="${2:?--scan-depth needs a number}"; shift 2 ;;
    --roots)       IFS=':' read -r -a ROOTS <<< "${2:?--roots needs a path}"; shift 2 ;;
    -h|--help)     usage; exit 0 ;;
    -*)            echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)             SELECTED+=("$1"); shift ;;
  esac
done

case "$RUST_LEVEL" in
  incremental|stale|nuke) ;;
  *) echo "--rust-level must be incremental, stale, or nuke" >&2; exit 2 ;;
esac

[[ ${#SELECTED[@]} -eq 0 ]] && SELECTED=(pkg rust browsers apps)
[[ ${#ROOTS[@]}  -eq 0 ]] && ROOTS=("$REPO_ROOT")

want() {
  local g
  for g in "${SELECTED[@]}"; do
    [[ "$g" == "$1" ]] && return 0
    [[ "$g" == "all" && "$1" != "vms" ]] && return 0
  done
  return 1
}

# ---------------------------------------------------------------------- output

BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; RESET=''
if [[ -t 1 ]]; then
  BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'
  GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
fi

TOTAL_KB=0
declare -a PLAN_LINES=()

human() {  # KiB -> human
  awk -v k="$1" 'BEGIN{
    split("KB MB GB TB", u, " ")
    i = 1
    while (k >= 1024 && i < 4) { k /= 1024; i++ }
    printf (k >= 100 || i == 1) ? "%.0f %s" : "%.1f %s", k, u[i]
  }'
}

say()     { [[ $QUIET -eq 1 ]] || printf '%s\n' "$*"; }
section() { [[ $QUIET -eq 1 ]] || printf '\n%s%s%s\n' "$BOLD" "$*" "$RESET"; }

size_kb() { du -sk "$1" 2>/dev/null | awk 'NR==1{print $1+0}' || echo 0; }

# Refuse to delete anything that looks like a mistake. A bug in a path variable
# should cost nothing rather than the home directory.
safe_to_remove() {
  local p="$1"
  case "$p" in
    ""|"/"|"$HOME"|"$HOME/"|/Users|/Volumes|/System*|/Library|/usr*|/etc*|/bin*)
      return 1 ;;
  esac
  [[ "$p" != /* ]] && return 1            # absolute paths only
  [[ ${#p} -lt 12 ]] && return 1          # nothing shallow
  [[ "$p" == *".."* ]] && return 1
  return 0
}

# remove LABEL PATH [PATH...]
# Sizes every path, records the plan, and deletes only under --apply.
remove() {
  local label="$1"; shift
  local kb=0 found=0 p sz
  for p in "$@"; do
    [[ -e "$p" ]] || continue
    if ! safe_to_remove "$p"; then
      say "  ${RED}refused${RESET} $p (failed safety check)"
      continue
    fi
    sz=$(size_kb "$p"); kb=$((kb + sz)); found=1
  done
  [[ $found -eq 0 ]] && return 0
  [[ $kb -eq 0 ]] && return 0

  TOTAL_KB=$((TOTAL_KB + kb))
  PLAN_LINES+=("$(printf '%10s  %s' "$(human "$kb")" "$label")")
  say "  $(printf '%10s' "$(human "$kb")")  $label"

  if [[ $APPLY -eq 1 ]]; then
    for p in "$@"; do
      [[ -e "$p" ]] || continue
      safe_to_remove "$p" || continue
      rm -rf -- "$p"
    done
  fi
}

# remove_via LABEL PATH COMMAND...
# Prefers a tool's own prune command (which may preserve live references) and
# falls back to removing the directory when the tool is not installed.
remove_via() {
  local label="$1" path="$2"; shift 2
  [[ -e "$path" ]] || return 0
  local kb; kb=$(size_kb "$path")
  [[ $kb -eq 0 ]] && return 0

  if command -v "$1" >/dev/null 2>&1; then
    if [[ $APPLY -eq 1 ]]; then
      "$@" >/dev/null 2>&1 || true
      local after; after=$(size_kb "$path")
      kb=$(( kb - after ))
      [[ $kb -lt 0 ]] && kb=0
    fi
    TOTAL_KB=$((TOTAL_KB + kb))
    local shown; shown="$(human "$kb")"
    [[ $APPLY -eq 0 ]] && shown="<=$shown"
    PLAN_LINES+=("$(printf '%10s  %s %s(via %s)%s' "$shown" "$label" "$DIM" "$*" "$RESET")")
    say "  $(printf '%10s' "$shown")  $label ${DIM}(via $*)${RESET}"
  else
    remove "$label" "$path"
  fi
}

# ------------------------------------------------------------ package managers

do_pkg() {
  section "Package-manager caches"

  # npm rebuilds _cacache on demand; rm is far faster than `npm cache clean`.
  remove "npm content-addressable cache" "$HOME/.npm/_cacache"
  remove "npx package cache"             "$HOME/.npm/_npx"

  # pnpm keeps one content-addressed store PER VOLUME, because node_modules
  # entries are hardlinks into it and hardlinks cannot cross a filesystem.
  # A store is never removed, only pruned -- and each one must be named
  # explicitly, since a bare `pnpm store prune` only touches the store that the
  # current working directory happens to resolve to.
  local s
  for s in "${PNPM_HOME:-$HOME/Library/pnpm}/store" /Volumes/*/.pnpm-store; do
    [[ -d "$s" ]] || continue
    remove_via "pnpm store (pruned)  ${DIM}$s${RESET}" "$s" pnpm store prune --store-dir "$s"
  done
  remove "pnpm cache-dir" "$HOME/Library/Caches/pnpm"

  remove "yarn cache"  "$HOME/Library/Caches/Yarn"
  remove_via "bun install cache" "$HOME/.bun/install/cache" bun pm cache rm
  remove_via "uv cache"          "$HOME/.cache/uv"          uv cache clean

  # `go clean` is the supported path but walks the tree; rm is equivalent.
  remove "go build cache"  "$HOME/Library/Caches/go-build"
  remove "go module cache" "$HOME/go/pkg/mod/cache/download"

  remove "gradle build cache" "$HOME/.gradle/caches/build-cache-1"

  # Cargo: the registry INDEX is expensive to refetch and small, so it stays.
  # `src` is just extracted `cache` tarballs; both regenerate from the index.
  remove "cargo registry sources"  "$HOME/.cargo/registry/src"
  remove "cargo registry tarballs" "$HOME/.cargo/registry/cache"
  remove "cargo git checkouts"     "$HOME/.cargo/git/checkouts"
}

# ----------------------------------------------------------------------- rust

# Emit every Cargo target root under the configured roots, one per line.
# A target root is identified by cargo's own markers rather than by name, which
# is what catches custom CARGO_TARGET_DIRs like target/agent-3.
find_target_roots() {
  local root
  for root in "${ROOTS[@]}"; do
    [[ -d "$root" ]] || continue
    find "$root" -maxdepth "$SCAN_DEPTH" \
         \( -name node_modules -o -name .git -o -name .Trash \
            -o -name Library -o -name .cargo -o -name .rustup \) -prune -o \
         -type f \( -name .rustc_info.json -o -name CACHEDIR.TAG \) -print 2>/dev/null \
    | while read -r marker; do
        local dir; dir="$(dirname "$marker")"
        # CACHEDIR.TAG is also used by non-cargo tools; require a cargo shape.
        if [[ -f "$dir/.rustc_info.json" ]] \
           || [[ -d "$dir/debug" ]] || [[ -d "$dir/release" ]] || [[ -d "$dir/.fingerprint" ]]; then
          printf '%s\n' "$dir"
        fi
      done
  done | sort -u
}

do_rust() {
  section "Rust build artifacts  ${DIM}(level: $RUST_LEVEL, roots: ${ROOTS[*]})${RESET}"

  local -a roots=()
  while IFS= read -r line; do [[ -n "$line" ]] && roots+=("$line"); done < <(find_target_roots)

  if [[ ${#roots[@]} -eq 0 ]]; then
    say "  ${DIM}no cargo target dirs found — pass --roots to widen the scan${RESET}"
  fi

  local current_target=""
  [[ -f "$REPO_ROOT/Cargo.toml" || -d "$REPO_ROOT/target" ]] && current_target="$REPO_ROOT/target"

  local t
  for t in ${roots[@]+"${roots[@]}"}; do
    case "$RUST_LEVEL" in
      incremental)
        # Only the incremental compilation cache. Dependency rlibs survive, so
        # the next build is one non-incremental pass, not a cold rebuild.
        local inc
        for inc in "$t"/*/incremental; do
          [[ -d "$inc" ]] && remove "incremental cache  ${DIM}${inc#$REPO_ROOT/}${RESET}" "$inc"
        done
        ;;
      stale)
        # A whole target root goes only if nothing in it was touched recently.
        # When it does go, its incremental dirs go with it — counting them
        # separately would double-report the space.
        if [[ "$t" != "$current_target" ]] \
           && [[ -z "$(find "$t" -maxdepth 2 -mtime "-$RUST_AGE" -print -quit 2>/dev/null)" ]]; then
          remove "stale target root (>${RUST_AGE}d)  ${DIM}${t#$REPO_ROOT/}${RESET}" "$t"
        else
          local inc
          for inc in "$t"/*/incremental; do
            [[ -d "$inc" ]] && remove "incremental cache  ${DIM}${inc#$REPO_ROOT/}${RESET}" "$inc"
          done
        fi
        ;;
      nuke)
        remove "target root  ${DIM}${t#$REPO_ROOT/}${RESET}" "$t"
        ;;
    esac
  done

  # Unused rustup toolchains. The active and default toolchains are never
  # candidates, and neither is anything pinned by a rust-toolchain file.
  if [[ "$RUST_LEVEL" != "incremental" ]] && command -v rustup >/dev/null 2>&1; then
    local keep active default pinned root
    active="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
    default="$(rustup toolchain list 2>/dev/null | awk '/\(default\)/{print $1}')"
    keep=$'\n'"$active"$'\n'"$default"$'\n'
    for root in "${ROOTS[@]}"; do
      while IFS= read -r f; do
        # .toml form: channel = "1.97.1"   legacy form: a bare "1.97.1"
        pinned="$(awk -F'"' '/^ *channel/{print $2}' "$f" 2>/dev/null)"
        [[ -z "$pinned" ]] && pinned="$(awk '/^ *[^#[:space:]]/{print $1; exit}' "$f" 2>/dev/null)"
        [[ -n "$pinned" ]] && keep+="$pinned"$'\n'
      done < <(find "$root" -maxdepth "$SCAN_DEPTH" -name 'rust-toolchain*' -type f 2>/dev/null)
    done

    local tc name
    for tc in "$HOME/.rustup/toolchains"/*; do
      [[ -d "$tc" ]] || continue
      name="$(basename "$tc")"
      # Match on the channel prefix so "1.88" pinned keeps "1.88-aarch64-...".
      if grep -qxF "$name" <<< "$keep" \
         || grep -qE "^${name%%-*}(-|$)" <<< "$keep" \
         || grep -q "^${name}" <<< "$keep"; then
        continue
      fi
      remove "unused toolchain  ${DIM}$name${RESET}" "$tc"
    done
  fi
}

# ------------------------------------------------------------------- browsers

do_browsers() {
  section "Downloaded test browsers  ${DIM}(re-downloaded on next test run)${RESET}"
  remove "playwright browsers"    "$HOME/Library/Caches/ms-playwright"
  remove "playwright-go browsers" "$HOME/Library/Caches/ms-playwright-go"
  remove "puppeteer browsers"     "$HOME/.cache/puppeteer"
  remove "cypress binaries"       "$HOME/Library/Caches/Cypress"
}

# ----------------------------------------------------------------------- apps

do_apps() {
  section "Application caches"
  remove "Chrome cache"     "$HOME/Library/Caches/Google/Chrome"
  remove "Brave cache"      "$HOME/Library/Caches/BraveSoftware" "$HOME/Library/Caches/com.brave.Browser"
  remove "hermit packages"  "$HOME/Library/Caches/hermit"
  remove "electron builds"  "$HOME/Library/Caches/electron"
  remove "node-gyp headers" "$HOME/Library/Caches/node-gyp"
  remove "typescript cache" "$HOME/Library/Caches/typescript"
  remove "codex runtimes"   "$HOME/.cache/codex-runtimes"
  remove_via "homebrew downloads" "$HOME/Library/Caches/Homebrew" brew cleanup -s

  # JetBrains keeps a cache per IDE version forever; only the newest of each
  # product is worth keeping.
  local jb="$HOME/Library/Caches/JetBrains"
  if [[ -d "$jb" ]]; then
    local product
    for product in $(ls -1 "$jb" 2>/dev/null | sed 's/[0-9.]*$//' | sort -u); do
      local -a versions=()
      while IFS= read -r v; do [[ -n "$v" ]] && versions+=("$jb/$v"); done \
        < <(ls -1 "$jb" 2>/dev/null | grep "^${product}[0-9]" | sort -V | sed '$d')
      [[ ${#versions[@]} -gt 0 ]] && remove "JetBrains $product (old versions)" "${versions[@]}"
    done
  fi

  # macOS installer/update leftovers.
  remove "app updater staging" \
    "$HOME/Library/Caches/com.hnc.Discord.ShipIt" \
    "$HOME/Library/Caches/com.github.GitHubClient.ShipIt" \
    "$HOME/Library/Caches/com.postmanlabs.agent.mac.ShipIt" \
    "$HOME/Library/Caches/lens-desktop-updater"
}

# ------------------------------------------------------------------------ vms

runtime_running() { pgrep -qf "$1" 2>/dev/null; }

do_vms() {
  section "Container-runtime disk images  ${DIM}(opt-in)${RESET}"
  say "  ${YELLOW}These hold real images and volumes, not caches. Removing one"
  say "  means re-pulling everything that runtime had.${RESET}"

  if runtime_running "com.docker.backend|Docker Desktop"; then
    say "  ${DIM}skipping Docker — it is running (use: docker system prune -a)${RESET}"
  else
    remove "Docker Desktop VM image" "$HOME/Library/Containers/com.docker.docker/Data/vms"
  fi

  if runtime_running "colima|limactl"; then
    say "  ${DIM}skipping colima — it is running (use: colima delete)${RESET}"
  else
    remove "colima VM image" "$HOME/.colima/_lima"
  fi

  if runtime_running "OrbStack|orbstack"; then
    say "  ${DIM}skipping OrbStack — it is running${RESET}"
  else
    local orb
    for orb in "$HOME/.orbstack" "$HOME/OrbStack" /Volumes/*/OrbStackData; do
      [[ -d "$orb" ]] && remove "OrbStack data  ${DIM}$orb${RESET}" "$orb"
    done
  fi
}

# ---------------------------------------------------------------------- driver

free_kb() { df -k "$HOME" | awk 'NR==2{print $4+0}'; }

BEFORE_FREE=$(free_kb)

say "${BOLD}reclaim-space${RESET}  ${DIM}groups: ${SELECTED[*]}${RESET}"
say "${DIM}free before: $(human "$BEFORE_FREE")${RESET}"

want pkg      && do_pkg
want rust     && do_rust
want browsers && do_browsers
want apps     && do_apps
want vms      && do_vms

say ""
if [[ $TOTAL_KB -eq 0 ]]; then
  say "${GREEN}Nothing to reclaim — already clean.${RESET}"
  exit 0
fi

if [[ $APPLY -eq 0 ]]; then
  printf '%s%s reclaimable%s  %s(dry run — nothing was deleted)%s\n' \
    "$BOLD" "$(human "$TOTAL_KB")" "$RESET" "$DIM" "$RESET"
  printf '%sRe-run with --apply to free it.%s\n' "$DIM" "$RESET"
  if [[ "$RUST_LEVEL" == "incremental" ]] && want rust; then
    printf '%sRust: level `incremental` only. Try --rust-level stale to also drop%s\n' "$DIM" "$RESET"
    printf '%starget roots untouched for %s days and unused rustup toolchains.%s\n' "$DIM" "$RUST_AGE" "$RESET"
  fi
  exit 0
fi

if [[ $ASSUME_YES -eq 0 ]]; then
  printf '\n%sAbout to delete %s.%s Continue? [y/N] ' "$BOLD" "$(human "$TOTAL_KB")" "$RESET"
  read -r reply
  case "$reply" in [yY]*) ;; *) echo "aborted"; exit 1 ;; esac
fi

AFTER_FREE=$(free_kb)
printf '\n%sFreed %s%s  %s(free: %s -> %s)%s\n' \
  "$GREEN" "$(human $((AFTER_FREE - BEFORE_FREE)))" "$RESET" \
  "$DIM" "$(human "$BEFORE_FREE")" "$(human "$AFTER_FREE")" "$RESET"

if ! command -v sccache >/dev/null 2>&1 && want rust; then
  printf '\n%sTip: with this much rebuilding, `cargo install sccache` plus%s\n' "$DIM" "$RESET"
  printf '%sRUSTC_WRAPPER=sccache makes clearing target/ nearly free —%s\n' "$DIM" "$RESET"
  printf '%sobject files come back from the shared cache instead of recompiling.%s\n' "$DIM" "$RESET"
fi
