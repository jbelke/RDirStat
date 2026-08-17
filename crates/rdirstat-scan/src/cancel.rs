//! Cooperative cancellation.
//!
//! One `AtomicBool` for the whole scan. It is checked at directory boundaries,
//! at least every [`CANCEL_CHECK_ENTRIES`] entries inside a reader, and in the
//! supervisor's channel selection (docs/02-SCANNER.md#progress-backpressure-and-cancellation).
//!
//! No design can promise a hard deadline for an uninterruptible remote
//! filesystem syscall, so "cancel acknowledged" and "resources closed" are
//! separate states in [`CancelState`](rdirstat_core::CancelState).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// How many entries a reader may enumerate between cancellation checks.
pub const CANCEL_CHECK_ENTRIES: u32 = 1_024;

/// A shared cancellation flag.
///
/// Cloning shares the flag. The store uses `Release` and the load uses
/// `Acquire` so that everything the cancelling thread did before requesting
/// cancellation is visible to the worker that observes it; the flag itself
/// would be correct under `Relaxed`, but the ordering is documented rather than
/// assumed.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_observes_the_same_flag() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled(), "cancellation is shared, not copied");
        token.cancel();
        assert!(clone.is_cancelled(), "cancel is idempotent");
    }
}
