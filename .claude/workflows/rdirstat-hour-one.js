export const meta = {
  name: 'rdirstat-hour-one',
  description: 'Build a running RDirStat vertical slice in ~1 hour: contract-first, then disjoint-ownership parallel agents, then a single integration owner.',
  whenToUse: 'Run once, from a clean STELLAR-RDIRSTAT checkout, to turn .settings/docs/00-08 into a launchable Tauri app. Not for incremental feature work afterwards.',
  phases: [
    { title: 'Contract',  detail: 'git init + Tauri scaffold + frozen core types and command signatures' },
    { title: 'Build',     detail: '7 agents, disjoint file ownership, each with its own CARGO_TARGET_DIR' },
    { title: 'Integrate', detail: 'one owner of the build; loop until cargo + pnpm are green' },
    { title: 'Verify',    detail: 'du agreement, launch the app, rust-skills review' },
  ],
}

// ---------------------------------------------------------------------------
// Scope for the hour. Stated here so it is a decision, not an omission.
// ---------------------------------------------------------------------------
const IN_SCOPE = `
  Workspace + Tauri v2 shell that launches.
  48-byte arena Node, name blob, DirTotals side table, iterative post-order rollup.
  StdReader + bounded parallel engine + single builder. Symlink/mount/hardlink/
  sparse policy. Exclusion defaults. Progress events at 10 Hz. Cancel.
  Categorizer with longest-suffix-first matching and the macOS taxonomy.
  Squarified treemap + icicle + sunburst layout -> Arrow IPC -> Canvas 2D.
  Volume picker launch screen, virtualized tree table with %Bar, details panel.
  rdirstat-cli scan --stats for measurement without the webview.
`
const CUT = `
  DuckDB / Parquet / rdirstat-catalog and ALL report routes (docs 06). Its bundled
    C++ amalgamation taxes every rebuild; it cannot pay for itself inside an hour.
  getattrlistbulk BulkReader (docs 02) - unsafe, and gated on a benchmark we will
    not have run.
  Full Disk Access onboarding, Developer ID signing, notarization (docs 03).
  *.rdstat snapshot format and the QDirStat cache importer (docs 01).
  Cushion shading (docs 05 defers it behind a flat renderer meeting budget).
  Scan diffing and duplicate detection (docs 07 phase 7).
  The 69M-entry acceptance run. The Node size assertion ships; the volume gate
    does not. Do not claim a performance target this workflow did not measure.
`

