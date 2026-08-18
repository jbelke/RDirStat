#!/usr/bin/env bash
#
#                    Stellar RDIRSTAT — one way in
#
# Wraps the four toolchains this project needs (pnpm, cargo, the Tauri CLI and
# docker compose) behind one flag set, so "run it in staging" is a thing you
# type rather than a thing you look up.
#
# The single idea: an ENVIRONMENT selects two files, and everything else falls
# out of that.
#
#   .env.<environment>                     process environment for the run
#   src-tauri/tauri.<profile>.conf.json    Tauri config merged over the base
#
# Staging and production get different bundle identifiers that way, which is
# what lets them install side by side instead of overwriting each other's
# settings and snapshot store.
#
# Written for bash 3.2, because that is what /bin/bash is on macOS.

set -euo pipefail

readonly RUSH_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$RUSH_ROOT"

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[34m'
else
  C_RESET=""; C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""
fi

say()  { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok()   { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die()  { printf '%s fail%s %s\n' "$C_RED$C_BOLD" "$C_RESET" "$*" >&2; exit 1; }
note() { printf '%s     %s%s\n' "$C_DIM" "$*" "$C_RESET"; }

# ---------------------------------------------------------------------------
# Defaults, overridable by every flag below
# ---------------------------------------------------------------------------

ENVIRONMENT=""
PROFILE=""
ENV_FILE=""
DETACH=0
NO_BUILD=0
RELEASE=0
SKIP_VERIFY=0
DRY_RUN=0
VERBOSE=0
CLEAN_ALL=0

readonly DEFAULT_ENVIRONMENT="development"

usage() {
  cat <<'HELP'
Stellar RDIRSTAT — build, run and package.

USAGE
  ./rush.sh <command> [options]
  ./rush.sh <command> --help

ENVIRONMENTS
  development   dev    Debug build, verbose logs, its own bundle identifier
  staging       stage  Release build that installs BESIDE production
  production    prod   The build that ships
  <anything>           Any other name works too; see CUSTOM PROFILES below

THE APP (macOS, native — containers cannot build a .dmg)
  run              Launch the desktop app          (default env: development)
  dev              Shorthand for `run -e development`
  staging          Shorthand for `run -e staging`
  prod             Shorthand for `run -e production`
  build            Compile the app bundle, no installer
  dmg              Build the branded, installable .dmg
  icons            Regenerate every brand asset from one source file
  verify           Assert the committed icons are still the branded ones
  check            The full native quality gate (`just check`)

CONTAINERS (the frontend, the portable crates, the asset pipeline)
  up [ENV]         Start the compose profile for ENV
  down [ENV]       Stop it and remove the containers
  logs [ENV]       Follow its logs
  ps               What is running
  compose ...      Anything else, passed through with the env file applied

EVERYTHING ELSE
  doctor           What is installed, and what each environment resolves to
  env              Print the resolved configuration for an environment
  clean            Remove build output (`--all` also drops Docker volumes)

OPTIONS
  -e, --env NAME        Environment to resolve. Default: development
  -f, --env-file PATH   Use this file instead of the resolved one
  -p, --profile NAME    Config profile for the Tauri overlay and the compose
                        profile. Defaults to the environment name.
  -d, --detach          Containers: run in the background
      --no-build        Containers: skip the image rebuild
      --release         run/build: optimised instead of debug
      --skip-verify     dmg: package even if the branding check fails
      --all             clean: also remove Docker volumes and node_modules
  -n, --dry-run         Print the commands instead of running them
  -v, --verbose         Echo each command as it runs
  -h, --help            This text
      --version         Print the project version

HOW AN ENVIRONMENT RESOLVES
  Environment file, first that exists:
    1.  the path given to --env-file
    2.  .env.<environment>            yours, git-ignored
    3.  .env.<environment>.example    the tracked template (prints a warning)
    4.  .env                          the shared local file

  Tauri config:
    src-tauri/tauri.conf.json, with src-tauri/tauri.<profile>.conf.json merged
    over it when that file exists.

  Variables from the environment file are exported into the child process. The
  app treats a real environment variable as higher precedence than the .env it
  reads on its own, so this wins without deleting anything.

CUSTOM PROFILES
  --env and --profile are separate on purpose. `--env` picks the values;
  `--profile` picks the build shape.

    ./rush.sh dmg -e production -p staging     production values, staging bundle id
    ./rush.sh run -e demo                      .env.demo + tauri.demo.conf.json
    ./rush.sh up -e staging -p prod            staging values, prod compose profile

  A new environment needs nothing but the files:
    cp .env.staging.example .env.demo
    cp src-tauri/tauri.staging.conf.json src-tauri/tauri.demo.conf.json

EXAMPLES
  ./rush.sh dev                      Work on the app
  ./rush.sh dmg                      Build the installer for production
  ./rush.sh dmg -e staging           ... the one that installs alongside it
  ./rush.sh up dev                   Vite dev server in Docker, port 1420
  ./rush.sh up staging -d            Built bundle behind nginx, detached
  ./rush.sh compose run --rm rust    The portable-crate gate, in a container
  ./rush.sh doctor                   Why is it not working

WHAT DOCKER DOES NOT DO
  It does not build the macOS app or the .dmg, and no Dockerfile can: the shell
  links AppKit and Security.framework, and `hdiutil` is part of macOS. Use
  `./rush.sh dmg` on a Mac. See the header of ./Dockerfile.
HELP
}

# ---------------------------------------------------------------------------
# Plumbing
# ---------------------------------------------------------------------------

# Runs a command, or prints it under --dry-run. Every mutating action goes
# through here so --dry-run is honest rather than approximately honest.
run_cmd() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf '%s+ %s%s\n' "$C_DIM" "$*" "$C_RESET"
    return 0
  fi
  if [ "$VERBOSE" -eq 1 ]; then
    printf '%s+ %s%s\n' "$C_DIM" "$*" "$C_RESET"
  fi
  "$@"
}

have() { command -v "$1" >/dev/null 2>&1; }

require() {
  have "$1" || die "$1 is not installed. \`./rush.sh doctor\` lists what this project needs."
}

# Expands the aliases people actually type.
canonical_environment() {
  case "$1" in
    dev|develop|development) printf 'development' ;;
    stage|staging)           printf 'staging' ;;
    prod|production)         printf 'production' ;;
    *)                       printf '%s' "$1" ;;
  esac
}

