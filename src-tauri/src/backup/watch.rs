//! Following a job's events and forwarding them to the UI.
//!
//! One task per (drive, job) long-polls the service's
//! `GET /v1/jobs/{id}/events?after=<seq>` and emits each page as a
//! [`BackupEventsEvent`]. It keeps the last `seq` it forwarded, so when the
//! service goes away (app-independent restarts, a drive that drops off) it
//! retries with backoff and resumes after that `seq`: no event is forwarded
//! twice or skipped. It stops when the job is finished and every event has
//! been forwarded, or when the job no longer exists.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hyper::Method;
use rdirstat_jobs::{EVENTS_DEFAULT_WAIT_SECS, EVENTS_MAX_LIMIT, EventsPage};
use tauri_specta::Event as _;

use super::client::{self, CallError};
use super::{drive, socket_for};
use crate::events::BackupEventsEvent;

/// Longest wait between retries while the service is away.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The running followers, by drive and job.
#[derive(Debug, Default)]
pub(crate) struct Watchers {
    tasks: Mutex<HashMap<(PathBuf, String), Follower>>,
}

#[derive(Debug)]
struct Follower {
    done: Arc<AtomicBool>,
    handle: tauri::async_runtime::JoinHandle<()>,
}

impl Watchers {
    /// Starts following `job` on `dest` after `after`, unless a follower for
    /// it is already running. Returns whether one was started.
    pub(crate) fn watch(&self, app: &tauri::AppHandle, dest: PathBuf, job: String, after: u64) -> bool {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (dest.clone(), job.clone());
        if tasks
            .get(&key)
            .is_some_and(|follower| !follower.done.load(Ordering::SeqCst))
        {
            return false;
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let emitter = app.clone();
        let handle = tauri::async_runtime::spawn(async move {
            follow(dest, job, after, |page| drop(page.emit(&emitter))).await;
            flag.store(true, Ordering::SeqCst);
        });
        tasks.insert(key, Follower { done, handle });
        true
    }

    /// Stops following `job` on `dest`. Returns whether a follower ran.
    pub(crate) fn unwatch(&self, dest: &Path, job: &str) -> bool {
        let removed = self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(dest.to_path_buf(), job.to_owned()));
        removed.is_some_and(|follower| {
            follower.handle.abort();
            !follower.done.load(Ordering::SeqCst)
        })
    }
}

/// One step's outcome.
enum Step {
    /// A page came back; go again at once.
    Page(EventsPage),
    /// No service; wait and retry.
    Away,
    /// Stop following.
    Stop,
}

/// Where the service's socket is.
#[derive(Debug, Clone)]
pub(crate) enum Locate {
    /// Worked out from the drive ([`socket_for`]) on every attempt, so a
    /// remounted drive is found again.
    FromDrive,
    /// A known path (tests).
    #[cfg_attr(not(test), allow(dead_code, reason = "tests point the follower at a fake service"))]
    At(PathBuf),
}

/// Follows `job` on `dest` from `after`, handing every page (and every
/// change of reachability) to `emit`, until the job is finished and caught
/// up or gone.
pub(crate) async fn follow(dest: PathBuf, job: String, after: u64, emit: impl Fn(BackupEventsEvent)) {
    follow_at(dest, job, after, &Locate::FromDrive, emit).await;
}

