//! What the app reads straight from a backup drive when its service does not
//! answer: the job records (`job.json`, `state.json`, `control`,
//! `events.jsonl`), the service lock, and the profiles. Small files and one
//! append-only log; nothing here walks the drive.

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use rdirstat_jobs::{
    CONTROL_FILE, Control, EVENTS_FILE, EVENTS_MAX_LIMIT, Event, EventsPage, JOB_FILE, JobSpec, JobState, JobSummary,
    LOCK_FILE, LockInfo, STATE_FILE, job_dir, jobs_dir, run_dir,
};
use rdirstat_organize::{PROFILES_DIR, Profile};

use super::BackupError;

/// Every job on the drive, by id, from its files. `state.json` may trail
/// `events.jsonl` by a few items; a job without one reads as queued.
pub(crate) fn read_jobs(dest: &Path) -> Vec<JobSummary> {
    let Ok(entries) = std::fs::read_dir(jobs_dir(dest)) else {
        return Vec::new();
    };
    let mut jobs: Vec<JobSummary> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| read_job(&entry.path()))
        .collect();
    jobs.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    jobs
}

fn read_job(dir: &Path) -> Option<JobSummary> {
    let spec: JobSpec = serde_json::from_slice(&std::fs::read(dir.join(JOB_FILE)).ok()?).ok()?;
    let state = std::fs::read(dir.join(STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JobState>(&bytes).ok())
        .unwrap_or_else(|| JobState::new(&spec, spec.created_at_ms));
    let control = std::fs::read_to_string(dir.join(CONTROL_FILE)).map_or(Control::Run, |text| Control::parse(&text));
    Some(JobSummary {
        unfinished: state.phase.is_unfinished(),
        spec,
        state,
        control,
        active: false,
    })
}

/// `events.jsonl` after `after`, at most `limit` (capped at
/// [`EVENTS_MAX_LIMIT`]), as the service would page it. `seq` is the line
/// number, so the first `after` lines are skipped unparsed; a last line cut
/// short by a crash is ignored.
///
/// # Errors
///
/// [`BackupError::Invalid`] for an unknown job.
pub(crate) fn read_events(dest: &Path, job: &str, after: u64, limit: usize) -> Result<EventsPage, BackupError> {
    let dir = job_dir(dest, job);
    let summary =
        read_job(&dir).ok_or_else(|| BackupError::Invalid(format!("no job {job:?} on {}", dest.display())))?;
    let limit = limit.clamp(1, EVENTS_MAX_LIMIT);
    let mut events = Vec::new();
    let mut last_seq = 0;
    if let Ok(file) = std::fs::File::open(dir.join(EVENTS_FILE)) {
        for (index, line) in std::io::BufReader::new(file).split(b'\n').enumerate() {
            let Ok(line) = line else { break };
            let seq = index as u64 + 1;
            if seq <= after || events.len() >= limit {
                last_seq = seq;
                continue;
            }
            match serde_json::from_slice::<Event>(&line) {
                Ok(event) => {
                    last_seq = event.seq;
                    events.push(event);
                }
                // A torn last line, or a kind this build does not know:
                // count it, show nothing.
                Err(_) => last_seq = seq,
            }
        }
    }
    Ok(EventsPage {
        events,
        last_seq: last_seq.max(summary.state.last_seq),
        finished: summary.state.phase.is_terminal(),
    })
}

/// The drive's service lock, if any. A lock that does not parse reads as
/// none: only the service judges it.
pub(crate) fn read_lock(dest: &Path) -> Option<LockInfo> {
    serde_json::from_slice(&std::fs::read(run_dir(dest).join(LOCK_FILE)).ok()?).ok()
}

fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        Command::new("/bin/hostname")
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    })
}

/// What a lock says about its holder, as far as this Mac can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    /// A process with the lock's pid runs here, or the lock is another
    /// host's and cannot be disproved.
    Alive,
    /// The lock is this host's and its pid is not running.
    Gone,
}