# Resolves ENVIRONMENT, PROFILE and ENV_FILE. Idempotent; safe to call twice.
resolve_environment() {
  [ -n "$ENVIRONMENT" ] || ENVIRONMENT="$DEFAULT_ENVIRONMENT"
  ENVIRONMENT="$(canonical_environment "$ENVIRONMENT")"
  [ -n "$PROFILE" ] || PROFILE="$ENVIRONMENT"
  PROFILE="$(canonical_environment "$PROFILE")"

  if [ -n "$ENV_FILE" ]; then
    [ -f "$ENV_FILE" ] || die "no such environment file: $ENV_FILE"
    return
  fi

  local candidate
  for candidate in ".env.$ENVIRONMENT" ".env.$ENVIRONMENT.example" ".env"; do
    if [ -f "$candidate" ]; then
      ENV_FILE="$candidate"
      break
    fi
  done

  case "$ENV_FILE" in
    *.example)
      warn "using the template $ENV_FILE — it is tracked in git, so do not put anything"
      warn "machine-specific or secret in it. Make it yours:"
      warn "    cp $ENV_FILE .env.$ENVIRONMENT"
      ;;
    "")
      warn "no environment file for '$ENVIRONMENT'; continuing with the ambient environment."
      ;;
  esac
}

# Sources the environment file into this process, exporting as it goes.
#
# This EXECUTES the file. That is how every dotenv loader works and is fine for
# a file you wrote, which is exactly why .env.<environment> is git-ignored and
# only the .example templates are tracked.
load_environment() {
  [ -n "$ENV_FILE" ] || return 0
  set -a
  # shellcheck disable=SC1090
  . "./$ENV_FILE"
  set +a
}

tauri_overlay() {
  local path="src-tauri/tauri.$PROFILE.conf.json"
  [ -f "$path" ] && printf '%s' "$path"
}

# The workspace puts build output at the repository root, not under src-tauri/.
# Checked rather than assumed, because a stray CARGO_TARGET_DIR moves it.
bundle_dir() {
  local flavour="$1" candidate
  for candidate in "${CARGO_TARGET_DIR:-target}/$flavour/bundle" "src-tauri/target/$flavour/bundle"; do
    if [ -d "$candidate" ]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

project_version() {
  node -p "require('./package.json').version" 2>/dev/null || printf 'unknown'
}

# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

cmd_env() {
  resolve_environment
  local overlay; overlay="$(tauri_overlay || true)"
  printf '%sEnvironment%s  %s\n' "$C_BOLD" "$C_RESET" "$ENVIRONMENT"
  printf '%sProfile%s      %s\n' "$C_BOLD" "$C_RESET" "$PROFILE"
  printf '%sEnv file%s     %s\n' "$C_BOLD" "$C_RESET" "${ENV_FILE:-<none>}"
  printf '%sTauri config%s src-tauri/tauri.conf.json%s\n' "$C_BOLD" "$C_RESET" \
    "$([ -n "$overlay" ] && printf ' + %s' "$overlay")"
  if [ -n "$ENV_FILE" ]; then
    printf '\n%sValues%s\n' "$C_BOLD" "$C_RESET"
    # Keys only for anything that smells like a credential: this output gets
    # pasted into issues.
    grep -v '^[[:space:]]*#' "$ENV_FILE" | grep '=' | while IFS= read -r line; do
      local key="${line%%=*}"; key="${key#export }"
      case "$key" in
        *PASSWORD*|*SECRET*|*TOKEN*|*KEY*|*CERTIFICATE*)
          printf '  %s=%s<redacted>%s\n' "$key" "$C_DIM" "$C_RESET" ;;
        *) printf '  %s\n' "$line" ;;
      esac
    done
  fi
}

