//! Test support for subscription plugins.
//!
//! This crate is unpublished and is only a dev-dependency of a subscription
//! plugin. [`test_host`] builds a `hofund_plugin_host::HostContext` with fake
//! backends. [`block_on`], [`join_all`], [`drive_until`], and [`poll_once`] drive async plugin
//! calls without a plugin depending on tokio. Conformance tests land in PR D.

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
    let until = std::pin::pin!(until);
    match futures_util::future::select(future, until).await {
        futures_util::future::Either::Left((result, _)) => {
            panic!("future finished early: {result:?}")
        }
        futures_util::future::Either::Right(((), _)) => {}
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

    use super::{block_on, join_all, poll_once, yield_times};

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
}