// ---------------------------------------------------------------------------
// Disjoint file ownership. This is what makes 7 concurrent writers safe in one
// checkout: no path appears twice. An agent that edits outside its list is the
// single failure mode that turns this workflow into a merge conflict.
// ---------------------------------------------------------------------------
const UNITS = [
  {
    key: 'scan',
    label: 'crate:rdirstat-scan',
    owns: 'crates/rdirstat-scan/**',
    doc: '.settings/docs/02-SCANNER.md',
    task: `Implement rdirstat-scan: the DirReader trait and RawEntry exactly as .settings/docs/02
      specifies, StdReader, BOTH schedulers, the single builder that owns the arena and
      is the only assigner of NodeIds, and the ITERATIVE post-order rollup (do not
      recurse; depth is untrusted).

      Ship both readers in the order .settings/docs/02 fixes, and keep their relationship:
      - StdReader is the CORRECTNESS ORACLE (std::fs::read_dir + DirEntry::metadata,
        which is symlink_metadata on Unix, so it does not follow symlinks). Write it
        first. It is expected to be the slow baseline, and no faster path may disagree
        with it silently.
      - A bounded parallel engine (crossbeam bounded channels, exact
        pending-directory termination counter) runs the SAME reader behind the same
        one-directory contract. Worker count is configurable and recorded in output;
        do not default to num_cpus by reflex - a sequential disk workload turns into
        seek contention.
      - Land the differential test that both schedulers produce identical normalized
        entry sets and error classes over one quiescent fixture. That test is what
        makes the parallel path trustworthy, so it is not optional.
      Traversal policy is non-negotiable and each rule needs a test: no symlink
      following; do not cross device boundaries; count hard-linked content once per
      (dev,ino) with later entries contributing zero and carrying a flag; keep size
      and alloc separate; virtual direct-files group (no arena node); special files
      are zero-contribution leaves. Exclusions compiled once, first-match-wins, with
      the macOS defaults from .settings/docs/02 including /System/Volumes/Data.
      EACCES/EPERM mark a node unreadable and the scan CONTINUES - never propagate a
      per-directory error out of the scan. Relaxed AtomicU64 counters, one AtomicBool
      cancel checked at directory boundaries.`,
  },
  {
    key: 'classify',
    label: 'crate:rdirstat-classify',
    owns: 'crates/rdirstat-classify/**',
    doc: '.settings/docs/04-CLASSIFICATION.md',
    task: `Implement rdirstat-classify. The algorithm is longest-suffix-first: split the
      name on dots and try the longest multi-part suffix first ("tar.gz" before "gz"),
      case-sensitive map before the ASCII-lowercased map, glob patterns only as a
      fallback, then the executable-bit rule, then Uncategorized. Symlink wins over
      everything and is checked first.
      Author the default taxonomy FROM SCRATCH from public format knowledge - the
      QDirStat checkout is GPL-2.0 and .settings/docs/README fixes the rule that we reproduce
      behaviour, never transcribe its tables. Include the macOS categories .settings/docs/04
      names: Disk Images, macOS Bundles, Photo & Media Libraries, Xcode & Build Junk,
      Caches (node_modules, DerivedData), Virtual Machines, Container Images, Apple
      Junk (.DS_Store, ._*), RAW Photos.
      Hard constraint: classify(&str, Kind, mode) -> u8 with ZERO heap allocation.
      Stack buffer for the lowercase form. Bench it; the target is >20M names/sec
      single-threaded so it disappears next to syscall cost.`,
  },
  {
    key: 'treemap',
    label: 'crate:rdirstat-treemap',
    owns: 'crates/rdirstat-treemap/**',
    doc: '.settings/docs/05-UI.md',
    task: `Implement rdirstat-treemap: three layouts over ONE traversal of the frozen
      tree, emitting an Arrow RecordBatch with columns node, depth, x, y, w, h,
      category.
      1. Squarified treemap with the sub-pixel cutoff (min_px, default 3.0). The
         cutoff is load-bearing - it is what bounds a huge tree to a few thousand
         drawn tiles. Stop recursing, do not draw sub-pixel tiles.
      2. Icicle: same (depth, offset, extent) triple laid out as stacked horizontal
         bars, depth on y.
      3. Sunburst: the icicle in polar coordinates. It is a coordinate transform of
         the same triple, not a third algorithm.
      Flat fill only - NO cushion shading this hour. Put the generation and a schema
      version in the Arrow schema metadata. Deterministic output for a given
      (tree, kind, viewport, min_px) so it is testable and resize-stable.`,
  },
  {
    key: 'cli',
    label: 'crate:rdirstat-cli',
    owns: 'crates/rdirstat-cli/**',
    doc: '.settings/docs/02-SCANNER.md',
    task: `Implement rdirstat-cli - the measurement surface, not a throwaway. This is how
      the scanner gets profiled without a webview, so it must exist before anyone
      trusts a number.
      'rdirstat-cli scan <path> --stats --format json' prints observed entries,
      retained nodes, arena bytes, name-blob bytes, peak RSS, wall time, error count
      by class, and logical + allocated totals. Add --quantity {logical,allocated}
      and --top-down, matching pdu so results are directly comparable. Add
      --exclude, --cross-filesystems, --threads, --aggregate-below.
      Also add 'rdirstat-cli verify <path>' which scans and diffs the allocated total
      against 'du -sk <path>', exiting non-zero on disagreement. That single command
      is the highest-value test in the repo: it catches symlink following, hard-link
      double counting, dot-entry accounting, and device-boundary crossing at once.
      anyhow is allowed here (binary); the libraries use thiserror.`,
  },
  {
    key: 'tauri',
    label: 'src-tauri commands',
    owns: 'src-tauri/src/**',
    doc: '.settings/docs/01-ARCHITECTURE.md',
    task: `Implement the Tauri command layer - thin. It adapts library APIs and owns no
      scanner logic.
      AppState holds RwLock<Option<Arc<CompletedScan>>>; a command takes a read lock
      only long enough to clone the Arc. The state machine is
      Idle -> Scanning -> Cancelling|Finalizing -> Ready|Failed, one active scan,
      monotonic ScanId and TreeGeneration, and commands REJECT a stale generation
      rather than applying an old selection to a new tree.
      Commands per .settings/docs/01: scan_start, scan_cancel, children (cursor-paged, limit
      CLAMPED to 500), layout (kind = treemap|icicle|sunburst, returns Arrow IPC via
      tauri::ipc::Response), node_details (lstat on demand), path_of, list_volumes
      (statfs), reveal_in_finder, move_to_trash.
      move_to_trash goes through NSFileManager trashItem (use the 'trash' crate) so
      Finder Put Back works - NEVER fs::remove_file. Re-stat and verify (dev,ino)
      match the selection before moving, and reject the scan root and virtual groups.
      Blocking work goes to spawn_blocking, never the async command executor. Progress
      is a 10 Hz timer thread reading atomics and emitting scan:progress with a
      sequence number. Errors cross IPC as a discriminated CommandError - a
      PermissionDenied variant the frontend can route on, never a bare String.
      Wire tauri-specta and emit src/lib/bindings.ts.`,
  },
  {
    key: 'shell',
    label: 'frontend shell',
    owns: 'src/** except src/components/canvas/**',
    doc: '.settings/docs/05-UI.md',
    task: `Build the frontend shell. Tailwind CSS v4 via @tailwindcss/vite with a CSS-first
      @theme block - no tailwind.config.js. Dark surface first, light derived.
      Category colors are CSS vars (--cat-*); Rust sends indices, CSS resolves color.
      Use shadcn/ui. If 'pnpm dlx shadcn@latest init' fails or wants interaction, do
      NOT burn time on it - hand-write the four primitives you need (Table, Alert,
      Tooltip, ContextMenu) with Tailwind classes and move on.
      Deliver: macOS titlebar overlay preserving native traffic lights, with the
      breadcrumb in it; the volume-picker launch screen with segmented capacity bars;
      ONE reusable DataTable on TanStack Table v8 in manualSorting + manualPagination
      mode (Rust owns ordering - a client-side sort would need the whole tree in JS,
      which the IPC contract forbids) virtualized with TanStack Virtual v3; the %Bar
      share-of-parent cell and category chip; the details panel; the scan progress
      strip with a live Cancel; the unreadable-directories Alert.
      TanStack Query keyed by TreeGeneration. Zustand for selection/nav/layout toggle
      only. Import the generated bindings.ts - do not hand-write invoke wrappers.
      You do NOT own src/components/canvas/** . Import from it; do not create it.`,
  },
  {
    key: 'canvas',
    label: 'frontend canvas',
    owns: 'src/components/canvas/**',
    doc: '.settings/docs/05-UI.md',
    task: `Build the hierarchy canvas - the one performance-critical frontend component.
      A single <canvas>. Read the Arrow IPC ArrayBuffer from the 'layout' command with
      tableFromIPC (apache-arrow), pull typed arrays for x/y/w/h/node/category, and
      fillRect from them. NOT SVG, NOT DOM nodes, NOT Recharts - thousands of tiles as
      DOM elements will not hold the paint budget.
      Reject a batch whose schema-metadata generation is not the one requested. A
      malformed or truncated buffer must fail closed with a visible error, never a
      partial render.
      Interaction: hover tooltip with path + size; click to select; double-click to
      zoom with a nav stack; cmd/shift-click multi-select; right-click context menu; a
      segmented treemap|icicle|sunburst toggle. Hit-testing is a reverse linear scan
      over the typed arrays on mousemove - a few thousand elements is microseconds and
      a quadtree is premature.
      Export a selection callback so the shell can drive bidirectional tree<->canvas
      sync. Redraw on resize and navigate only, never per frame. Respect
      prefers-reduced-motion. Provide a keyboard-navigable largest-items list as the
      accessible equivalent - a canvas is opaque to a screen reader.`,
  },
]