cmd_icons() {
  require node
  say "Regenerating brand assets"
  run_cmd node scripts/generate-icons.mjs
  if [ "$(uname -s)" != "Darwin" ]; then
    warn "icon.icns was left alone: iconutil is macOS-only. Re-run this on a Mac"
    warn "before building a release, or the app ships a stale Dock icon."
  fi
}

cmd_verify() {
  require node
  say "Verifying the committed brand assets"
  if run_cmd node scripts/generate-icons.mjs --check; then
    return 0
  fi
  return 1
}

cmd_check() {
  say "Native quality gate"
  if have just; then
    run_cmd just check
  else
    warn "just is not installed; running the gate step by step."
    run_cmd node scripts/check-docs.mjs
    run_cmd cargo fmt --all --check
    run_cmd cargo clippy --workspace --all-targets --all-features -- -D warnings
    run_cmd pnpm typecheck
    run_cmd cargo test --workspace --all-targets --all-features
    run_cmd pnpm test
    run_cmd pnpm build
  fi
}

cmd_run() {
  resolve_environment
  load_environment
  require pnpm

  local overlay; overlay="$(tauri_overlay || true)"
  say "Launching Stellar RDIRSTAT — $ENVIRONMENT"
  note "env file     ${ENV_FILE:-<none>}"
  note "tauri config src-tauri/tauri.conf.json${overlay:+ + $overlay}"
  [ -n "${RDIRSTAT_DATA_DIR:-}" ] && note "snapshots    $RDIRSTAT_DATA_DIR"

  # The app needs the directory to exist; creating it here means a fresh
  # checkout does not fail on the first scan.
  if [ -n "${RDIRSTAT_DATA_DIR:-}" ] && [ "$DRY_RUN" -eq 0 ]; then
    mkdir -p "$RDIRSTAT_DATA_DIR"
  fi

  local args="dev"
  [ "$RELEASE" -eq 1 ] && args="dev --release"
  # shellcheck disable=SC2086
  if [ -n "$overlay" ]; then
    run_cmd pnpm tauri $args --config "$overlay"
  else
    run_cmd pnpm tauri $args
  fi
}

cmd_build() {
  resolve_environment
  load_environment
  require pnpm

  local overlay; overlay="$(tauri_overlay || true)"
  say "Building the app bundle — $ENVIRONMENT"
  if [ -n "$overlay" ]; then
    run_cmd pnpm tauri build --no-bundle --config "$overlay"
  else
    run_cmd pnpm tauri build --no-bundle
  fi
}

# Reports whether this build can leave the machine that made it.
report_signing() {
  local identity="${APPLE_SIGNING_IDENTITY:-}"
  if [ -n "$identity" ]; then
    ok "APPLE_SIGNING_IDENTITY is set — the bundle will be Developer ID signed."
    if [ -z "${APPLE_ID:-}" ] || [ -z "${APPLE_PASSWORD:-}" ]; then
      warn "APPLE_ID / APPLE_PASSWORD are not set, so the .dmg is signed but NOT notarized."
      warn "Gatekeeper will still warn on a Mac that has not seen it before."
    fi
    return
  fi
  warn "APPLE_SIGNING_IDENTITY is not set."
  warn "The .dmg will be ad-hoc signed: it opens on this machine and Gatekeeper"
  warn "quarantines it everywhere else, usually reporting it as \"damaged\"."
  warn "That is a signing failure wearing a corruption message, not a bad build."
  warn "See the signing block in .env.production.example."
}

