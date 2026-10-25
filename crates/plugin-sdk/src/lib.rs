//! Helpers a plugin may use without depending on the kernel.
//!
//! [`transport::Call`] sends on the host's pooled client. The client itself
//! lives in `hofund-plugin-host`. This crate re-exports [`tracing`],
//! [`macro@async_trait`], and [`apply_quirks`] so a plugin does not name those
//! crates. The API does not depend on quirks.
//!
//! `SigV4`, OAuth, and paging are not here yet.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub use async_trait::async_trait;
pub use hofund_quirks::apply as apply_quirks;
pub use tracing;

pub mod register;
pub mod transport;

pub use register::Contribution;
pub use transport::{Call, HttpRequest, HttpResponse, Reply, text_response};

#[cfg(test)]
mod tests {
    #[test]
    fn tracing_re_export_is_the_facade() {
        let _ = tracing::Level::INFO;
    }
}
