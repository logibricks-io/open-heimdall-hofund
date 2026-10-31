//! Deadlines shared by the daemon integration tests.
//!
//! A localhost request or a graceful shutdown that stalls (seen on Windows
//! CI, issues #409, #462 and #467) used to hang until the CI job was
//! cancelled. These helpers make such a stall fail within about a minute and
//! name the step that stalled.

#![allow(dead_code)]

use std::future::Future;
use std::time::Duration;

/// Upper bound for one request against the local daemon.
pub const REQUEST: Duration = Duration::from_secs(60);

/// Upper bound for a graceful daemon shutdown.
pub const SHUTDOWN: Duration = Duration::from_secs(60);

/// A client for the local daemon: every request has a deadline and no idle
/// connection is kept, so a pooled keep-alive connection can neither be
/// reused after the server dropped it nor hold a graceful shutdown open.
pub fn client() -> reqwest::Client {
    builder().build().expect("test client")
}

/// [`client`]'s settings as a builder, for tests that add their own.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(REQUEST)
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(0)
}

/// Awaits `future`, panicking with `what` when it takes longer than `limit`.
pub async fn within<F: Future>(what: &str, limit: Duration, future: F) -> F::Output {
    match tokio::time::timeout(limit, future).await {
        Ok(output) => output,
        Err(_) => panic!("{what} did not finish within {limit:?}"),
    }
}

/// Shuts the daemon down, failing the test instead of hanging when the
/// graceful drain stalls.
pub async fn shutdown(running: hofund_daemon::Running) {
    within("daemon shutdown", SHUTDOWN, running.shutdown()).await;
}
