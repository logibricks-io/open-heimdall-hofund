//! Test support for agent plugins.
//!
//! This crate is unpublished and is only a dev-dependency of an agent plugin.
//! [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends. [`conformance`] replays compiled descriptions against golden
//! files without network access. Shared `all/` cases run on every OS, while
//! `<os>/` cases run only on that OS; regeneration deduplicates identical
//! three-OS trees into `all/`. [`MemoryCredentialStore`] stands in for the
//! OS credential store.

mod conformance;
mod credential;

pub use conformance::{conformance, parse_jsonc};
pub use credential::MemoryCredentialStore;
pub use hofund_plugin_host::{
    RecordingHttp, RecordingObserve, RecordingProcess, RecordingScheduler, RecordingSecrets,
    TestHost, test_host,
};
