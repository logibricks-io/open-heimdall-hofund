use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use futures_util::future::join_all;
use hofund_plugin_api::{AccountId, PluginError, PluginErrorKind};

use super::{NEGATIVE_TTL, TokenCache};
use crate::oauth::Clock;

/// A clock the test moves by hand.
struct ManualClock {
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().expect("clock") += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000) + self.monotonic()
    }

    fn monotonic(&self) -> Duration {
        *self.elapsed.lock().expect("clock")
    }
}

fn account(name: &'static str) -> AccountId {
    AccountId::from_static(name)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[tokio::test(start_paused = true)]
async fn token_cache_one_refresh_for_eight_waiters() {
    let cache = TokenCache::new(secs(60));
    let clock = ManualClock::new();
    let calls = AtomicUsize::new(0);
    let work = account("work");
    let results = join_all((0..8).map(|_| {
        cache.get(&clock, &work, || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(secs(1)).await;
            Ok((format!("token-{n}"), Some(clock.now() + secs(3600))))
        })
    }))
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for result in results {
        assert_eq!(result.expect("token"), "token-0");
    }
    // Another account gets its own refresh.
    let other = cache
        .get(&clock, &account("home"), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(("home".to_owned(), None))
        })
        .await
        .expect("home");
    assert_eq!(other, "home");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn token_cache_refreshes_inside_margin() {
    let cache = TokenCache::new(secs(60));
    let clock = ManualClock::new();
    let calls = AtomicUsize::new(0);
    let work = account("work");
    let refresh = || async {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        Ok((n, Some(clock.now() + secs(100))))
    };
    assert_eq!(cache.get(&clock, &work, refresh).await.expect("first"), 0);
    // 39 s in: 61 s left, outside the 60 s margin.
    clock.advance(secs(39));
    assert_eq!(cache.get(&clock, &work, refresh).await.expect("cached"), 0);
    // 40 s in: exactly 60 s left, inside the margin.
    clock.advance(secs(1));
    assert_eq!(
        cache.get(&clock, &work, refresh).await.expect("refreshed"),
        1
    );
    // A value without an expiry is kept until invalidated.
    let never = TokenCache::new(secs(60));
    let first = never
        .get(&clock, &work, || async { Ok(("kept", None)) })
        .await
        .expect("kept");
    clock.advance(secs(1_000_000));
    let again = never
        .get(&clock, &work, || async { Ok(("new", None)) })
        .await
        .expect("kept");
    assert_eq!((first, again), ("kept", "kept"));
}

#[tokio::test]
async fn invalidate_only_when_stale_matches() {
    let cache = TokenCache::new(secs(60));
    let clock = ManualClock::new();
    let work = account("work");
    let value = |v: &'static str| move || async move { Ok((v, None)) };
    assert_eq!(
        cache.get(&clock, &work, value("old")).await.expect("old"),
        "old"
    );
    // A caller holding a token that was already replaced does not drop the new one.
    assert!(
        !cache
            .invalidate_if(&work, |current| *current == "older")
            .await
    );
    assert_eq!(
        cache.get(&clock, &work, value("new")).await.expect("kept"),
        "old"
    );
    assert!(
        cache
            .invalidate_if(&work, |current| *current == "old")
            .await
    );
    assert_eq!(
        cache.get(&clock, &work, value("new")).await.expect("new"),
        "new"
    );
    // Nothing cached: nothing to drop.
    assert!(!cache.invalidate_if(&account("none"), |_| true).await);
    cache.remove(&work);
    assert_eq!(
        cache
            .get(&clock, &work, value("after"))
            .await
            .expect("after"),
        "after"
    );
}

#[tokio::test]
async fn negative_cache_five_seconds() {
    let cache: TokenCache<String> = TokenCache::new(secs(60));
    let clock = ManualClock::new();
    let calls = AtomicUsize::new(0);
    let work = account("work");
    let failing = || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::quota_status(429, "slow down").with_retry_after(secs(30)))
    };
    let first = cache.get(&clock, &work, failing).await.expect_err("fails");
    assert_eq!(first.kind(), PluginErrorKind::Quota);
    clock.advance(
        NEGATIVE_TTL
            .checked_sub(Duration::from_millis(1))
            .expect("ttl"),
    );
    let replayed = cache.get(&clock, &work, failing).await.expect_err("cached");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no second request inside 5 s"
    );
    assert_eq!(replayed.kind(), PluginErrorKind::Quota);
    assert_eq!(replayed.status(), Some(429));
    assert_eq!(replayed.retry_after(), Some(secs(30)));
    assert_eq!(replayed.message(), "slow down");
    clock.advance(Duration::from_millis(1));
    let ok = cache
        .get(&clock, &work, || async { Ok(("fresh".to_owned(), None)) })
        .await
        .expect("retried at 5 s");
    assert_eq!(ok, "fresh");
    // An Auth failure is remembered too.
    assert!(cache.invalidate_if(&work, |_| true).await);
    let auth = cache
        .get(&clock, &work, || async {
            Err(PluginError::auth("signed out"))
        })
        .await
        .expect_err("auth");
    assert_eq!(auth.kind(), PluginErrorKind::Auth);
    let replayed = cache
        .get(&clock, &work, || async { Ok(("no".to_owned(), None)) })
        .await
        .expect_err("replayed");
    assert_eq!(replayed.kind(), PluginErrorKind::Auth);
    assert_eq!(replayed.message(), "signed out");
}

#[test]
fn failure_replay_keeps_kind_and_status() {
    let cases = [
        PluginError::auth("m"),
        PluginError::quota("m"),
        PluginError::quota_status(429, "m"),
        PluginError::status_error(503, "m"),
        PluginError::connect("m"),
        PluginError::timeout("m"),
        PluginError::transport("m"),
        PluginError::interrupted("m"),
        PluginError::decode("m"),
        PluginError::unsupported("m"),
        PluginError::config("m"),
        PluginError::cancelled("m"),
        PluginError::io("m"),
        PluginError::internal("m"),
    ];
    for error in cases {
        let replayed = super::Failure::new(Duration::ZERO, &error).replay();
        assert_eq!(replayed.kind(), error.kind(), "{error:?}");
        assert_eq!(replayed.status(), error.status(), "{error:?}");
        assert_eq!(replayed.message(), "m");
    }
}
