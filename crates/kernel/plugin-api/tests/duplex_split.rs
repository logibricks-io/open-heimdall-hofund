//! Duplex session split must not leak the shared guard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use hofund_plugin_api::kernel::{DuplexGuard, DuplexWriterBackend};
use hofund_plugin_api::{BodyStream, DuplexSession, DuplexWriter, PluginError};

struct SplitGuard {
    finished: AtomicBool,
    split: AtomicBool,
}

impl DuplexGuard for SplitGuard {
    fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }

    fn mark_split(&self) {
        self.split.store(true, Ordering::SeqCst);
    }

    fn session_should_abort_on_drop(&self) -> bool {
        !self.split.load(Ordering::SeqCst)
    }

    fn abort_if_unfinished(&self) {}
}

struct Sink;

#[async_trait]
impl DuplexWriterBackend for Sink {
    async fn write(&mut self, _bytes: Bytes) -> Result<(), PluginError> {
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[test]
fn into_split_releases_the_guard_when_both_halves_drop() {
    let guard: Arc<dyn DuplexGuard> = Arc::new(SplitGuard {
        finished: AtomicBool::new(false),
        split: AtomicBool::new(false),
    });
    let weak = Arc::downgrade(&guard);
    let session = DuplexSession::kernel_new(
        http::StatusCode::OK,
        http::HeaderMap::new(),
        BodyStream::from_stream(stream::empty()),
        DuplexWriter::kernel_new(Box::new(Sink), Arc::clone(&guard)),
        guard,
    );
    let (reader, writer) = session.into_split();
    drop(reader);
    drop(writer);
    assert!(weak.upgrade().is_none());
}
