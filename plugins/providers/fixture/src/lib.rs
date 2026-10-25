//! Fixture provider compiled by `cargo xtask check-plugins`.
//!
//! The crate depends on [`hofund_plugin_api`] without the `kernel` feature.
//! It exists so the plugin CI job compiles a real package instead of an empty
//! `plugins/` tree.

#![deny(missing_docs)]

/// Reports that the fixture package linked the public plugin API.
#[must_use]
pub fn fixture_ready() -> bool {
    hofund_plugin_api::API == "1"
}
