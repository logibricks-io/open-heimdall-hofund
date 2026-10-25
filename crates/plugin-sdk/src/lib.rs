//! Helpers a plugin may use without depending on the kernel.
//!
//! HTTP transports, `SigV4`, OAuth, retry, and paging move here in PR D. This
//! crate is the skeleton: it re-exports [`tracing`] and [`apply_quirks`] so a
//! plugin does not name those crates itself. The API does not depend on quirks.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub use hofund_quirks::apply as apply_quirks;
pub use tracing;

/// Transports for the five protocols. Empty until PR D moves them onto the pooled client.
pub mod transport {}

#[cfg(test)]
mod tests {
    #[test]
    fn tracing_re_export_is_the_facade() {
        let _ = tracing::Level::INFO;
    }
}
