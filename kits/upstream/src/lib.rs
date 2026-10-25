//! Test support for upstream plugins.
//!
//! This crate is unpublished and is only a dev-dependency of an upstream
//! plugin. [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends. Conformance tests land in PR D.

pub use hofund_plugin_host::{
    RecordingHttp, RecordingObserve, RecordingProcess, RecordingScheduler, RecordingSecrets,
    TestHost, test_host,
};
