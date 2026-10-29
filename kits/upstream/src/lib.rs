//! Test support for upstream plugins.
//!
//! This crate is unpublished and is only a dev-dependency of an upstream
//! plugin. [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends, and [`runtime_host`] builds one with the pooled HTTP client for
//! tests against a local mock server. [`check_list_and_probe`] runs the
//! mandatory model-list and probe contract against local mock servers.
//! [`block_on`] drives an async plugin call without a plugin depending on
//! tokio.

mod conformance;

pub use conformance::{check_deployments, check_list_and_probe};
pub use hofund_plugin_host::{
    HttpLimits, RecordingHttp, RecordingObserve, RecordingProcess, RecordingScheduler,
    RecordingSecrets, TestHost, runtime_host, test_host,
};

/// Drives `future` on a current-thread runtime.
///
/// Plugin tests use this so the plugin crate does not depend on tokio.
pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
        .block_on(future)
}
