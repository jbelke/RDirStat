//! One job's durable record: `events.jsonl` (append-only, `fsync`ed per
//! event), `state.json` (replaced atomically), and `control`.
//!
//! The record never creates its job directory. When the drive is away the
//! directory is missing, every write fails with `ENOENT`, and events wait in
//! memory with their `seq` already assigned; the next write after the drive
//! returns appends them in order.

use std::io::{BufRead as _, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rdirstat_jobs::{CONTROL_FILE, Control, EVENTS_FILE, Event, EventBody, JobSpec, JobState, STATE_FILE};
use tokio::sync::watch;

use crate::backup::now_ms;

/// One byte offset is kept per this many events, so a reader resuming at
/// any `seq` seeks near it instead of reading the file from the start.
const MARK_EVERY: u64 = 256;
/// Least time between two throttled `state.json` writes.
const STATE_INTERVAL: Duration = Duration::from_secs(1);

/// A job's events, state and control, in memory and on the drive.
#[derive(Debug)]
pub(crate) struct Record {
    dir: PathBuf,
    /// `seq` the next event gets.
    next_seq: u64,
    /// `marks[k]`: offset in `events.jsonl` of the line with
    /// `seq == k * MARK_EVERY + 1`.
    marks: Vec<u64>,
    /// Bytes of `events.jsonl` known to hold whole, `fsync`ed lines.
    durable_len: u64,
    /// Highest `seq` in those bytes.
    durable_seq: u64,
    /// Events not yet on the drive, in `seq` order.
    pending: Vec<Event>,
    state: JobState,
    state_dirty: bool,
    last_state_write: Option<Instant>,
    control: Control,
    control_dirty: bool,
    /// Carries the highest `seq` to long-polling readers.
    notify: watch::Sender<u64>,
}

impl Record {
    /// Loads the record of the job in `dir`. A missing `state.json` is a
    /// queued job; a last `events.jsonl` line that does not parse (a crash
    /// mid-write) is cut off so the next append starts on a fresh line.
    ///
    /// # Errors
    ///
    /// An unreadable `events.jsonl` or `state.json`.
    pub(crate) fn load(dir: &Path, spec: &JobSpec) -> Result<Self> {
        let state = match std::fs::read(dir.join(STATE_FILE)) {
            Ok(bytes) => serde_json::from_slice::<JobState>(&bytes)
                .with_context(|| format!("{} is not a job state", dir.join(STATE_FILE).display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => JobState::new(spec, now_ms()),
            Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.join(STATE_FILE).display())),
        };
        let control =
            std::fs::read_to_string(dir.join(CONTROL_FILE)).map_or(Control::Run, |text| Control::parse(&text));
        let (marks, durable_len, durable_seq) = scan_events(&dir.join(EVENTS_FILE))?;
        let last = durable_seq.max(state.last_seq);
        let (notify, _) = watch::channel(last);
        Ok(Self {
            dir: dir.to_path_buf(),
            next_seq: last + 1,
            marks,
            durable_len,
            durable_seq,
            pending: Vec::new(),
            state,
            state_dirty: false,
            last_state_write: None,
            control,
            control_dirty: false,
            notify,
        })
    }

    /// The job directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The live state.
    pub(crate) fn state(&self) -> &JobState {
        &self.state
    }

    /// Changes the live state; call [`Record::save_state`] after.
    pub(crate) fn state_mut(&mut self) -> &mut JobState {
        self.state_dirty = true;
        &mut self.state
    }

    /// The current control word.
    pub(crate) fn control(&self) -> Control {
        self.control
    }

    /// Sets the control word and tries to persist it.
    pub(crate) fn set_control(&mut self, control: Control) {
        self.control = control;
        self.control_dirty = true;
        self.flush_control();
    }

    /// A receiver that sees the highest `seq` change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }

    /// The highest `seq` assigned.
    pub(crate) fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Appends an event: assigns its `seq`, writes and `fsync`s it (or holds
    /// it while the drive is away), records it in the state, and wakes
    /// readers. Phase events also write `state.json` at once.
    pub(crate) fn emit(&mut self, body: EventBody) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let force = matches!(body, EventBody::Phase { .. } | EventBody::Round { .. });
        self.pending.push(Event {
            seq,
            ts_ms: now_ms(),
            body,
        });
        self.state.last_seq = seq;
        self.state_dirty = true;
        self.flush_events();
        self.save_state(force);
        self.notify.send_replace(seq);
        seq
    }

    /// Writes `state.json` (tmp + `fsync` + rename) if it changed, at most
    /// once per [`STATE_INTERVAL`] unless `force`. A failed write stays dirty
    /// and is retried by the next call.
    pub(crate) fn save_state(&mut self, force: bool) {
        self.flush_control();
        if !self.state_dirty {
            return;
        }
        if !force && self.last_state_write.is_some_and(|at| at.elapsed() < STATE_INTERVAL) {
            return;
        }
        self.state.updated_at_ms = now_ms();
        let _phase = rdirstat_copy::phase::enter(rdirstat_copy::phase::Phase::Journal);
        if write_atomic(&self.dir, STATE_FILE, &self.state).is_ok() {
            self.state_dirty = false;
            self.last_state_write = Some(Instant::now());
        }
    }

    /// Writes held events, then held state and control: called by a runner
    /// when it sees the drive come back.
    pub(crate) fn flush(&mut self) {
        self.flush_events();
        self.save_state(true);
    }

    fn flush_control(&mut self) {
        if self.control_dirty && write_text_atomic(&self.dir, CONTROL_FILE, self.control.word()).is_ok() {
            self.control_dirty = false;
        }
    }

    fn flush_events(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let _phase = rdirstat_copy::phase::enter(rdirstat_copy::phase::Phase::Journal);
        let path = self.dir.join(EVENTS_FILE);
        let result = (|| -> std::io::Result<()> {
            // `create` but never `create_dir`: a missing job directory means
            // the drive is away, and the event waits.
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            if file.metadata()?.len() != self.durable_len {
                // A write that failed half way (the drive went away under it).
                file.set_len(self.durable_len)?;
            }
            let mut offset = self.durable_len;
            let mut buf = Vec::new();
            let mut marks = Vec::new();
            for event in &self.pending {
                if (event.seq - 1) % MARK_EVERY == 0 {
                    marks.push((event.seq, offset));
                }
                let start = buf.len();
                serde_json::to_writer(&mut buf, event).map_err(std::io::Error::other)?;
                buf.push(b'\n');
                offset += (buf.len() - start) as u64;
            }
            file.write_all(&buf)?;
            file.sync_data()?;
            for (seq, at) in marks {
                set_mark(&mut self.marks, seq, at);
            }
            self.durable_len = offset;
            Ok(())
        })();
        if result.is_ok() {
            self.durable_seq = self.pending.last().map_or(self.durable_seq, |event| event.seq);
            self.pending.clear();
        }
    }

    /// Up to `limit` events with `seq > after`, from the drive and then from
    /// those held in memory.
    ///
    /// # Errors
    ///
    /// `events.jsonl` cannot be read (the drive is away and the events asked
    /// for are only on it).
    pub(crate) fn events_after(&self, after: u64, limit: usize) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if after < self.durable_seq && limit > 0 {
            let path = self.dir.join(EVENTS_FILE);
            let mut file = std::fs::File::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
            let mark = usize::try_from(after / MARK_EVERY).unwrap_or(usize::MAX);
            let start = self.marks.get(mark).copied().unwrap_or(0);
            file.seek(SeekFrom::Start(start))?;
            let reader = std::io::BufReader::new(file.take(self.durable_len.saturating_sub(start)));
            for line in reader.lines() {
                let line = line?;
                let Ok(event) = serde_json::from_str::<Event>(&line) else {
                    continue;
                };
                if event.seq > after {
                    out.push(event);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        for event in &self.pending {
            if out.len() >= limit {
                break;
            }
            if event.seq > after {
                out.push(event.clone());
            }
        }
        Ok(out)
    }
}

fn set_mark(marks: &mut Vec<u64>, seq: u64, offset: u64) {
    let index = usize::try_from((seq - 1) / MARK_EVERY).unwrap_or(usize::MAX);
    if index == marks.len() {
        marks.push(offset);
    } else if let Some(slot) = marks.get_mut(index) {
        *slot = offset;
    }
}

/// Reads `events.jsonl` once: its marks, the length of its whole lines, and
/// its last `seq`. A torn last line is cut off.
fn scan_events(path: &Path) -> Result<(Vec<u64>, u64, u64)> {
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0, 0)),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let total = file.metadata()?.len();
    let mut reader = std::io::BufReader::new(&file);
    let mut marks = Vec::new();
    let (mut offset, mut good_len, mut last_seq) = (0_u64, 0_u64, 0_u64);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let whole = line.last() == Some(&b'\n');
        match serde_json::from_slice::<Event>(&line) {
            Ok(event) if whole => {
                if (event.seq - 1) % MARK_EVERY == 0 {
                    set_mark(&mut marks, event.seq, offset);
                }
                last_seq = last_seq.max(event.seq);
                offset += read as u64;
                good_len = offset;
            }
            _ if whole => {
                // A whole line that is not an event (a newer kind this build
                // does not know still parses; this is garbage). Keep it and
                // move on: readers skip it too.
                offset += read as u64;
                good_len = offset;
            }
            _ => break,
        }
    }
    drop(reader);
    if good_len < total {
        file.set_len(good_len)
            .with_context(|| format!("cannot cut the torn last line of {}", path.display()))?;
    }
    Ok((marks, good_len, last_seq))
}

/// Writes `value` as pretty JSON to `dir/name` through a temp file in `dir`.
pub(crate) fn write_atomic<T: serde::Serialize>(dir: &Path, name: &str, value: &T) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_bytes_atomic(dir, name, &bytes)
}

fn write_text_atomic(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    write_bytes_atomic(dir, name, format!("{text}\n").as_bytes())
}

fn write_bytes_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name)).inspect_err(|_| {
        let _ignored = std::fs::remove_file(&tmp);
    })
}
