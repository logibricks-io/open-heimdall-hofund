//! Helpers a plugin may use without depending on the kernel.
//!
//! [`transport::Call`] sends on the host's pooled client. The client itself
//! lives in `hofund-plugin-host`. This crate re-exports [`tracing`],
//! [`json`] (`serde_json`), [`macro@async_trait`], and [`apply_quirks`] so a
//! plugin does not name those crates. [`sleep`] waits on the host runtime. The
//! API does not depend on quirks.
//!
//! [`sigv4`] signs one AWS request. [`bedrock`] fits Anthropic betas for
//! Bedrock's Messages wire. [`oauth`] parses a token response and
//! builds a form body. [`page`] reads cursors and walks bounded model lists.
//! PKCE, the loopback callback, and device flow are not here. The host handle
//! streams a 2xx body. [`transport::Call::send`] still reads all of it, and
//! [`transport::Call::open`] returns it unread. [`wire`] sends Chat Completions
//! and Responses calls and keeps a stream streaming.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub use async_trait::async_trait;
pub use hofund_codec::plain_code_assist_tools;
pub use hofund_quirks::apply as apply_quirks;
pub use hofund_quirks::{refused_passthrough, without_passthrough};
/// Response headers, as [`wire::Sent::headers`] returns them.
pub use http::HeaderMap;
pub use serde_json as json;
pub use tracing;
/// The URL parser, so a plugin checks a URL the way the host does.
pub use url;

/// Waits `duration` on the host runtime, for a plugin that polls.
pub async fn sleep(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}

/// Runs `future` for at most `duration` on the host runtime. `None` means
/// the time ran out and `future` was dropped.
pub async fn timeout<F: Future>(duration: std::time::Duration, future: F) -> Option<F::Output> {
    tokio::time::timeout(duration, future).await.ok()
}

pub mod bedrock;
pub mod classify;
pub mod jwt;
pub mod oauth;
pub mod page;
pub mod probe;
pub mod process;
pub mod register;
pub mod sigv4;
pub mod time;
pub mod transport;
pub mod wire;

pub use oauth::{AccessToken, TokenError, form_body, parse_access_token, refresh_form};
pub use page::{
    BODY_CAP, ERROR_RUNES, LIST_BUDGET, MAX_PAGES, PAGE_LIMIT, PageError, PageErrorKind, after_id,
    cursor_pages, gemini_models, gemini_models_with_dropped, json_error_message, next_cursor,
    string_items,
};
pub use register::Contribution;
pub use transport::{Call, HttpRequest, HttpResponse, Reply, text_response};

#[cfg(test)]
mod tests {
    #[test]
    fn tracing_re_export_is_the_facade() {
        let _ = tracing::Level::INFO;
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_gives_up_only_when_time_runs_out() {
        let short = std::time::Duration::from_millis(5);
        assert_eq!(
            super::timeout(short, std::future::pending::<u8>()).await,
            None
        );
        assert_eq!(super::timeout(short, async { 7_u8 }).await, Some(7));
    }
}
