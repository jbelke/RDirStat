//! The service against temp dirs, over a real socket. Two temp dirs share
//! one disk, so the service gets fixed disk facts through [`DiskProbe`].
//! Temp dirs are made under `/tmp` because a Unix socket path must fit in
//! 104 bytes and `$TMPDIR` on macOS is long. Each fixture's socket goes to
//! its own `t/rds/`, never to the real per-user directory.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test code: a failed fixture must abort loudly"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use rdirstat_jobs::{
    Event, EventBody, EventsPage, Health, ItemOutcome, JobList, JobSource, JobSummary, LockInfo, NewJob, Phase,
};
use rdirstat_organize::DiskFacts;

use super::client::{CallError, call};
use super::lock::{Lock, LockRefusal};
use super::record::Record;
use super::*;

const DEST_UUID: &str = "B6C3D4E7-1AA7-3BD1-B725-6033F3101516";
const T: Duration = Duration::from_secs(10);

/// Paths under `source` are on disk3, the rest on disk14 with [`DEST_UUID`].
struct FakeProbe {
    source: PathBuf,
}

impl DiskProbe for FakeProbe {
    fn facts(&self, path: &Path) -> Result<DiskFacts> {
        let on_source = path.starts_with(&self.source);
        Ok(DiskFacts {
            disk_id: Some(if on_source { "disk3" } else { "disk14" }.to_owned()),
            volume_uuid: Some(if on_source { "SOURCE-UUID" } else { DEST_UUID }.to_owned()),
            st_dev: if on_source { 1 } else { 2 },
            filesystem: "apfs".into(),
            total_bytes: 1 << 40,
            available_bytes: 1 << 40,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    /// Parent of the source root; the probe treats everything under it as
    /// the source disk, so the root itself may come and go.
    source_disk: PathBuf,
    source: PathBuf,
    dest: PathBuf,
    catalog: PathBuf,
    /// Stands in for `$TMPDIR`: the socket goes under it.
    tmp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self::empty();
        build_source(&fixture.source);
        fixture
    }

    /// A fixture whose source root does not exist yet.
    fn empty() -> Self {
        let root = tempfile::Builder::new()
            .prefix("rds")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let base = std::fs::canonicalize(root.path()).expect("canonical");
        let source_disk = base.join("s");
        let dest = base.join("d");
        let catalog = base.join("c");
        let tmp = base.join("t");
        for dir in [&source_disk, &dest, &catalog, &tmp] {
            std::fs::create_dir(dir).expect("mkdir");
        }
        Self {
            _root: root,
            source: source_disk.join("vol"),
            source_disk,
            dest,
            catalog,
            tmp,
        }
    }

    fn local(&self) -> LocalDirs {
        LocalDirs {
            home: None,
            tmpdir: Some(self.tmp.clone()),
        }
    }

    fn config(&self, on_item: Option<Arc<ItemHook>>) -> ServeConfig {
        ServeConfig {
            dest: self.dest.clone(),
            probe: Arc::new(FakeProbe {
                source: self.source_disk.clone(),
            }),
            default_catalog: Some(self.catalog.clone()),
            app_catalog: None,
            local: self.local(),
            signals: false,
            on_item,
        }
    }

    fn start(&self, on_item: Option<Arc<ItemHook>>) -> Running {
        start(self.config(on_item)).expect("start")
    }

    fn socket(&self) -> PathBuf {
        self.tmp
            .join(rdirstat_jobs::TMP_RUN_DIR)
            .join(format!("{DEST_UUID}.sock"))
    }

    fn new_job(&self, id: &str) -> NewJob {
        NewJob {
            id: Some(id.to_owned()),
            sources: vec![JobSource {
                label: "mac-data".into(),
                root: self.source.clone(),
            }],
            floor: Some(8192),
            mount_poll_ms: Some(20),
            ..NewJob::default()
        }
    }

    fn submit(&self, new: &NewJob) -> JobSummary {
        post(&self.socket(), "/v1/jobs", Some(&serde_json::to_vec(new).unwrap())).expect("submit")
    }

    fn job(&self, id: &str) -> JobSummary {
        get(&self.socket(), &format!("/v1/jobs/{id}")).expect("job")
    }

    /// Polls the job until `done` holds for it.
    fn wait(&self, id: &str, what: &str, done: impl Fn(&JobSummary) -> bool) -> JobSummary {
        let deadline = Instant::now() + T;
        loop {
            let job = self.job(id);
            if done(&job) {
                return job;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {job:#?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_phase(&self, id: &str, phase: Phase) -> JobSummary {
        self.wait(id, &format!("{phase:?}"), |job| job.state.phase == phase && !job.active)
    }

    fn events(&self, id: &str) -> Vec<Event> {
        let page: EventsPage = get(&self.socket(), &format!("/v1/jobs/{id}/events?after=0&wait=0")).expect("events");
        page.events
    }
}

fn get<T: serde::de::DeserializeOwned>(socket: &Path, path: &str) -> std::result::Result<T, CallError> {
    call(socket, "GET", path, None, T)
}

fn post<T: serde::de::DeserializeOwned>(
    socket: &Path,
    path: &str,
    body: Option<&[u8]>,
) -> std::result::Result<T, CallError> {
    call(socket, "POST", path, body, T)
}

fn put(root: &Path, relative: &str, len: usize, seed: u8) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes: Vec<u8> = (0..len)
        .map(|i| u8::try_from((i * 31 + usize::from(seed)) % 251).unwrap_or(0))
        .collect();
    std::fs::write(path, bytes).unwrap();
}

fn build_source(root: &Path) {
    put(root, "Movies/big.bin", 64 * 1024, 1);
    put(root, "Documents/report.pdf", 32 * 1024, 2);
    put(root, "Documents/dupe-a.bin", 16 * 1024, 3);
    put(root, "Archive/old/dupe-b.bin", 16 * 1024, 3);
    put(root, "Music/song.flac", 20 * 1024, 4);
    put(root, "notes.txt", 5, 5);
}

fn items(events: &[Event]) -> Vec<(u32, String, ItemOutcome)> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Item {
                round,
                relative,
                outcome,
                ..
            } => Some((*round, relative.clone(), *outcome)),
            _ => None,
        })
        .collect()
}

fn phases(events: &[Event]) -> Vec<Phase> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Phase { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect()
}

fn assert_contiguous(events: &[Event]) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1, "seq gap at {index}: {events:#?}");
    }
}

