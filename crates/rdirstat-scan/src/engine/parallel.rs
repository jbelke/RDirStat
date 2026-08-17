//! The bounded parallel scheduler.
//!
//! A fixed pool of readers pulls independent directories through bounded
//! `crossbeam-channel`s; one supervisor thread owns the builder and is still
//! the only assigner of [`NodeId`](rdirstat_core::NodeId)s. Termination is an
//! **exact** count — directories queued plus directories in flight — never a
//! heuristic over approximate counters.
//!
//! Two bounds make the memory story real:
//!
//! * the task and result channels have fixed capacity, so a slow builder
//!   blocks the readers instead of growing an O(node count) backlog;
//! * a worker splits one directory into chunks of at most
//!   [`CHUNK_ENTRIES`], so a directory with ten million children does not
//!   become one ten-million-element message.
//!
//! The supervisor `select`s over "send a task" and "receive a chunk" together,
//! which is what stops the classic bounded-channel deadlock where the
//! supervisor blocks sending while every worker blocks replying.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::thread;

use crossbeam_channel::{Receiver, Select, Sender, bounded};
use rdirstat_core::{DisplayPath, ErrorClass, Operation, ScanError, ScanState};

use crate::builder::{ScanBuilder, path_bytes};
use crate::cancel::CancelToken;
use crate::engine::{Completion, EngineContext, check_memory};
use crate::entry::{EntryError, RawEntry};
use crate::progress::{Counters, CurrentDir, PROGRESS_INTERVAL, ProgressPublisher};
use crate::reader::{DirHandle, DirReader, ReadDirError};

/// Entries a worker accumulates before flushing a partial result.
pub const CHUNK_ENTRIES: usize = 4_096;

/// Task-channel capacity per worker.
const TASK_SLOTS_PER_WORKER: usize = 2;

/// Result-channel capacity per worker.
const RESULT_SLOTS_PER_WORKER: usize = 4;

/// One partial or final result for a directory.
#[derive(Debug)]
struct Chunk {
    sequence: u64,
    entries: Vec<RawEntry>,
    errors: Vec<EntryError>,
    /// `Some` on the last chunk for this directory.
    finished: Option<Result<(), ReadDirError>>,
}

/// The error a worker breaks with when the supervisor has already gone away.
/// It is never observed: the only reader of it is a channel nobody is holding.
fn abandoned() -> ScanError {
    ScanError::Io {
        path: DisplayPath::from_bytes(b""),
        operation: Operation::ReadDir,
        class: ErrorClass::Other,
        os_code: 0,
    }
}

/// Walks everything reachable from `root` with `workers` reader threads.
///
/// # Errors
///
/// Only fatal failures: an exhausted arena or the memory ceiling.
pub(crate) fn run(
    builder: &mut ScanBuilder<'_>,
    root: DirHandle,
    context: &EngineContext<'_>,
    publisher: &mut ProgressPublisher<'_>,
    workers: usize,
) -> Result<Completion, ScanError> {
    let workers = workers.max(1);
    let (task_tx, task_rx) = bounded::<(u64, DirHandle)>(workers * TASK_SLOTS_PER_WORKER);
    let (result_tx, result_rx) = bounded::<Chunk>(workers * RESULT_SLOTS_PER_WORKER);

    thread::scope(|scope| {
        for _ in 0..workers {
            let tasks = task_rx.clone();
            let results = result_tx.clone();
            let reader = context.reader;
            let cancel = context.cancel;
            let current = context.current;
            scope.spawn(move || worker(reader, cancel, current, &tasks, &results));
        }
        drop(task_rx);
        drop(result_tx);

        let outcome = supervise(builder, root, context, publisher, &task_tx, &result_rx);

        // Close the task channel so the workers exit, then drain whatever they
        // still have in flight so nobody is blocked on a full result channel.
        drop(task_tx);
        while result_rx.recv().is_ok() {}
        outcome
    })
}

