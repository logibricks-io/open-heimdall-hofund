//! Helpers a plugin may use without depending on the kernel.
//!
//! [`transport::Call`] sends on the host's pooled client. The client itself
//! lives in `hofund-plugin-host`. This crate re-exports [`tracing`],
//! [`macro@async_trait`], and [`apply_quirks`] so a plugin does not name those
//! crates. The API does not depend on quirks.
//!
//! [`sigv4`] signs one AWS request. [`oauth`] parses a token response and
//! builds a form body. [`page`] reads cursors and walks bounded model lists.
//! PKCE, the loopback callback, and device flow are not here. [`transport::Call`] still
//! buffers the response body. Streaming adapters use the host's pooled client.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub use async_trait::async_trait;
pub use hofund_quirks::apply as apply_quirks;
pub use tracing;

pub mod classify;
pub mod oauth;
pub mod page;
pub mod register;
pub mod sigv4;
pub mod transport;

pub use oauth::{AccessToken, TokenError, form_body, parse_access_token, refresh_form};
pub use page::{
    BODY_CAP, ERROR_RUNES, LIST_BUDGET, MAX_PAGES, PAGE_LIMIT, PageError, after_id, cursor_pages,
    gemini_models, gemini_models_with_dropped, json_error_message, next_cursor, string_items,
};
pub use register::Contribution;
pub use transport::{Call, HttpRequest, HttpResponse, Reply, text_response};

#[cfg(test)]
mod tests {
    #[test]
    fn tracing_re_export_is_the_facade() {
        let _ = tracing::Level::INFO;
    }
}
