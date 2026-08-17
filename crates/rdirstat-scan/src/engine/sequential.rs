//! The single-threaded scheduler.
//!
//! One explicit stack, never recursion: a 4096-deep `node_modules` chain is a
//! real input and a stack overflow is not a diagnosable failure. There is no
//! channel, no thread, and no shared mutable state, which is exactly why it is
//! the reference the parallel engine is diffed against.

use std::ops::ControlFlow;

use rdirstat_core::{ScanError, ScanState};

use crate::builder::{ScanBuilder, path_bytes};
use crate::engine::{Completion, EngineContext, check_memory};
use crate::progress::{Counters, ProgressPublisher};
use crate::reader::{DirHandle, ReadDirError};

/// Walks everything reachable from `root`.
///
/// # Errors
///
/// Only fatal failures: an exhausted arena or the memory ceiling. A directory
/// that cannot be read is recorded on its node and the walk continues.
pub(crate) fn run(
    builder: &mut ScanBuilder<'_>,
    root: DirHandle,
    context: &EngineContext<'_>,
    publisher: &mut ProgressPublisher<'_>,
) -> Result<Completion, ScanError> {
    let mut stack: Vec<DirHandle> = Vec::with_capacity(64);
    let mut discovered: Vec<DirHandle> = Vec::with_capacity(64);
    stack.push(root);

    while let Some(task) = stack.pop() {
        if context.cancel.is_cancelled() {
            return Ok(Completion::Cancelled);
        }
        let pending = u64::try_from(stack.len().saturating_add(1)).unwrap_or(u64::MAX);
        Counters::set(&context.counters.pending_dirs, pending);
        context.current.offer(path_bytes(task.path()));

        discovered.clear();
        let mut fatal: Option<ScanError> = None;
        let outcome = context
            .reader
            .read_batches(
                &task,
                context.cancel,
                &mut |batch| match builder.absorb_batch(&task, batch, &mut discovered) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => {
                        fatal = Some(error.clone());
                        ControlFlow::Break(error)
                    }
                },
            );

        match outcome {
            Ok(()) => builder.finish_directory(&task, None),
            Err(ReadDirError::Cancelled) => return Ok(Completion::Cancelled),
            Err(ReadDirError::Aborted(_)) => {
                return Err(fatal.unwrap_or_else(|| ScanError::Arena(arena_unknown())));
            }
            Err(other) => builder.finish_directory(&task, Some(&other)),
        }

        stack.append(&mut discovered);

        let pending = u64::try_from(stack.len()).unwrap_or(u64::MAX);
        Counters::set(&context.counters.pending_dirs, pending);
        let footprint = builder.footprint(pending);
        check_memory(footprint, context.memory_limit)?;
        publisher.maybe_publish(ScanState::Scanning, context.counters, context.current, footprint);
    }

    Counters::set(&context.counters.pending_dirs, 0);
    Ok(Completion::Finished)
}

/// Only reachable if `Aborted` arrived without the sink having stored the
/// error, which the closure above makes impossible; it keeps the branch total
/// without an `unwrap`.
fn arena_unknown() -> rdirstat_core::ArenaError {
    rdirstat_core::ArenaError::UnknownNode {
        node: rdirstat_core::NodeId::NONE,
    }
}