async fn follow_at(dest: PathBuf, job: String, mut after: u64, locate: &Locate, emit: impl Fn(BackupEventsEvent)) {
    let mut connected: Option<bool> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        let step = step(&dest, &job, after, locate).await;
        match step {
            Step::Page(page) => {
                backoff = Duration::from_secs(1);
                let caught_up = page.events.last().map_or(after, |event| event.seq) >= page.last_seq;
                if let Some(last) = page.events.last() {
                    after = last.seq;
                }
                let finished = page.finished && caught_up;
                if !page.events.is_empty() || connected != Some(true) || finished {
                    emit(BackupEventsEvent {
                        dest: dest.to_string_lossy().into_owned(),
                        job_id: job.clone(),
                        events: page.events,
                        last_seq: page.last_seq,
                        finished: page.finished,
                        connected: true,
                    });
                }
                connected = Some(true);
                if finished {
                    return;
                }
            }
            Step::Away => {
                // The drive's own record says what the service last wrote.
                let (dest_c, job_c) = (dest.clone(), job.clone());
                let from_drive = tauri::async_runtime::spawn_blocking(move || {
                    drive::read_events(&dest_c, &job_c, after, EVENTS_MAX_LIMIT)
                })
                .await
                .ok()
                .and_then(Result::ok);
                let finished = from_drive.as_ref().is_some_and(|page| page.finished);
                if connected != Some(false) || from_drive.as_ref().is_some_and(|page| !page.events.is_empty()) {
                    let page = from_drive.unwrap_or(EventsPage {
                        events: Vec::new(),
                        last_seq: after,
                        finished: false,
                    });
                    if let Some(last) = page.events.last() {
                        after = last.seq;
                    }
                    emit(BackupEventsEvent {
                        dest: dest.to_string_lossy().into_owned(),
                        job_id: job.clone(),
                        events: page.events,
                        last_seq: page.last_seq,
                        finished: page.finished,
                        connected: false,
                    });
                }
                connected = Some(false);
                if finished {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Step::Stop => return,
        }
    }
}

async fn step(dest: &Path, job: &str, after: u64, locate: &Locate) -> Step {
    let socket = match locate {
        Locate::At(socket) => socket.clone(),
        Locate::FromDrive => {
            let dest = dest.to_path_buf();
            match tauri::async_runtime::spawn_blocking(move || socket_for(&dest)).await {
                Ok(Ok(socket)) => socket,
                // The drive is not mounted right now: the same as no service.
                _ => return Step::Away,
            }
        }
    };
    let path = format!("/v1/jobs/{job}/events?after={after}&wait={EVENTS_DEFAULT_WAIT_SECS}&limit={EVENTS_MAX_LIMIT}");
    let timeout = Duration::from_secs(EVENTS_DEFAULT_WAIT_SECS + 10);
    match client::call::<EventsPage>(&socket, Method::GET, &path, None, timeout).await {
        Ok(page) => Step::Page(page),
        Err(CallError::Unreachable(_) | CallError::TimedOut) => Step::Away,
        Err(CallError::Api { status: 404, .. }) => Step::Stop,
        Err(error) => {
            tracing::warn!(%error, job, dest = %dest.display(), "following a backup job");
            Step::Away
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "test code: a failed fixture must abort loudly")]

    use std::sync::Mutex;

    use rdirstat_jobs::{Event, EventBody, Phase};

    use super::*;

    fn page(seqs: std::ops::RangeInclusive<u64>, last_seq: u64, finished: bool) -> String {
        let events: Vec<Event> = seqs
            .map(|seq| Event {
                seq,
                ts_ms: 0,
                body: EventBody::Phase {
                    phase: Phase::Applying,
                    label: None,
                    round: 1,
                    detail: None,
                },
            })
            .collect();
        serde_json::to_string(&EventsPage {
            events,
            last_seq,
            finished,
        })
        .expect("json")
    }

    #[test]
    fn pages_are_forwarded_in_order_until_the_job_is_finished_and_caught_up() {
        let dir = client::tests::short_dir();
        let socket = dir.path().join("s.sock");
        let server =
            client::tests::fake_service(&socket, vec![(200, page(1..=2, 4, false)), (200, page(3..=4, 4, true))]);
        let emitted = Mutex::new(Vec::new());
        tauri::async_runtime::block_on(follow_at(
            PathBuf::from("/Volumes/b"),
            "j".into(),
            0,
            &Locate::At(socket),
            |event| emitted.lock().expect("lock").push(event),
        ));
        let emitted = emitted.into_inner().expect("lock");
        let seqs: Vec<_> = emitted
            .iter()
            .flat_map(|event| event.events.iter().map(|event| event.seq))
            .collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert!(emitted.iter().all(|event| event.connected));
        assert!(emitted.last().expect("last").finished);
        let limit = EVENTS_MAX_LIMIT;
        let wait = EVENTS_DEFAULT_WAIT_SECS;
        assert_eq!(
            server.join().expect("server"),
            vec![
                format!("GET /v1/jobs/j/events?after=0&wait={wait}&limit={limit} HTTP/1.1"),
                format!("GET /v1/jobs/j/events?after=2&wait={wait}&limit={limit} HTTP/1.1"),
            ]
        );
    }

    #[test]
    fn without_a_service_it_resumes_from_the_drive_and_stops_on_a_finished_job() {
        let dest = tempfile::tempdir().expect("tempdir");
        let dir = rdirstat_jobs::job_dir(dest.path(), "j");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let spec = serde_json::json!({
            "schema_version": 1, "id": "j", "created_at_ms": 0, "dest": dest.path(),
            "sources": [{"label": "a", "root": "/s"}], "floor": 1, "max_rounds": 1,
            "rollup": true, "mount_poll_ms": 1
        });
        std::fs::write(dir.join(rdirstat_jobs::JOB_FILE), spec.to_string()).expect("write");
        let mut state: rdirstat_jobs::JobState = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "job_id": "j", "phase": "done", "round": 1, "labels": [],
            "counts": {}, "items_done": 0, "items_total": 0, "last_seq": 3, "updated_at_ms": 0
        }))
        .expect("state");
        state.phase = Phase::Done;
        std::fs::write(
            dir.join(rdirstat_jobs::STATE_FILE),
            serde_json::to_string(&state).expect("json"),
        )
        .expect("write");
        let lines: String = (1..=3_u64)
            .map(|seq| {
                let mut line = serde_json::json!({
                    "seq": seq, "ts_ms": 0, "event": "phase", "phase": "done", "round": 1
                })
                .to_string();
                line.push('\n');
                line
            })
            .collect();
        std::fs::write(dir.join(rdirstat_jobs::EVENTS_FILE), lines).expect("write");

        let emitted = Mutex::new(Vec::new());
        let nobody = dest.path().join("nobody.sock");
        tauri::async_runtime::block_on(follow_at(
            dest.path().to_path_buf(),
            "j".into(),
            1,
            &Locate::At(nobody),
            |event| emitted.lock().expect("lock").push(event),
        ));
        let emitted = emitted.into_inner().expect("lock");
        assert_eq!(emitted.len(), 1, "{emitted:?}");
        let event = &emitted[0];
        assert!(!event.connected);
        assert!(event.finished);
        let seqs: Vec<_> = event.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [2, 3], "resumes after the seq it was given");
    }
}
