//! Test support for subscription plugins.
//!
//! This crate is unpublished and is only a dev-dependency of a subscription
//! plugin. [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends. [`block_on`], [`join_all`], [`drive_until`], and [`poll_once`] drive async plugin
//! calls without a plugin depending on tokio. [`FakeClaude`] is an in-process stand-in for the Claude Code binary: it records argv, env, and stdin, checks optional assertions, and returns a scripted stdout fixture. Conformance tests land in PR D.

mod fake_claude;
pub use fake_claude::{FakeClaude, FakeClaudeError, RunRecord};

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

mod servers;

pub use hofund_plugin_host::{
    PipedScript, PipedSpawn, RecordingHttp, RecordingObserve, RecordingProcess, RecordingScheduler,
    RecordingSecrets, TestHost, test_host,
};
pub use servers::{
    HeaderPark, Http1Response, Http1Server, Http2Response, Http2Server, header_park,
};

/// Sends one HTTP/1.1 GET to a test sign-in listener and returns its reply.
///
/// # Errors
///
/// Returns an I/O error when the listener is closed or the exchange fails.
pub async fn loopback_get(port: u16, target: &str) -> std::io::Result<(u16, String)> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    let status = reply
        .get(9..12)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    Ok((status, reply))
}

/// Runs `future` on a paused Tokio clock so tests can [`advance_time`].
///
/// # Panics
///
/// Panics when the runtime cannot be built.
pub fn block_on_paused<T>(future: impl Future<Output = T>) -> T {
    block_on(async {
        tokio::time::pause();
        future.await
    })
}

/// Advances the paused Tokio clock used by [`block_on_paused`].
pub async fn advance_time(duration: std::time::Duration) {
    tokio::time::advance(duration).await;
}

/// Drives `future` on a current-thread runtime.
///
/// Timers and sockets work, and one-shot jobs the plugin starts run on this
/// runtime while `future` waits.
///
/// # Panics
///
/// Panics when the runtime cannot be built.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

/// Runs every future concurrently and returns their outputs in order.
pub async fn join_all<T>(futures: Vec<Pin<Box<dyn Future<Output = T> + '_>>>) -> Vec<T> {
    futures_util::future::join_all(futures).await
}

/// Polls `future` with a real runtime waker until `until` completes.
///
/// Use this when a call must make progress (for example recording HTTP)
/// before the test acts, instead of a fixed `yield_times` budget.
///
/// # Panics
///
/// Panics if `future` completes first.
pub async fn drive_until<F, U, T>(future: Pin<&mut F>, until: U)
where
    F: Future<Output = T> + ?Sized,
    U: Future<Output = ()>,
    T: std::fmt::Debug,
{
    if let Some(result) = drive_until_output(future, until).await {
        panic!("future finished early: {result:?}")
    }
}

async fn drive_until_output<F, U, T>(future: Pin<&mut F>, until: U) -> Option<T>
where
    F: Future<Output = T> + ?Sized,
    U: Future<Output = ()>,
{
    let until = std::pin::pin!(until);
    match futures_util::future::select(future, until).await {
        futures_util::future::Either::Left((result, _)) => Some(result),
        futures_util::future::Either::Right(((), _)) => None,
    }
}

/// Waits until the `count`th HTTP/2 request is recorded.
///
/// The wait always leaves a yield queued, so a paused runtime cannot expire a
/// client's header wait while loopback accept and handshake I/O is pending.
///
/// # Panics
///
/// Panics if `count` is zero.
pub async fn wait_for_http2_request(server: &Http2Server, count: usize) {
    assert!(count > 0, "HTTP/2 request counts start at one");
    while server.requests().len() < count {
        tokio::task::yield_now().await;
    }
}

/// Drives `future` until the `count`th HTTP/2 request is recorded.
///
/// # Panics
///
/// Panics if `count` is zero or `future` completes first.
pub async fn drive_until_http2_request<F, T>(
    future: Pin<&mut F>,
    server: &Http2Server,
    count: usize,
) where
    F: Future<Output = T> + ?Sized,
{
    if drive_until_output(future, wait_for_http2_request(server, count))
        .await
        .is_some()
    {
        panic!("future finished early");
    }
}