const RULES = `
HARD RULES - violating either is what makes this workflow fail rather than finish:

1. YOU OWN ONLY: {OWNS}. Do not create, edit, or delete a single file outside it.
   Six other agents are writing this checkout concurrently. If you need something
   that is not yours, code against the frozen contract below and assume it exists.
2. Build with CARGO_TARGET_DIR={TARGET_DIR} on every cargo invocation. A shared
   target/ makes seven agents serialize on cargo's lock and lose the hour.
   Run 'cargo check' against your own crate as you go. Do not run a workspace-wide
   build; that belongs to the integration phase.

Other standing rules:
- Read {DOC} in full first. It is the contract, not background reading.
- Follow the /rust-skills guidelines and .settings/docs/08-RUST-PRACTICES.md: workspace lints
  with unsafe_code forbidden and unwrap_used denied, thiserror in libraries, newtypes
  over primitives, #[non_exhaustive] on public enums, no allocation in hot loops,
  OsStr/Path over String (macOS names are NFD bytes and may not be UTF-8).
- No unsafe. The one crate permitted it (the getattrlistbulk reader) is out of scope.
- Write tests as you go, with tempfile fixtures. Never write outside a TempDir.
- Do not stub or fake. If you cannot finish something, leave a compiling
  todo!()-free minimal real implementation and SAY SO in your report. A silent stub
  that compiles is worse than a named gap.
`

