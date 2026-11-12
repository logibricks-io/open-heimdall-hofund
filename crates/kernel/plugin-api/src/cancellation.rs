//! Request-local cancellation causes shared by a caller and the kernel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Marks that the caller stopped waiting on its own deadline.
///
/// Attach a clone to an in-process HTTP request's extensions before calling
/// the gateway. Keep another clone and call [`Self::stop`] immediately before
/// dropping the in-flight call. The gateway reads the shared state if its
/// request tracker is dropped, distinguishing this from a disconnected client.
#[derive(Clone, Debug, Default)]
pub struct StoppedWaiting {
    stopped: Arc<AtomicBool>,
}

impl StoppedWaiting {
    /// A request-local marker, initially not stopped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that the caller is about to stop waiting.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Whether [`Self::stop`] has been called on any clone.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}