/// An item hook that runs `action` once, after the first item.
fn once(action: impl Fn() + Send + Sync + 'static) -> Arc<ItemHook> {
    let fired = AtomicBool::new(false);
    Arc::new(move |_job: &str, _result: &crate::backup::ItemResult| {
        if !fired.swap(true, Ordering::SeqCst) {
            action();
        }
    })
}

fn lock_info(pid: u32, host: String, boot_id: String) -> LockInfo {
    LockInfo {
        pid,
        host,
        boot_id,
        version: "0.0.0".into(),
        started_at_ms: 0,
    }
}

/// A pid that is not running: a child that has been reaped.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true").spawn().expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

fn write_lock(path: &Path, info: &LockInfo) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(info).unwrap()).unwrap();
}

#[test]
fn lock_takes_over_only_a_provably_stale_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let me = lock::me(1);

    // A dead pid on this host and boot: taken over.
    write_lock(&path, &lock_info(dead_pid(), me.host.clone(), me.boot_id.clone()));
    let lock = Lock::acquire(&path, me.clone()).expect("stale pid is taken over");
    assert!(lock.took_over.is_some());
    // Held by this live process: refused, and the file is untouched.
    let refusal = Lock::acquire(&path, lock::me(2)).unwrap_err();
    assert!(
        matches!(refusal.downcast_ref::<LockRefusal>(), Some(LockRefusal::Held(_))),
        "{refusal:#}"
    );
    drop(lock);
    assert!(!path.exists(), "dropping the lock removes it");

    // Another boot of this host: stale even if the pid is alive now.
    write_lock(
        &path,
        &lock_info(std::process::id(), me.host.clone(), "older-boot".into()),
    );
    let lock = Lock::acquire(&path, me.clone()).expect("other boot is taken over");
    drop(lock);

    // Another host: cannot be judged, never taken over.
    write_lock(
        &path,
        &lock_info(dead_pid(), "elsewhere.local".into(), me.boot_id.clone()),
    );
    let refusal = Lock::acquire(&path, me.clone()).unwrap_err();
    assert!(matches!(
        refusal.downcast_ref::<LockRefusal>(),
        Some(LockRefusal::Unjudgeable(_))
    ));
    assert!(path.exists());

    // Garbage: refused, left for a person.
    std::fs::write(&path, b"{not json").unwrap();
    let refusal = Lock::acquire(&path, me).unwrap_err();
    assert!(matches!(
        refusal.downcast_ref::<LockRefusal>(),
        Some(LockRefusal::Unjudgeable(_))
    ));
}

