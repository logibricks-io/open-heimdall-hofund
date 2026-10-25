//! Test support for subscription plugins.
//!
//! This crate is unpublished and is only a dev-dependency of a subscription
//! plugin. [`test_host`] builds a [`hofund_plugin_host::HostContext`] with fake
//! backends. Conformance tests land in PR D.

pub use hofund_plugin_host::test_host;
