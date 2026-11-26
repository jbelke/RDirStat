/**
 * The Backup page's pure logic: the live job reducer, formatting, the fill
 * bar, draft checks and the index status parser.
 *
 * Run with `pnpm test` (`node --test`, Node's built-in TypeScript stripping),
 * like the other `src/lib/*.test.ts`.
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { BackupEventsEvent, BackupJobSummary, BackupProfile, DestFill, Event } from "./bindings.ts";
import {
  RECENT_LIMIT,
  addSample,
  applyEvents,
  draftProblems,
  fillBar,
  formatEta,
  formatRate,
  formatSizeExact,
  isTerminal,
  labelFor,
  liveFromSummary,
  logLine,
  parseIndexStatus,
  parseSize,
  relativeInclude,
  roundEta,
  throughput,
  validLabel,
  type LiveJob,
} from "./backup.ts";

const DEST = "/Volumes/Backup";
const JOB = "job-1";

function summary(overrides: Partial<BackupJobSummary["state"]> = {}): BackupJobSummary {
  return {
    spec: {
      schema_version: 1,
      id: JOB,
      created_at_ms: 0,
      dest: DEST,
      sources: [{ label: "data", root: "/Volumes/Data" }],
      floor: 0,
      max_rounds: 5,
      rollup: true,
      mount_poll_ms: 1000,
    },
    state: {
      schema_version: 1,
      job_id: JOB,
      phase: "applying",
      label: "data",
      round: 1,
      labels: [],
      counts: { copied: { items: 10, bytes: 1000 }, cloned: { items: 2, bytes: 200 }, failed: { items: 1, bytes: 5 } },
      items_done: 13,
      items_total: 100,
      last_seq: 50,
      updated_at_ms: 0,
      ...overrides,
    },
    control: "run",
    active: true,
    unfinished: true,
  };
}

function item(seq: number, outcome: Extract<Event, { event: "item" }>["outcome"], bytes = 100, ts = seq * 1000): Event {
  return {
    seq,
    ts_ms: ts,
    event: "item",
    label: "data",
    round: 1,
    index: seq - 36,
    total: 100,
    relative: `file-${seq}`,
    dest: `${DEST}/Backups/data/file-${seq}`,
    outcome,
    bytes,
  };
}

function page(events: Event[], overrides: Partial<BackupEventsEvent> = {}): BackupEventsEvent {
  return {
    dest: DEST,
    job_id: JOB,
    events,
    last_seq: events.at(-1)?.seq ?? 0,
    finished: false,
    connected: true,
    ...overrides,
  };
}

test("a summary seeds the counters, landed bytes are copied plus cloned", () => {
  const job = liveFromSummary(DEST, summary());
  assert.equal(job.copied, 10);
  assert.equal(job.cloned, 2);
  assert.equal(job.failed, 1);
  assert.equal(job.landedBytes, 1200);
  assert.equal(job.lastSeq, 50);
  assert.equal(job.itemIndex, 13);
});

test("events fold into counters, and ones already seen are ignored", () => {
  let job = liveFromSummary(DEST, summary());
  job = applyEvents(job, page([item(50, "copied"), item(51, "copied", 300), item(52, "cloned", 50), item(53, "already_landed")]));
  assert.equal(job.copied, 11, "seq 50 was already in the summary");
  assert.equal(job.cloned, 3);
  assert.equal(job.alreadyLanded, 1);
  assert.equal(job.landedBytes, 1200 + 350);
  assert.equal(job.lastSeq, 53);
  assert.equal(job.lastItem, "file-53");
  const again = applyEvents(job, page([item(53, "copied")]));
  assert.equal(again, job, "a repeat page returns the same object");
});

test("changed and failed items reach the log, landed items do not", () => {
  let job = liveFromSummary(DEST, summary());
  job = applyEvents(job, page([item(51, "copied"), item(52, "changed_since_scan"), item(53, "failed"), item(54, "changed_during_copy")]));
  assert.equal(job.changed, 2);
  assert.equal(job.failed, 2);
  assert.deepEqual(
    job.recent.map((line) => line.seq),
    [52, 53, 54],
  );
});

test("the log keeps only the last RECENT_LIMIT lines", () => {
  let job = liveFromSummary(DEST, summary({ last_seq: 0 }));
  const events: Event[] = [];
  for (let seq = 1; seq <= RECENT_LIMIT + 50; seq += 1) events.push(item(seq, "failed", 1));
  job = applyEvents(job, page(events));
  assert.equal(job.recent.length, RECENT_LIMIT);
  assert.equal(job.recent[0]?.seq, 51);
  assert.equal(job.failed, 1 + RECENT_LIMIT + 50);
});

test("a phase event moves the job, and waiting for the drive carries its reason", () => {
  let job = liveFromSummary(DEST, summary());
  job = applyEvents(
    job,
    page([{ seq: 51, ts_ms: 1, event: "phase", phase: "waiting_for_mount", label: "data", round: 2, detail: "/Volumes/Data is gone" }]),
  );
  assert.equal(job.phase, "waiting_for_mount");
  assert.equal(job.round, 2);
  assert.equal(job.detail, "/Volumes/Data is gone");
  assert.match(job.recent[0]?.text ?? "", /Waiting for drive/u);
});

test("a page with no events still carries connection and finish changes", () => {
  const job = liveFromSummary(DEST, summary());
  const lost = applyEvents(job, page([], { connected: false }));
  assert.equal(lost.connected, false);
  const back = applyEvents(lost, page([], { connected: true, finished: true }));
  assert.equal(back.connected, true);
  assert.equal(back.finished, true);
});

test("another job's page is ignored", () => {
  const job = liveFromSummary(DEST, summary());
  assert.equal(applyEvents(job, page([item(60, "copied")], { job_id: "job-2" })), job);
});

test("throughput comes from the sample window, and the ETA from the item rate", () => {
  let job: LiveJob = liveFromSummary(DEST, summary({ last_seq: 0, counts: {} }));
  job = applyEvents(job, page([item(1, "copied", 0, 0)]));
  job = applyEvents(job, page([item(2, "copied", 10_000_000, 2000), item(3, "copied", 10_000_000, 4000)]));
  const rate = throughput(job.samples);
  assert.equal(rate.bytesPerSecond, 5_000_000);
  assert.equal(rate.itemsPerSecond, 0.5);
  // item(3) has index 3 - 36 < 0, so set a position explicitly.
  const eta = roundEta({ phase: "applying", itemIndex: 90, itemTotal: 100 }, rate);
  assert.equal(eta, 20_000);
  assert.equal(roundEta({ phase: "paused", itemIndex: 90, itemTotal: 100 }, rate), null);
});

test("samples older than the window are dropped", () => {
  let samples = addSample([], { ts: 0, bytes: 0, items: 0 });
  samples = addSample(samples, { ts: 10_000, bytes: 1, items: 1 });
  samples = addSample(samples, { ts: 20_000, bytes: 2, items: 2 });
  assert.deepEqual(
    samples.map((s) => s.ts),
    [10_000, 20_000],
  );
  assert.equal(throughput([{ ts: 0, bytes: 0, items: 0 }]).bytesPerSecond, null);
});

test("formatting", () => {
  assert.equal(formatRate(null), "—");
  assert.equal(formatRate(2_410_000_000), "2.41 GB/s");
  assert.equal(formatEta(null), "—");
  assert.equal(formatEta(20_000), "under a minute");
  assert.equal(formatEta(12 * 60_000), "12 min");
  assert.equal(formatEta(185 * 60_000), "3 h 05 min");
  assert.equal(parseSize("100 MB"), 100_000_000);
  assert.equal(parseSize("100MiB"), 104_857_600);
  assert.equal(parseSize("1.5g"), 1_500_000_000);
  assert.equal(parseSize("42"), 42);
  assert.equal(parseSize("lots"), null);
  for (const bytes of [104_857_600, 5_000_000, 2 * 1024 ** 4, 12_345, 1024]) {
    assert.equal(parseSize(formatSizeExact(bytes)), bytes, formatSizeExact(bytes));
  }
  assert.equal(formatSizeExact(104_857_600), "100 MiB");
  assert.equal(formatSizeExact(1024), "1 KiB");
  assert.equal(isTerminal("done"), true);
  assert.equal(isTerminal("waiting_for_mount"), false);
});

test("log lines name rounds and errors", () => {
  const round = logLine({
    seq: 1,
    ts_ms: 0,
    event: "round",
    label: "data",
    round: 2,
    landed: { items: 3, bytes: 3000 },
    already_landed: { items: 1, bytes: 0 },
    not_landed: { items: 0, bytes: 0 },
    complete: true,
  });
  assert.equal(round?.tone, "ok");
  assert.match(round?.text ?? "", /data round 2 complete/u);
  assert.equal(logLine({ seq: 2, ts_ms: 0, event: "error", message: "boom" })?.text, "boom");
  assert.equal(logLine(item(3, "copied")), null);
});

function fill(overrides: Partial<DestFill> = {}): DestFill {
  return {
    total_bytes: 1000,
    available_bytes: 800,
    used_bytes: 200,
    reserve_pressure: 0.75,
    planned_logical: 300,
    wanted_logical: 500,
    projected_fill_planned: 0.5,
    projected_fill_wanted: 0.7,
    fits: true,
    ...overrides,
  };
}

test("the fill bar splits used, planned and deferred against the limit", () => {
  assert.deepEqual(fillBar(fill()), { used: 20, planned: 30, deferred: 20, limit: 75, fits: true, overBy: 0 });
});

test("a fill bar that does not fit never draws past 100% and says by how much it misses", () => {
  const bar = fillBar(fill({ planned_logical: 550, wanted_logical: 2000, fits: false }));
  assert.equal(bar.used + bar.planned + bar.deferred, 100);
  assert.equal(bar.fits, false);
  assert.equal(bar.overBy, 200 + 2000 - 750);
  assert.deepEqual(fillBar(fill({ total_bytes: 0 })).used, 0);
});

function profile(overrides: Partial<BackupProfile> = {}): BackupProfile {
  return {
    schema_version: 1,
    name: "nightly",
    dest: DEST,
    rules: null,
    floor_allocated: 100,
    review_cap: 50_000,
    sources: [{ label: "data", root: "/Volumes/Data", include: [] }],
    ...overrides,
  };
}

const DISKS: Record<string, string> = { [DEST]: "disk4", "/Volumes/Data": "disk6", "/Volumes/Twin": "disk4" };
const diskOf = (mount: string) => DISKS[mount] ?? null;

test("a good draft has no problems", () => {
  assert.deepEqual(draftProblems(profile(), diskOf), []);
});

test("a source on the destination's physical disk is refused next to the source", () => {
  const problems = draftProblems(profile({ sources: [{ label: "twin", root: "/Volumes/Twin", include: [] }] }), diskOf);
  assert.equal(problems.length, 1);
  assert.equal(problems[0]?.field, "source:0");
  assert.match(problems[0]?.message ?? "", /same physical disk \(disk4\)/u);
});

test("draft checks mirror Profile::validate", () => {
  const fields = draftProblems(
    profile({
      name: ".hidden",
      review_cap: 0,
      sources: [
        { label: "data", root: "/Volumes/Data", include: ["/Volumes/Other/x"] },
        { label: "data", root: "/Volumes/Data2", include: [] },
      ],
    }),
    diskOf,
  ).map((problem) => problem.field);
  assert.deepEqual(fields, ["name", "cap", "source:0", "source:1"]);
  assert.deepEqual(
    draftProblems(profile({ sources: [], dest: "" }), diskOf).map((p) => p.field),
    ["dest", "sources"],
  );
});

test("labels and includes", () => {
  assert.equal(validLabel("mac-data_1.x"), true);
  assert.equal(validLabel(".x"), false);
  assert.equal(validLabel("a/b"), false);
  assert.equal(validLabel("x".repeat(65)), false);
  assert.equal(labelFor("/"), "Macintosh-HD");
  assert.equal(labelFor("/Volumes/My Photos"), "My-Photos");
  assert.equal(relativeInclude("/Volumes/Data", "/Volumes/Data/Projects/a"), "Projects/a");
  assert.equal(relativeInclude("/", "/Users/me"), "Users/me");
});

test("the index status parser reads the CLI document and tolerates gaps", () => {
  const status = parseIndexStatus({
    dest: DEST,
    catalog: `${DEST}/.rdirstat/catalog`,
    sources: [
      {
        source_label: "data",
        backed_up: { items: 5, bytes: 500 },
        cloned: { items: 1, bytes: 100 },
        last_plan: "plan-3",
        pending: { items: 0, bytes: 0 },
        skipped: { items: 2, bytes: 20 },
        floor_allocated: 100,
        not_backed_up: null,
        largest_not_backed_up: [["big.mov", 9000]],
      },
      { source_label: "partial" },
    ],
    warnings: ["Spotlight is indexing this drive"],
  });
  assert.equal(status.sources.length, 2);
  assert.deepEqual(status.sources[0]?.backedUp, { items: 5, bytes: 500 });
  assert.deepEqual(status.sources[0]?.largestNotBackedUp, [{ path: "big.mov", bytes: 9000 }]);
  assert.equal(status.sources[0]?.notBackedUp, null);
  assert.deepEqual(status.sources[1]?.pending, { items: 0, bytes: 0 });
  assert.equal(status.warnings.length, 1);
  assert.deepEqual(parseIndexStatus("nonsense").sources, []);
});