#[test]
fn second_serve_on_the_same_drive_refuses_while_the_first_is_alive() {
    let fixture = Fixture::new();
    let first = fixture.start(None);
    let refusal = start(fixture.config(None)).unwrap_err();
    assert!(
        matches!(refusal.downcast_ref::<LockRefusal>(), Some(LockRefusal::Held(_))),
        "{refusal:#}"
    );
    // The first still answers: the refused start touched neither socket nor lock.
    let health: Health = get(&fixture.socket(), "/v1/health").expect("health");
    assert_eq!(health.pid, std::process::id());
    first.shutdown();
    first.join().unwrap();
    assert!(!fixture.socket().exists(), "a clean exit removes the socket");
    let second = fixture.start(None);
    second.shutdown();
    second.join().unwrap();
}

#[test]
fn the_socket_is_per_user_and_never_on_the_drive() {
    let fixture = Fixture::new();
    let running = fixture.start(None);
    assert_eq!(running.socket, fixture.socket());
    assert!(
        !running.socket.starts_with(&fixture.dest),
        "{}",
        running.socket.display()
    );
    assert_eq!(
        socket_for(
            &fixture.dest,
            &FakeProbe {
                source: fixture.source_disk.clone()
            },
            &fixture.local()
        )
        .unwrap(),
        running.socket,
        "a client computes the path the service bound"
    );
    let run = rdirstat_jobs::run_dir(&fixture.dest);
    assert!(
        run.join(rdirstat_jobs::LOCK_FILE).is_file(),
        "the lock stays on the drive"
    );
    let on_drive: Vec<_> = std::fs::read_dir(&run)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(on_drive, vec![std::ffi::OsString::from(rdirstat_jobs::LOCK_FILE)]);
    let dir = running.socket.parent().unwrap().to_path_buf();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

    // Removed from under it (a temp-dir cleaner): listening again, directory
    // and all, within a rebind tick.
    std::fs::remove_dir_all(&dir).unwrap();
    let deadline = Instant::now() + REBIND_EVERY * 3;
    loop {
        if get::<Health>(&fixture.socket(), "/v1/health").is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the socket was not rebound");
        std::thread::sleep(Duration::from_millis(100));
    }
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn a_stale_socket_is_replaced_once_the_lock_proves_its_service_dead() {
    let fixture = Fixture::new();
    let run = rdirstat_jobs::run_dir(&fixture.dest);
    let me = lock::me(0);
    write_lock(
        &run.join(rdirstat_jobs::LOCK_FILE),
        &lock_info(dead_pid(), me.host, me.boot_id),
    );
    std::fs::create_dir_all(fixture.socket().parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(fixture.socket()).unwrap());
    assert!(fixture.socket().exists());
    assert!(matches!(
        get::<Health>(&fixture.socket(), "/v1/health"),
        Err(CallError::Unreachable(_))
    ));

    let running = fixture.start(None);
    let health: Health = get(&fixture.socket(), "/v1/health").expect("health over the new socket");
    assert_eq!(health.dest, fixture.dest);
    assert_eq!(health.dest_disk_id.as_deref(), Some("disk14"));
    let mode = std::fs::metadata(fixture.socket()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(fixture.socket().parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "a pre-existing socket directory is tightened to 0700");
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn api_runs_a_job_to_done_and_events_resume_after_any_seq() {
    let fixture = Fixture::new();
    let running = fixture.start(None);
    let socket = fixture.socket();
    assert!(matches!(
        post::<JobSummary>(&socket, "/v1/jobs", Some(b"{}")),
        Err(CallError::Api { status: 400, .. })
    ));
    let created = fixture.submit(&fixture.new_job("first"));
    assert_eq!(created.spec.dest, fixture.dest);
    assert!(matches!(
        post::<JobSummary>(
            &socket,
            "/v1/jobs",
            Some(&serde_json::to_vec(&fixture.new_job("first")).unwrap())
        ),
        Err(CallError::Api { status: 409, .. })
    ));
    assert!(matches!(
        get::<JobSummary>(&socket, "/v1/jobs/nope"),
        Err(CallError::Api { status: 404, .. })
    ));

    // Follow the job with long-polls only, as the app will.
    let mut seen = Vec::new();
    let mut after = 0;
    let deadline = Instant::now() + T;
    loop {
        let page: EventsPage = get(&socket, &format!("/v1/jobs/first/events?after={after}&wait=2&limit=3")).unwrap();
        assert!(page.events.len() <= 3);
        if let Some(last) = page.events.last() {
            after = last.seq;
        }
        seen.extend(page.events);
        if page.finished && after == page.last_seq {
            break;
        }
        assert!(Instant::now() < deadline, "job did not finish: {seen:#?}");
    }
    assert_contiguous(&seen);
    let job = fixture.wait_phase("first", Phase::Done);
    assert!(!job.unfinished);
    assert_eq!(job.state.labels[0].status, rdirstat_jobs::LabelStatus::Done);
    let landed = items(&seen);
    assert!(
        landed
            .iter()
            .any(|(_, relative, outcome)| relative == "Movies/big.bin" && outcome.landed_now())
    );
    assert_eq!(
        std::fs::read(fixture.dest.join("Backups/mac-data/Movies/big.bin")).unwrap(),
        std::fs::read(fixture.source.join("Movies/big.bin")).unwrap()
    );
    assert!(phases(&seen).contains(&Phase::Indexing));
    assert!(fixture.dest.join("index.parquet").exists());

    // The same events from the file and from one unpaged read.
    assert_eq!(fixture.events("first"), seen);
    let dir = rdirstat_jobs::job_dir(&fixture.dest, "first");
    let on_disk: Vec<Event> = std::fs::read_to_string(dir.join(rdirstat_jobs::EVENTS_FILE))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(on_disk, seen);
    let state: rdirstat_jobs::JobState =
        serde_json::from_slice(&std::fs::read(dir.join(rdirstat_jobs::STATE_FILE)).unwrap()).unwrap();
    assert_eq!(state.phase, Phase::Done);
    assert_eq!(state.last_seq, seen.last().unwrap().seq);

    let list: JobList = get(&socket, "/v1/jobs").unwrap();
    assert_eq!(list.jobs.len(), 1);
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn pause_holds_between_items_and_resume_continues() {
    let fixture = Fixture::new();
    let socket = fixture.socket();
    let hook_socket = socket.clone();
    let running = fixture.start(Some(once(move || {
        let _: JobSummary = post(&hook_socket, "/v1/jobs/held/pause", None).unwrap();
    })));
    fixture.submit(&fixture.new_job("held"));
    let paused = fixture.wait("held", "pause", |job| job.state.phase == Phase::Paused);
    assert_eq!(paused.control, rdirstat_jobs::Control::Pause);
    let items_when_paused = items(&fixture.events("held")).len();
    assert_eq!(items_when_paused, 1, "the hold starts at the next item boundary");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        items(&fixture.events("held")).len(),
        items_when_paused,
        "nothing lands while paused"
    );
    let control = std::fs::read_to_string(rdirstat_jobs::job_dir(&fixture.dest, "held").join("control")).unwrap();
    assert_eq!(control.trim(), "pause", "the pause survives a restart");

    let _: JobSummary = post(&socket, "/v1/jobs/held/resume", None).unwrap();
    fixture.wait_phase("held", Phase::Done);
    let seen = phases(&fixture.events("held"));
    let paused_at = seen.iter().position(|phase| *phase == Phase::Paused).unwrap();
    assert_eq!(seen[paused_at + 1], Phase::Applying);
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn cancel_stops_between_items_and_resume_finishes_without_copying_twice() {
    let fixture = Fixture::new();
    let socket = fixture.socket();
    let hook_socket = socket.clone();
    let running = fixture.start(Some(once(move || {
        let _: JobSummary = post(&hook_socket, "/v1/jobs/cut/cancel", None).unwrap();
    })));
    fixture.submit(&fixture.new_job("cut"));
    let cancelled = fixture.wait_phase("cut", Phase::Cancelled);
    assert!(!cancelled.unfinished);
    let first = items(&fixture.events("cut"));
    assert_eq!(first.len(), 1, "{first:?}");
    let (_, first_item, first_outcome) = first[0].clone();
    assert!(first_outcome.landed_now());

    let _: JobSummary = post(&socket, "/v1/jobs/cut/resume", None).unwrap();
    let done = fixture.wait_phase("cut", Phase::Done);
    assert_eq!(
        done.state.labels[0].rounds, 2,
        "the cancelled round counts; the resume is round 2"
    );
    let all = items(&fixture.events("cut"));
    let copied_again = all
        .iter()
        .any(|(round, relative, outcome)| *round == 2 && *relative == first_item && outcome.landed_now());
    assert!(!copied_again, "{first_item} was copied twice: {all:?}");
    assert_contiguous(&fixture.events("cut"));
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn shutdown_finishes_the_item_and_the_next_service_resumes_the_job() {
    let fixture = Fixture::new();
    let socket = fixture.socket();
    let hook_socket = socket.clone();
    let running = fixture.start(Some(once(move || {
        let _: Health = post(&hook_socket, "/v1/shutdown", None).unwrap();
    })));
    fixture.submit(&fixture.new_job("interrupted"));
    running.join().unwrap();

    // Down: the drive alone says what happened.
    let list = read_jobs(&fixture.dest).unwrap();
    let job = &list.jobs[0];
    assert_eq!(job.state.phase, Phase::Stopped);
    assert!(job.unfinished);
    assert_eq!(items(&fixture.events_from_disk("interrupted")).len(), 1);
    let text = jobs_text(&fixture.dest, &list, "drive");
    assert!(text.contains("UNFINISHED"), "{text}");
    assert!(text.contains("not running"), "{text}");

    // Up again: picked up without being asked.
    let running = fixture.start(None);
    let done = fixture.wait_phase("interrupted", Phase::Done);
    assert_eq!(done.state.labels[0].status, rdirstat_jobs::LabelStatus::Done);
    let events = fixture.events("interrupted");
    assert_contiguous(&events);
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn a_missing_source_is_waited_for_then_backed_up() {
    let fixture = Fixture::empty();
    let running = fixture.start(None);
    fixture.submit(&fixture.new_job("late"));
    let waiting = fixture.wait("late", "the mount wait", |job| {
        job.state.phase == Phase::WaitingForMount
    });
    assert_eq!(waiting.state.labels[0].rounds, 0, "waiting costs no round");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.job("late").state.phase, Phase::WaitingForMount);

    // The volume "mounts": built aside, then renamed into place whole.
    let staging = fixture.source_disk.join("staging");
    build_source(&staging);
    std::fs::rename(&staging, &fixture.source).unwrap();
    let done = fixture.wait_phase("late", Phase::Done);
    assert_eq!(done.state.labels[0].rounds, 1);
    assert!(fixture.dest.join("Backups/mac-data/Movies/big.bin").exists());
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn a_volume_that_never_comes_back_fails_the_label_after_the_timeout() {
    let fixture = Fixture::empty();
    let running = fixture.start(None);
    let mut new = fixture.new_job("gone");
    new.mount_timeout_ms = Some(150);
    fixture.submit(&new);
    let failed = fixture.wait_phase("gone", Phase::Failed);
    assert!(
        failed
            .state
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("did not come back"),
        "{failed:#?}"
    );
    running.shutdown();
    running.join().unwrap();
}

#[test]
fn the_record_resumes_seq_after_a_torn_line_and_seeks_by_mark() {
    let dir = tempfile::tempdir().unwrap();
    let spec: rdirstat_jobs::JobSpec = serde_json::from_value(serde_json::json!({
        "schema_version": 1, "id": "r", "created_at_ms": 0, "dest": "/d",
        "sources": [], "floor": 1, "max_rounds": 1, "rollup": true, "mount_poll_ms": 1
    }))
    .unwrap();
    let error = |n: u64| EventBody::Error {
        label: None,
        message: format!("e{n}"),
    };
    {
        let mut record = Record::load(dir.path(), &spec).unwrap();
        for n in 1..=600 {
            assert_eq!(record.emit(error(n)), n);
        }
        let page = record.events_after(300, 5).unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![301, 302, 303, 304, 305]
        );
    }
    // A crash mid-write leaves half a line.
    let path = dir.path().join(rdirstat_jobs::EVENTS_FILE);
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, br#"{"seq":601,"ts_ms":1,"ev"#).unwrap();
    drop(file);

    let mut record = Record::load(dir.path(), &spec).unwrap();
    assert_eq!(record.last_seq(), 600);
    assert_eq!(record.emit(error(601)), 601);
    let all: Vec<Event> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_contiguous(&all);
    assert_eq!(record.events_after(599, 10).unwrap().len(), 2);
}

#[test]
fn events_wait_in_memory_while_the_job_directory_is_away() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("job");
    std::fs::create_dir(&dir).unwrap();
    let spec: rdirstat_jobs::JobSpec = serde_json::from_value(serde_json::json!({
        "schema_version": 1, "id": "r", "created_at_ms": 0, "dest": "/d",
        "sources": [], "floor": 1, "max_rounds": 1, "rollup": true, "mount_poll_ms": 1
    }))
    .unwrap();
    let mut record = Record::load(&dir, &spec).unwrap();
    record.emit(EventBody::Error {
        label: None,
        message: "before".into(),
    });
    // The drive drops: the directory is gone, and must not be recreated.
    let aside = root.path().join("away");
    std::fs::rename(&dir, &aside).unwrap();
    record.emit(EventBody::Error {
        label: None,
        message: "during".into(),
    });
    assert!(!dir.exists(), "the record never creates its directory");
    assert_eq!(
        record.events_after(0, 10).unwrap_or_default().len(),
        0,
        "seq 1 is only on the absent drive"
    );
    std::fs::rename(&aside, &dir).unwrap();
    record.flush();
    let lines = std::fs::read_to_string(dir.join(rdirstat_jobs::EVENTS_FILE)).unwrap();
    let seqs: Vec<u64> = lines
        .lines()
        .map(|line| serde_json::from_str::<Event>(line).unwrap().seq)
        .collect();
    assert_eq!(seqs, vec![1, 2]);
}

impl Fixture {
    fn events_from_disk(&self, id: &str) -> Vec<Event> {
        std::fs::read_to_string(rdirstat_jobs::job_dir(&self.dest, id).join(rdirstat_jobs::EVENTS_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