/// Judges a lock's holder with `kill -0` (run as a tool: this crate has no
/// `unsafe`). The service makes the authoritative call when it starts.
pub(crate) fn holder(lock: &LockInfo) -> Holder {
    if lock.host != hostname() {
        return Holder::Alive;
    }
    let alive = Command::new("/bin/kill")
        .arg("-0")
        .arg(lock.pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if alive { Holder::Alive } else { Holder::Gone }
}

/// `<dest>/.rdirstat/profiles`.
pub(crate) fn profiles_dir(dest: &Path) -> PathBuf {
    dest.join(PROFILES_DIR)
}

/// The names of the profiles on the drive (`<name>.toml`, not hidden), sorted.
pub(crate) fn profile_names(dest: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(profiles_dir(dest)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".toml")?;
            (!stem.starts_with('.') && rdirstat_organize::validate_label(stem).is_ok()).then(|| stem.to_owned())
        })
        .collect();
    names.sort();
    names
}

/// Reads and validates profile `name` on `dest`.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a bad name, an unreadable file, or a profile
/// that does not parse or validate.
pub(crate) fn load_profile(dest: &Path, name: &str) -> Result<Profile, BackupError> {
    rdirstat_organize::validate_label(name)
        .map_err(|error| BackupError::Invalid(format!("profile name {name:?}: {error}")))?;
    let path = rdirstat_organize::profile_path(dest, name);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| BackupError::Invalid(format!("cannot read {}: {error}", path.display())))?;
    Profile::from_toml(&text).map_err(|error| BackupError::Invalid(format!("{}: {error}", path.display())))
}