// ---------------------------------------------------------------------------
// Phase 1 - Contract. The only justified barrier in this workflow: nothing can
// be written in parallel until the shared types are frozen text everyone shares.
// ---------------------------------------------------------------------------
phase('Contract')
log('Freezing the type contract. Everything downstream codes against this text.')

const CONTRACT_SCHEMA = {
  type: 'object',
  required: ['core_types', 'command_signatures', 'workspace_notes'],
  properties: {
    core_types: { type: 'string', description: 'Verbatim contents of crates/rdirstat-core/src/lib.rs public API: Node, NodeId, NameRef, Kind, Tree, DirTotals, CompletedScan, ScanError, flags constants.' },
    command_signatures: { type: 'string', description: 'Verbatim Rust signatures of every #[tauri::command], plus the ScanProgress event payload and CommandError variants.' },
    workspace_notes: { type: 'string', description: 'Crate names, versions of every dependency added, the bundle id, and anything a sibling agent must not re-declare.' },
  },
}

const [scaffold, contract] = await parallel([
  () => agent(`You are preparing the STELLAR-RDIRSTAT repository at /Volumes/tuf8tb/STELLAR-RDIRSTAT.
    Read .settings/docs/07-BUILD-PHASES.md phase 0 first.

    YOU OWN: the repository root files, src-tauri/** EXCEPT src-tauri/src/**, src/**
    ONLY as far as the Vite/Tailwind/TS scaffold config goes (index.html,
    vite.config.ts, tsconfig*.json, package.json, src/main.tsx, src/index.css).
    You do NOT own crates/**, src-tauri/src/**, or any React component.

    1. 'git init' on main. Per this project's policy: never create a branch. Write a
       .gitignore covering target/, node_modules/, dist/, .agents/, and the five
       nested reference checkouts in .settings/reference-code/ (they contain their own .git -
       they must NOT be added as embedded repos). Keep .settings/reference-code/AGENTS.md
       tracked. Commit the existing .settings/docs/ and skills/ as the first commit.
    2. Scaffold Tauri v2 + React + TypeScript + pnpm into a TEMP directory, inspect
       what it generated, then move its files to the repo root. .settings/docs/, skills/,
       .settings/reference-code/, and .claude/ are never generator targets and must survive.
       'cargo tauri' 2.11.0 is already installed; 'create-tauri-app' is not, so
       'cargo install create-tauri-app --locked' first.
    3. Tailwind CSS v4 via @tailwindcss/vite, CSS-first @theme in src/index.css. No
       tailwind.config.js. Add deps: @tanstack/react-table, @tanstack/react-virtual,
       @tanstack/react-query, apache-arrow, zustand. Attempt a NON-INTERACTIVE
       shadcn init; if it needs input, skip it and note that in your report.
    4. Set a stable bundle identifier and a window title. In tauri.conf.json enable
       the macOS titlebar overlay style that PRESERVES native traffic lights.
    5. Verify 'cargo tauri dev' can start and 'pnpm build' runs, then stop.

    Report exactly what you scaffolded, every dependency version you pinned, and
    anything that failed. Do not paper over a failure - a sibling will build on it.`,
    { label: 'scaffold', phase: 'Contract' }),

  () => agent(`You are freezing the type contract for STELLAR-RDIRSTAT at
    /Volumes/tuf8tb/STELLAR-RDIRSTAT. Six agents will code against your output in
    parallel without being able to ask you questions, so this must be complete and
    internally consistent on the first try.

    Read .settings/docs/01-ARCHITECTURE.md IN FULL, then .settings/docs/02-SCANNER.md's reader contract.
    Follow /rust-skills and .settings/docs/08-RUST-PRACTICES.md.

    YOU OWN: the root Cargo.toml, rust-toolchain.toml, every crates/*/Cargo.toml,
    and crates/rdirstat-core/src/**. Nothing else. You do NOT own the other crates'
    src/, src-tauri/, or src/.

    1. Root Cargo.toml: [workspace] with members crates/* and src-tauri, the
       [workspace.lints] table from .settings/docs/08 (rust.unsafe_code = "forbid",
       clippy.unwrap_used = "deny", clippy.panic = "deny",
       clippy.cast_possible_truncation = "deny", pedantic warn), the release profile
       (lto thin, codegen-units 1), and [workspace.dependencies] pinning EVERY shared
       dep so siblings never declare a conflicting version. Pin: rayon,
       crossbeam-channel, thiserror, anyhow, tracing, tracing-subscriber, arrow,
       serde, specta, tauri-specta, tempfile, criterion, trash, libc.
    2. Create a Cargo.toml for rdirstat-scan, -classify, -treemap, -cli that each
       use 'lints.workspace = true' and 'workspace = true' dependencies.
    3. Write crates/rdirstat-core/src/ COMPLETELY - it is the shared contract:
       - #[repr(transparent)] NodeId(u32) with NONE sentinel and bit 31 reserved for
         tagged virtual direct-files groups; NameRef(u64) as 48-bit offset + 16-bit
         length behind a CHECKED accessor (no unchecked blob slicing anywhere).
       - #[repr(C)] Node laid out exactly as .settings/docs/01 specifies, plus
         'const _: () = assert!(size_of::<Node>() <= 48);' and an align assertion.
       - The name blob, the sorted Vec<NodeId> directory index + parallel
         Vec<DirTotals>, Kind, the flags bit constants, Tree, CompletedScan
         (carrying root, ScanId, TreeGeneration, counts, totals, errors, mutations),
         and the ScanError / QueryError enums with thiserror. #[non_exhaustive] on
         every public enum.
       - Byte-size formatting in DECIMAL SI, because macOS Finder does and a
         disagreeing number reads as a bug. Logical and allocated stay separate.
    4. In a doc comment at the top of core's lib.rs, write the FULL list of
       #[tauri::command] signatures from .settings/docs/01 verbatim, so every sibling sees the
       same wire contract.
    5. 'cargo check -p rdirstat-core' must pass. Use CARGO_TARGET_DIR=target/contract.

    Return core's exact public API text and the command signatures verbatim - your
    return value is pasted into six sibling prompts.`,
    { label: 'contract', phase: 'Contract', schema: CONTRACT_SCHEMA }),
])