cmd_dmg() {
  resolve_environment
  load_environment

  [ "$(uname -s)" = "Darwin" ] || die "a .dmg can only be built on macOS; hdiutil is part of the OS."
  require pnpm
  have xcrun || warn "the Xcode command line tools were not found; the bundle step will likely fail."

  # The whole point of the exercise: refuse to ship an installer carrying
  # icons that are not ours.
  if [ "$SKIP_VERIFY" -eq 1 ]; then
    warn "--skip-verify: not checking that the icons are the branded ones."
  elif ! cmd_verify; then
    die "brand assets have drifted. Run \`./rush.sh icons\`, or \`--skip-verify\` to override."
  fi

  local overlay; overlay="$(tauri_overlay || true)"
  say "Packaging Stellar RDIRSTAT — $ENVIRONMENT"
  note "env file     ${ENV_FILE:-<none>}"
  note "tauri config src-tauri/tauri.conf.json${overlay:+ + $overlay}"
  report_signing

  if [ -n "$overlay" ]; then
    run_cmd pnpm tauri build --bundles dmg --config "$overlay"
  else
    run_cmd pnpm tauri build --bundles dmg
  fi

  [ "$DRY_RUN" -eq 1 ] && return 0

  local dir; dir="$(bundle_dir release)" || die "no bundle directory; the build produced nothing."
  local dmg
  dmg="$(find "$dir/dmg" -name '*.dmg' -maxdepth 1 2>/dev/null | sort | tail -1)"
  [ -n "$dmg" ] || die "the build finished but no .dmg was produced under $dir/dmg."

  say "Built"
  ok "$dmg"
  note "$(du -h "$dmg" | cut -f1) — $(basename "$dmg")"
  if have codesign; then
    if codesign -dv "$dir/macos/"*.app >/dev/null 2>&1; then
      note "signature: $(codesign -dv "$dir/macos/"*.app 2>&1 | grep -i '^Authority' | head -1 | sed 's/^Authority=//' || echo 'ad-hoc')"
    fi
  fi
  note "install: open the .dmg and drag the app to Applications"
}

# --- containers -------------------------------------------------------------

compose() {
  require docker
  resolve_environment
  local args=(docker compose)
  [ -n "$ENV_FILE" ] && args+=(--env-file "$ENV_FILE")
  args+=(--profile "$PROFILE")
  run_cmd "${args[@]}" "$@"
}

cmd_up() {
  local extra=(up)
  [ "$NO_BUILD" -eq 1 ] || extra+=(--build)
  [ "$DETACH" -eq 1 ] && extra+=(-d)
  resolve_environment
  say "Starting the '$PROFILE' compose profile"
  case "$PROFILE" in
    development|dev) note "Vite dev server → http://localhost:${DEV_PORT:-1420}" ;;
    staging)         note "built bundle    → http://localhost:${WEB_PORT:-4174}" ;;
    production|prod) note "built bundle    → http://localhost:${WEB_PORT:-4175}" ;;
  esac
  note "this serves the UI only: there is no Tauri backend in a browser, so"
  note "nothing scans. \`./rush.sh dev\` runs the real application."
  compose "${extra[@]}"
}

cmd_down() { compose down --remove-orphans; }
cmd_logs() { compose logs -f; }
cmd_ps()   { compose ps; }

# --- diagnostics ------------------------------------------------------------

report_tool() {
  local label="$1" binary="$2" version_cmd="$3" requirement="$4"
  if have "$binary"; then
    printf '  %s%-18s%s %s\n' "$C_GREEN" "$label" "$C_RESET" "$(eval "$version_cmd" 2>&1 | head -1)"
  else
    printf '  %s%-18s%s missing — %s\n' "$C_RED" "$label" "$C_RESET" "$requirement"
  fi
}