/// Writes `profile` to `<dest>/.rdirstat/profiles/<name>.toml` through a temp
/// file, `fsync` and a rename, so a reader sees the old profile or the new
/// one. `dest` is the resolved destination; the file keeps the profile's
/// `dest` as written.
///
/// # Errors
///
/// [`BackupError::Invalid`] when the profile does not validate,
/// [`BackupError::Internal`] for I/O failures.
pub(crate) fn save_profile(dest: &Path, profile: &Profile) -> Result<PathBuf, BackupError> {
    use std::io::Write as _;
    let text = profile
        .to_toml()
        .map_err(|error| BackupError::Invalid(error.to_string()))?;
    let path = rdirstat_organize::profile_path(dest, &profile.name);
    let dir = profiles_dir(dest);
    let io = |what: &str, path: &Path, error: std::io::Error| {
        BackupError::Internal(format!("cannot {what} {}: {error}", path.display()))
    };
    std::fs::create_dir_all(&dir).map_err(|error| io("create", &dir, error))?;
    let temp = dir.join(format!(".{}.toml.tmp-{}", profile.name, std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&temp).map_err(|error| io("create", &temp, error))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| io("write", &temp, error))?;
        std::fs::rename(&temp, &path).map_err(|error| io("publish", &path, error))
    })();
    if written.is_err() {
        drop(std::fs::remove_file(&temp));
    }
    written?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "test code: a failed fixture must abort loudly")]

    use rdirstat_jobs::{EventBody, JobSource, Phase, SCHEMA_VERSION};
    use rdirstat_organize::{PROFILE_SCHEMA_VERSION, ProfileSource};

    use super::*;

    fn spec(dest: &Path, id: &str) -> JobSpec {
        JobSpec {
            schema_version: SCHEMA_VERSION,
            id: id.to_owned(),
            created_at_ms: 1,
            dest: dest.to_path_buf(),
            sources: vec![JobSource {
                label: "a".into(),
                root: "/s".into(),
                include: Vec::new(),
            }],
            rules: None,
            floor: 1,
            max_rounds: 1,
            review_cap: None,
            rollup: true,
            catalog: None,
            mount_poll_ms: 1,
            mount_timeout_ms: None,
            profile: None,
        }
    }

    fn event(seq: u64, phase: Phase) -> Event {
        Event {
            seq,
            ts_ms: 0,
            body: EventBody::Phase {
                phase,
                label: None,
                round: 0,
                detail: None,
            },
        }
    }

    fn write_job(dest: &Path, id: &str, phase: Phase, events: u64) {
        let dir = job_dir(dest, id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let spec = spec(dest, id);
        std::fs::write(dir.join(JOB_FILE), serde_json::to_vec(&spec).expect("json")).expect("write");
        let mut state = JobState::new(&spec, 0);
        state.phase = phase;
        state.last_seq = events.saturating_sub(1);
        std::fs::write(dir.join(STATE_FILE), serde_json::to_vec(&state).expect("json")).expect("write");
        let mut log = String::new();
        for seq in 1..=events {
            log.push_str(&serde_json::to_string(&event(seq, Phase::Applying)).expect("json"));
            log.push('\n');
        }
        log.push_str("{\"seq\":"); // torn by a crash
        std::fs::write(dir.join(EVENTS_FILE), log).expect("write");
    }

    #[test]
    fn jobs_and_events_read_from_the_drive_page_like_the_service() {
        let dest = tempfile::tempdir().expect("tempdir");
        assert!(read_jobs(dest.path()).is_empty());
        write_job(dest.path(), "b", Phase::Applying, 5);
        write_job(dest.path(), "a", Phase::Done, 2);
        std::fs::create_dir_all(job_dir(dest.path(), "junk")).expect("mkdir");

        let jobs = read_jobs(dest.path());
        let ids: Vec<_> = jobs.iter().map(|job| job.spec.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(!jobs[0].unfinished && jobs[1].unfinished);
        assert!(jobs.iter().all(|job| !job.active));

        let page = read_events(dest.path(), "b", 2, 2).expect("page");
        let seqs: Vec<_> = page.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [3, 4]);
        assert_eq!(page.last_seq, 6, "the torn line counts, as a line");
        assert!(!page.finished);
        assert!(read_events(dest.path(), "a", 0, 10).expect("page").finished);
        assert!(matches!(
            read_events(dest.path(), "nope", 0, 10),
            Err(BackupError::Invalid(_))
        ));
    }

    #[test]
    fn a_lock_on_this_host_with_a_dead_pid_is_gone() {
        let mut lock = LockInfo {
            pid: std::process::id(),
            host: hostname().to_owned(),
            boot_id: String::new(),
            version: String::new(),
            started_at_ms: 0,
        };
        assert_eq!(holder(&lock), Holder::Alive);
        lock.pid = 999_999;
        assert_eq!(holder(&lock), Holder::Gone);
        lock.host = "another-host.example".into();
        assert_eq!(holder(&lock), Holder::Alive, "another host's lock cannot be disproved");
    }

    #[test]
    fn profiles_save_atomically_and_list_by_name() {
        let dest = tempfile::tempdir().expect("tempdir");
        let profile = Profile {
            schema_version: PROFILE_SCHEMA_VERSION,
            name: "laptop".into(),
            dest: dest.path().to_path_buf(),
            rules: None,
            floor_allocated: 1 << 20,
            review_cap: 10,
            sources: vec![ProfileSource {
                label: "mac-data".into(),
                root: "/System/Volumes/Data".into(),
                include: vec!["Users/x/Documents".into()],
            }],
        };
        let path = save_profile(dest.path(), &profile).expect("save");
        assert_eq!(path, rdirstat_organize::profile_path(dest.path(), "laptop"));
        std::fs::write(profiles_dir(dest.path()).join(".hidden.toml"), "x").expect("write");
        std::fs::write(profiles_dir(dest.path()).join("notes.txt"), "x").expect("write");
        assert_eq!(profile_names(dest.path()), ["laptop"]);
        assert_eq!(load_profile(dest.path(), "laptop").expect("load"), profile);
        assert!(matches!(
            load_profile(dest.path(), "../x"),
            Err(BackupError::Invalid(_))
        ));

        let mut bad = profile;
        bad.sources.clear();
        assert!(matches!(save_profile(dest.path(), &bad), Err(BackupError::Invalid(_))));
        assert_eq!(
            load_profile(dest.path(), "laptop").expect("load").sources.len(),
            1,
            "a refused save leaves the old profile"
        );
    }
}