if (!contract) {
  log('FATAL: the contract agent failed. Everything downstream would diverge. Stopping.')
  return { ok: false, reason: 'contract phase failed', scaffold }
}

const FROZEN = `
=========================== FROZEN CONTRACT ===========================
Authored by the contract agent. Treat as already-committed code. Do not
edit these types; code against them. If something you need is missing,
add it INSIDE YOUR OWN crate rather than changing core.

--- crates/rdirstat-core public API ---
${contract.core_types}

--- Tauri command signatures + event payloads ---
${contract.command_signatures}

--- Workspace / dependency notes ---
${contract.workspace_notes}
=======================================================================

--- Scaffold report (what actually exists on disk) ---
${scaffold || 'SCAFFOLD AGENT FAILED - assume no Tauri/Vite scaffold and report that you were blocked by it.'}
`

// ---------------------------------------------------------------------------
// Phase 2 - Build. Seven concurrent writers, disjoint paths, private target dirs.
// pipeline() would buy nothing here: the integration phase genuinely needs all
// seven before it can compile anything, so this barrier is the correct shape.
// ---------------------------------------------------------------------------
phase('Build')
log(`Fanning out ${UNITS.length} agents over disjoint paths. Cut this hour: DuckDB/reports, BulkReader, FDA/notarization, snapshots, cushion shading, diff/dupes.`)

