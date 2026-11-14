//! Antigravity hub version from the auto-updater manifest.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::HostContext;
use hofund_plugin_sdk::Call;

pub(crate) const FALLBACK_VERSION: &str = "2.9.1";
pub(crate) const MANIFEST_USER_AGENT: &str = "electron-builder";

const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const BODY_CAP: usize = 64 << 10;

struct Cache {
    version: String,
    fetched_at: Option<SystemTime>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TLS_CACHE: std::cell::RefCell<Option<Cache>> = const { std::cell::RefCell::new(None) };
}

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            version: String::new(),
            fetched_at: None,
        })
    })
}

fn with_cache_mut<R>(write: impl FnOnce(&mut Cache) -> R) -> R {
    #[cfg(test)]
    if let Some(local) = TLS_CACHE.with(|slot| slot.borrow_mut().take()) {
        let mut local = local;
        let out = write(&mut local);
        TLS_CACHE.with(|slot| *slot.borrow_mut() = Some(local));
        return out;
    }
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write(&mut guard)
}

/// Resolves the Antigravity hub version for outbound requests.
///
/// The manifest is read at most once every six hours. Fetch and parse failures
/// keep the last good reading, or [`FALLBACK_VERSION`] when none is stored yet.
pub(crate) async fn resolve(host: &HostContext, manifest_url: &str) -> String {
    let now = host.clock().now();
    if let Some(version) = with_cache_mut(|cache| {
        (!cache.version.is_empty()
            && cache
                .fetched_at
                .is_some_and(|at| now.duration_since(at).unwrap_or(Duration::ZERO) < CACHE_TTL))
        .then(|| cache.version.clone())
    }) {
        return version;
    }

    let keep = with_cache_mut(|cache| {
        cache.fetched_at = Some(now);
        if cache.version.is_empty() {
            cache.version = FALLBACK_VERSION.to_owned();
        }
        cache.version.clone()
    });

    if let Some(parsed) = fetch_manifest(host, manifest_url).await {
        with_cache_mut(|cache| {
            cache.version.clone_from(&parsed);
        });
        return parsed;
    }
    keep
}

async fn fetch_manifest(host: &HostContext, url: &str) -> Option<String> {
    let reply = Call::get(url)
        .header("user-agent", MANIFEST_USER_AGENT)
        .read_timeout(FETCH_TIMEOUT)
        .fetch(host.http())
        .await
        .ok()?;
    if reply.status() != 200 {
        return None;
    }
    parse_version(reply.body())
}

fn parse_version(body: &[u8]) -> Option<String> {
    let capped = &body[..body.len().min(BODY_CAP)];
    #[allow(clippy::never_loop)]
    for line in capped.split(|byte| *byte == b'\n') {
        let line = std::str::from_utf8(line).ok()?;
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("version:") else {
            continue;
        };
        let mut value = rest.trim();
        value = value.trim_matches(|ch| ch == '"' || ch == '\'').trim();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

#[must_use]
pub(crate) fn user_agent_for(version: &str) -> String {
    format!(
        "antigravity/hub/{version} {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[cfg(test)]
pub(crate) fn reset_cache_for_test() {
    TLS_CACHE.with(|slot| *slot.borrow_mut() = None);
    with_cache_mut(|cache| {
        cache.version.clear();
        cache.fetched_at = None;
    });
}

#[cfg(test)]
pub(crate) fn warm_cache_for_test(version: &str, fetched_at: SystemTime) {
    TLS_CACHE.with(|slot| {
        *slot.borrow_mut() = Some(Cache {
            version: version.to_owned(),
            fetched_at: Some(fetched_at),
        });
    });
}

#[cfg(test)]
pub(crate) fn use_global_cache_for_test() {
    TLS_CACHE.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
mod unit {
    use super::parse_version;

    #[test]
    fn parses_the_first_version_line() {
        let body = b"version: 3.1.4\npath: x.zip\n";
        assert_eq!(parse_version(body).as_deref(), Some("3.1.4"));
    }

    #[test]
    fn trims_quotes_and_rejects_garbage() {
        let body = b"version: \"2.10.0\"\n";
        assert_eq!(parse_version(body).as_deref(), Some("2.10.0"));
        assert!(parse_version(b"version: 2.9.1-beta\n").is_none());
    }
}