cmd_doctor() {
  printf '%sStellar RDIRSTAT %s%s\n\n' "$C_BOLD" "$(project_version)" "$C_RESET"

  say "Toolchain"
  report_tool "node" node "node --version" "required; >= 22.12"
  report_tool "pnpm" pnpm "pnpm --version" "required; corepack enable"
  report_tool "cargo" cargo "cargo --version" "required; https://rustup.rs"
  report_tool "just" just "just --version" "optional; \`./rush.sh check\` falls back"
  report_tool "docker" docker "docker --version" "optional; only the container profiles need it"

  printf '\n'
  say "macOS packaging"
  if [ "$(uname -s)" = "Darwin" ]; then
    report_tool "xcode-select" xcode-select "xcode-select -p" "required for a .dmg"
    report_tool "iconutil" iconutil "echo present" "required to regenerate icon.icns"
    report_tool "hdiutil" hdiutil "echo present" "required to assemble the .dmg"
    if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
      ok "APPLE_SIGNING_IDENTITY is set"
    elif have security && security find-identity -v -p codesigning 2>/dev/null | grep -q 'Developer ID'; then
      warn "a Developer ID identity is in the keychain but APPLE_SIGNING_IDENTITY is unset;"
      warn "the build will be ad-hoc signed until you export it."
    else
      warn "no Developer ID signing identity — builds will only open on this Mac."
    fi
  else
    warn "not macOS: \`build\`, \`dmg\` and \`run\` are unavailable here."
    note "the container profiles (up / compose) work on any platform."
  fi

  printf '\n'
  say "Brand assets"
  if node scripts/generate-icons.mjs --check >/dev/null 2>&1; then
    ok "the committed icons match scripts/generate-icons.mjs"
  else
    warn "the committed icons have drifted; run \`./rush.sh verify\` for the detail"
  fi

  printf '\n'
  say "Environments"
  local candidate name marker
  for name in development staging production; do
    marker="missing"
    for candidate in ".env.$name" ".env.$name.example"; do
      if [ -f "$candidate" ]; then marker="$candidate"; break; fi
    done
    local overlay="src-tauri/tauri.$name.conf.json"
    [ -f "$overlay" ] || overlay="(base config only)"
    printf '  %-12s %-28s %s\n' "$name" "$marker" "$overlay"
  done
  printf '\n'
  note "\`./rush.sh env -e staging\` shows exactly what one of them resolves to."
}

cmd_clean() {
  say "Removing build output"
  run_cmd rm -rf dist .vite
  local dir
  for dir in target/release/bundle target/debug/bundle src-tauri/target/release/bundle; do
    [ -d "$dir" ] && run_cmd rm -rf "$dir"
  done
  if [ "$CLEAN_ALL" -eq 1 ]; then
    run_cmd rm -rf node_modules .local
    if have docker; then
      say "Removing Docker volumes"
      resolve_environment
      run_cmd docker compose ${ENV_FILE:+--env-file "$ENV_FILE"} down -v --remove-orphans || true
    fi
  else
    note "compiled Rust artifacts kept; \`--all\` also drops node_modules and Docker volumes."
  fi
  ok "clean"
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

[ $# -gt 0 ] || { usage; exit 0; }

COMMAND="$1"; shift

# `up dev`, `logs staging` — the environment as a bare second word, because
# typing `-e` for the only argument a command takes is a tax.
case "$COMMAND" in
  up|down|logs|ps)
    if [ $# -gt 0 ]; then
      case "$1" in -*) ;; *) ENVIRONMENT="$1"; shift ;; esac
    fi
    ;;
esac

PASSTHROUGH=()
while [ $# -gt 0 ]; do
  case "$1" in
    -e|--env)        ENVIRONMENT="${2:?--env needs a value}"; shift 2 ;;
    -f|--env-file)   ENV_FILE="${2:?--env-file needs a value}"; shift 2 ;;
    -p|--profile)    PROFILE="${2:?--profile needs a value}"; shift 2 ;;
    -d|--detach)     DETACH=1; shift ;;
    --no-build)      NO_BUILD=1; shift ;;
    --release)       RELEASE=1; shift ;;
    --skip-verify)   SKIP_VERIFY=1; shift ;;
    --all)           CLEAN_ALL=1; shift ;;
    -n|--dry-run)    DRY_RUN=1; shift ;;
    -v|--verbose)    VERBOSE=1; shift ;;
    -h|--help)       usage; exit 0 ;;
    --version)       project_version; echo; exit 0 ;;
    --)              shift; PASSTHROUGH+=("$@"); break ;;
    *)               PASSTHROUGH+=("$1"); shift ;;
  esac
done

case "$COMMAND" in
  run)              cmd_run ;;
  dev)              ENVIRONMENT="${ENVIRONMENT:-development}"; cmd_run ;;
  staging)          ENVIRONMENT="staging"; cmd_run ;;
  prod|production)  ENVIRONMENT="production"; cmd_run ;;
  build)            cmd_build ;;
  dmg|package)      ENVIRONMENT="${ENVIRONMENT:-production}"; cmd_dmg ;;
  icons)            cmd_icons ;;
  verify)           cmd_verify ;;
  check)            cmd_check ;;
  up)               cmd_up ;;
  down)             cmd_down ;;
  logs)             cmd_logs ;;
  ps)               cmd_ps ;;
  compose)          compose "${PASSTHROUGH[@]+"${PASSTHROUGH[@]}"}" ;;
  doctor)           cmd_doctor ;;
  env)              cmd_env ;;
  clean)            cmd_clean ;;
  help|-h|--help)   usage ;;
  *)                printf '%sunknown command: %s%s\n\n' "$C_RED" "$COMMAND" "$C_RESET" >&2
                    usage >&2
                    exit 2 ;;
esac