fn supervise(
    builder: &mut ScanBuilder<'_>,
    root: DirHandle,
    context: &EngineContext<'_>,
    publisher: &mut ProgressPublisher<'_>,
    task_tx: &Sender<(u64, DirHandle)>,
    result_rx: &Receiver<Chunk>,
) -> Result<Completion, ScanError> {
    let mut queue: VecDeque<DirHandle> = VecDeque::with_capacity(64);
    let mut in_flight: HashMap<u64, DirHandle> = HashMap::new();
    let mut discovered: Vec<DirHandle> = Vec::with_capacity(64);
    let mut next_sequence = 0_u64;
    queue.push_back(root);

    loop {
        if context.cancel.is_cancelled() {
            return Ok(Completion::Cancelled);
        }
        // The exact termination condition: nothing queued and nothing out with
        // a worker. Never inferred from a progress counter.
        if queue.is_empty() && in_flight.is_empty() {
            break;
        }

        let pending = u64::try_from(queue.len().saturating_add(in_flight.len())).unwrap_or(u64::MAX);
        Counters::set(&context.counters.pending_dirs, pending);
        Counters::set(
            &context.counters.result_queue_depth,
            u64::try_from(result_rx.len()).unwrap_or(u64::MAX),
        );
        let footprint = builder.footprint(pending);
        check_memory(footprint, context.memory_limit)?;
        publisher.maybe_publish(ScanState::Scanning, context.counters, context.current, footprint);

        let mut select = Select::new();
        let send_index = if queue.is_empty() {
            None
        } else {
            Some(select.send(task_tx))
        };
        let recv_index = select.recv(result_rx);

        let Ok(operation) = select.select_timeout(PROGRESS_INTERVAL) else {
            continue;
        };
        let index = operation.index();

        if Some(index) == send_index {
            let Some(handle) = queue.pop_front() else {
                continue;
            };
            let sequence = next_sequence;
            next_sequence = next_sequence.saturating_add(1);
            if operation.send(task_tx, (sequence, handle.clone())).is_err() {
                // Every worker is gone; nothing else can make progress.
                break;
            }
            in_flight.insert(sequence, handle);
            continue;
        }

        if index == recv_index {
            let Ok(chunk) = operation.recv(result_rx) else {
                break;
            };
            // Take the handle out for the duration so the builder can borrow it
            // while the map stays untouched; put it back unless this was the
            // directory's last chunk.
            let Some(handle) = in_flight.remove(&chunk.sequence) else {
                continue;
            };
            discovered.clear();
            let batch = crate::entry::RawEntryBatch::new(&chunk.entries, &chunk.errors);
            let absorbed = builder.absorb_batch(&handle, batch, &mut discovered);
            queue.extend(discovered.drain(..));
            absorbed?;

            match chunk.finished {
                None => {
                    in_flight.insert(chunk.sequence, handle);
                }
                Some(Ok(())) => builder.finish_directory(&handle, None),
                Some(Err(ReadDirError::Cancelled)) => return Ok(Completion::Cancelled),
                Some(Err(ReadDirError::Aborted(error))) => return Err(error),
                Some(Err(other)) => builder.finish_directory(&handle, Some(&other)),
            }
        }
    }

    Counters::set(&context.counters.pending_dirs, 0);
    Counters::set(&context.counters.result_queue_depth, 0);
    Ok(Completion::Finished)
}

fn worker(
    reader: &dyn DirReader,
    cancel: &CancelToken,
    current: &CurrentDir,
    tasks: &Receiver<(u64, DirHandle)>,
    results: &Sender<Chunk>,
) {
    while let Ok((sequence, handle)) = tasks.recv() {
        if cancel.is_cancelled() {
            let _ignored = results.send(Chunk {
                sequence,
                entries: Vec::new(),
                errors: Vec::new(),
                finished: Some(Err(ReadDirError::Cancelled)),
            });
            return;
        }
        current.offer(path_bytes(handle.path()));

        let mut entries: Vec<RawEntry> = Vec::new();
        let mut errors: Vec<EntryError> = Vec::new();
        let mut disconnected = false;

        let outcome = reader.read_batches(&handle, cancel, &mut |batch| {
            entries.extend_from_slice(batch.entries());
            errors.extend_from_slice(batch.errors());
            if entries.len() >= CHUNK_ENTRIES {
                let chunk = Chunk {
                    sequence,
                    entries: core::mem::take(&mut entries),
                    errors: core::mem::take(&mut errors),
                    finished: None,
                };
                if results.send(chunk).is_err() {
                    disconnected = true;
                    return ControlFlow::Break(abandoned());
                }
            }
            ControlFlow::Continue(())
        });

        if disconnected {
            return;
        }
        let chunk = Chunk {
            sequence,
            entries,
            errors,
            finished: Some(outcome),
        };
        if results.send(chunk).is_err() {
            return;
        }
    }
}