const UNIT_SCHEMA = {
  type: 'object',
  required: ['files_written', 'checks_pass', 'gaps'],
  properties: {
    files_written: { type: 'array', items: { type: 'string' }, description: 'Every path created or edited.' },
    checks_pass: { type: 'boolean', description: 'True only if cargo check / tsc actually succeeded on your own unit.' },
    gaps: { type: 'string', description: 'What is missing, stubbed, or unverified. Be blunt; silence here becomes a bug in integration.' },
    integration_notes: { type: 'string', description: 'Anything the integration agent must know: assumptions made about siblings, deps added, config touched.' },
  },
}

const built = await parallel(UNITS.map((u, i) => () => agent(
  `Implement ${u.label} for STELLAR-RDIRSTAT at /Volumes/tuf8tb/STELLAR-RDIRSTAT.

${u.task}

${RULES.replace('{OWNS}', u.owns).replace('{TARGET_DIR}', `target/agent-${i}`).replace('{DOC}', u.doc)}

IN SCOPE for this hour: ${IN_SCOPE}
EXPLICITLY CUT - do not build these, do not import them: ${CUT}

${FROZEN}`,
  { label: u.key, phase: 'Build', schema: UNIT_SCHEMA },
)))

const ok = built.filter(Boolean)
const failed = UNITS.filter((_, i) => !built[i]).map(u => u.key)
if (failed.length) log(`Units that returned nothing: ${failed.join(', ')} - integration will have to cover or stub them.`)

const REPORTS = UNITS.map((u, i) => built[i]
  ? `### ${u.key} (${u.owns})\n  checks_pass: ${built[i].checks_pass}\n  files: ${(built[i].files_written || []).join(', ')}\n  gaps: ${built[i].gaps}\n  notes: ${built[i].integration_notes || 'none'}`
  : `### ${u.key} (${u.owns})\n  AGENT FAILED - nothing was returned. Assume this unit is absent or half-written.`
).join('\n\n')

// ---------------------------------------------------------------------------
// Phase 3 - Integrate. ONE owner of the build. Seven agents that each only
// type-checked their own crate will not compile together on the first try; this
// is the phase the hour actually turns on.
// ---------------------------------------------------------------------------
phase('Integrate')

let green = false
let attempt = 0
let lastReport = ''

while (!green && attempt < 3 && (!budget.total || budget.remaining() > 60_000)) {
  attempt += 1
  log(`Integration attempt ${attempt}/3${budget.total ? ` - ${Math.round(budget.remaining() / 1000)}k tokens left` : ''}`)

  const fix = await agent(
    `You are the SOLE owner of the build for STELLAR-RDIRSTAT at
    /Volumes/tuf8tb/STELLAR-RDIRSTAT. Seven agents just wrote disjoint parts of this
    app in parallel against a frozen contract. Each only type-checked its own crate.
    Your job is to make the whole thing compile and run, and to report honestly on
    what does not.

    You own EVERY file. Use the shared target/ directory (you are the only builder
    now). This is attempt ${attempt} of at most 3.
    ${attempt > 1 ? `\nPrevious attempt ended here - do not repeat what already failed:\n${lastReport}\n` : ''}

    In order, and do not skip ahead:
    1. 'cargo build --workspace' and fix every error. Prefer adapting the CALLER to
       the frozen core contract over changing core - siblings all assumed core.
       Version conflicts go to [workspace.dependencies].
    2. 'cargo clippy --workspace' - fix real defects. Downgrade a pedantic lint in
       the workspace table if it is noise; never silence unwrap_used or
       unsafe_code, which exist for reasons .settings/docs/08 states.
    3. 'cargo test --workspace'. A failing test is a finding, not something to
       delete. If a test encodes a wrong expectation, fix the test AND say so.
    4. 'pnpm install && pnpm tsc --noEmit && pnpm build'. Fix type and import
       errors. bindings.ts must match the actual commands - regenerate rather than
       hand-edit.
    5. 'cargo tauri build --debug' (or 'dev' with a timeout) far enough to prove the
       app starts.
    6. Wire anything left dangling between units: registered commands in the Tauri
       builder, the layout command actually calling rdirstat-treemap, the canvas
       actually mounted by the shell, the volume picker actually starting a scan.
    7. Commit to main with explicit paths - never 'git add -A', per this project's
       policy. Small commits.

    Report: what compiles, what runs, every test result verbatim, and a blunt list
    of what is broken or missing. Do NOT report success you did not observe.

    Unit reports from the seven builders:
    ${REPORTS}

    ${FROZEN}`,
    { label: `integrate-${attempt}`, phase: 'Integrate' },
  )

  lastReport = fix || 'integration agent returned nothing'
  green = !!fix && /cargo build[^\n]*(succeed|pass|clean|ok|green)|compiles cleanly|builds cleanly/i.test(fix)
  if (!green) log(`Attempt ${attempt} did not reach a clean build.`)
}

