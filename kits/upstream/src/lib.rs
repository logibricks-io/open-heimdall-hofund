//! Test support for upstream plugins.
//!
//! This crate is unpublished and is only a dev-dependency of an upstream
//! plugin. [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends. Mandatory conformance tests are later.
//! [`block_on`] drives an async plugin call without a plugin depending on tokio.

pub use hofund_plugin_host::{
    RecordingHttp, RecordingObserve, RecordingProcess, RecordingScheduler, RecordingSecrets,
    TestHost, test_host,
};

/// Drives `future` on a current-thread runtime.
///
/// Plugin tests use this so the plugin crate does not depend on tokio.
pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(future)
}