/// Waits until the `count`th HTTP/2 request records client DATA.
///
/// Cursor writes its opening DATA only after response headers arrive, so this
/// condition proves that the HTTP/2 path was selected instead of HTTP/1 fallback.
/// Like [`wait_for_http2_request`], it prevents paused-clock auto-advance.
///
/// # Panics
///
/// Panics if `count` is zero.
pub async fn wait_for_http2_opening(server: &Http2Server, count: usize) {
    assert!(count > 0, "HTTP/2 request counts start at one");
    loop {
        if server
            .requests()
            .get(count - 1)
            .is_some_and(|request| !request.data().is_empty())
        {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// Drives `future` until the `count`th HTTP/2 request records client DATA.
///
/// # Panics
///
/// Panics if `count` is zero or `future` completes first.
pub async fn drive_until_http2_opening<F, T>(
    future: Pin<&mut F>,
    server: &Http2Server,
    count: usize,
) where
    F: Future<Output = T> + ?Sized,
{
    if drive_until_output(future, wait_for_http2_opening(server, count))
        .await
        .is_some()
    {
        panic!("future finished early");
    }
}

/// Drives `future` to completion without auto-advancing a paused clock.
///
/// Use this after a scripted request is recorded but before its loopback
/// response has settled. The queued yield keeps the runtime from jumping to a
/// retry or deadline timer while the response is still in flight.
pub async fn drive_to_completion<F>(future: F) -> F::Output
where
    F: Future,
{
    let mut future = std::pin::pin!(future);
    loop {
        let yield_now = std::pin::pin!(tokio::task::yield_now());
        match futures_util::future::select(future.as_mut(), yield_now).await {
            futures_util::future::Either::Left((output, _)) => return output,
            futures_util::future::Either::Right(((), _)) => {}
        }
    }
}

/// Drives `future` through the HTTP/2 opening gate and then to completion.
///
/// An early result is returned as-is. This is needed when a client queues its
/// opening write and returns before the server records the corresponding DATA.
pub async fn drive_http2_to_completion<F>(
    future: F,
    server: &Http2Server,
    count: usize,
) -> F::Output
where
    F: Future,
{
    let mut future = std::pin::pin!(future);
    match futures_util::future::select(
        future.as_mut(),
        std::pin::pin!(wait_for_http2_opening(server, count)),
    )
    .await
    {
        futures_util::future::Either::Left((output, _)) => output,
        futures_util::future::Either::Right(((), _)) => drive_to_completion(future).await,
    }
}

/// Polls `future` once with a no-op waker.
///
/// A test uses it to start a call, then drops the future to act as a caller
/// that went away.
pub fn poll_once<T>(future: Pin<&mut (dyn Future<Output = T> + '_)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Yields to the runtime `times` times, so spawned jobs make progress.
pub async fn yield_times(times: usize) {
    for _ in 0..times {
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::net::TcpStream;

    use super::{
        Http2Response, Http2Server, block_on, block_on_paused, join_all, poll_once, yield_times,
    };

    #[test]
    fn join_all_keeps_order() {
        let out = block_on(join_all(vec![
            Box::pin(async { 1 }),
            Box::pin(async {
                yield_times(2).await;
                2
            }),
            Box::pin(async { 3 }),
        ]));
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn poll_once_stops_at_the_first_pending_point() {
        let steps = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&steps);
        block_on(async move {
            let mut future = pin!(async {
                seen.fetch_add(1, Ordering::SeqCst);
                yield_times(1).await;
                seen.fetch_add(1, Ordering::SeqCst);
                7
            });
            assert!(poll_once(future.as_mut()).is_pending());
            assert_eq!(steps.load(Ordering::SeqCst), 1);
            assert_eq!(poll_once(future.as_mut()), Poll::Ready(7));
            assert_eq!(steps.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn yield_times_lets_spawned_work_run() {
        let ran = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&ran);
        block_on(async move {
            tokio::spawn(async move {
                seen.fetch_add(1, Ordering::SeqCst);
            });
            assert_eq!(ran.load(Ordering::SeqCst), 0);
            yield_times(3).await;
            assert_eq!(ran.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn http2_gate_prevents_paused_header_wait_from_winning_before_accept() {
        block_on_paused(async {
            let server = Http2Server::start(vec![Http2Response::new(200)]).await;
            let attempt = async {
                let tcp = TcpStream::connect(server.base_url().trim_start_matches("http://"))
                    .await
                    .expect("connect");
                let (mut client, connection) =
                    h2::client::handshake(tcp).await.expect("client handshake");
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                let request = http::Request::builder()
                    .method("POST")
                    .uri(format!("{}/v1/bidi", server.base_url()))
                    .body(())
                    .expect("request");
                let (response, mut send) =
                    client.send_request(request, false).expect("send request");
                send.send_data(Bytes::from_static(b"opening"), true)
                    .expect("opening DATA");
                let response = tokio::time::timeout(Duration::from_secs(15), response)
                    .await
                    .expect("paused header wait must not precede loopback accept")
                    .expect("response headers");
                assert_eq!(response.status(), 200);
            };
            super::drive_http2_to_completion(attempt, &server, 1).await;
            assert_eq!(server.requests()[0].data(), [b"opening".to_vec()]);
        });
    }
}