// ---------------------------------------------------------------------------
// Phase 4 - Verify. Independent checks, because an integration agent grading its
// own work is the least reliable signal in this workflow.
// ---------------------------------------------------------------------------
phase('Verify')

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['claim', 'holds', 'evidence'],
  properties: {
    claim: { type: 'string' },
    holds: { type: 'boolean' },
    evidence: { type: 'string', description: 'Actual command output or screenshot description. Not reasoning.' },
  },
}

const checks = await parallel([
  () => agent(`Independently verify SCAN CORRECTNESS for STELLAR-RDIRSTAT at
    /Volumes/tuf8tb/STELLAR-RDIRSTAT. You did not write this code; do not trust its
    author's report.
    Build a tempfile fixture tree containing: nested dirs, a symlink pointing
    outside the tree, a symlink loop, a hard-linked file appearing twice, a sparse
    file, an unreadable directory (chmod 000), and a non-UTF-8 filename.
    Run 'cargo run -p rdirstat-cli -- verify <fixture>' and 'du -sk <fixture>' and
    compare allocated totals. Then scan a real directory (~/Downloads or
    /Applications) and compare against 'du -sk' on the same path.
    Report the ACTUAL numbers. If they disagree, say by how much and which policy
    (symlink following, hard-link counting, dot-entry accounting, device crossing)
    most likely explains it. A disagreement is the finding - do not fix it, report
    it.`, { label: 'verify:du', phase: 'Verify', schema: VERDICT_SCHEMA }),

  () => agent(`Independently verify that the RDirStat APP RUNS at
    /Volumes/tuf8tb/STELLAR-RDIRSTAT. Launch it ('cargo tauri dev'), wait for the
    window, and screenshot it. Then drive the real user path: pick a volume or
    folder, watch the scan progress strip, confirm the tree table populates and
    scrolls, click a tile in the canvas and confirm the tree selection follows,
    toggle treemap/icicle/sunburst.
    Report what you actually SAW, with screenshots. If it does not launch, report
    the exact error. If a surface is blank or a toggle does nothing, that is the
    finding. Do not describe intended behaviour as observed behaviour.
    Do NOT exercise Move to Trash against real user files.`,
    { label: 'verify:runs', phase: 'Verify', schema: VERDICT_SCHEMA }),

  () => agent(`Review the Rust in STELLAR-RDIRSTAT at /Volumes/tuf8tb/STELLAR-RDIRSTAT
    against the /rust-skills guidelines and .settings/docs/08-RUST-PRACTICES.md. Read the
    skill's rules under .agents/skills/rust-skills/rules/ and cite rule IDs.
    Prioritise, in this order: any unwrap/expect/panic on a filesystem path;
    allocation inside the scan hot loop (anti-format-hot-path,
    anti-clone-excessive); String where OsStr/Path is required for correctness on
    macOS NFD non-UTF-8 names; a Mutex around the arena or held across an await
    (anti-lock-across-await); missing #[non_exhaustive]; any unsafe at all.
    Report the top findings with file:line. Do not fix them and do not rewrite
    working code for style - this is a review pass.`,
    { label: 'verify:rust', phase: 'Verify', schema: VERDICT_SCHEMA }),
])

return {
  scope: { in_scope: IN_SCOPE, cut: CUT },
  contract_frozen: true,
  units_built: ok.length,
  units_failed: failed,
  unit_reports: REPORTS,
  build_green: green,
  integration_attempts: attempt,
  integration_report: lastReport,
  verification: checks.filter(Boolean),
}
