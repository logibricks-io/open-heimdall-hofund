//! Running cursor-agent and reading macOS keychain tokens.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hofund_plugin_api::{BodyStream, HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::timeout;

use crate::auth;

/// The CLI cursor-agent installs.
pub(crate) const COMMAND: &str = "cursor-agent";

/// How long `cursor-agent status` may run.
pub(crate) const STATUS_WAIT: Duration = Duration::from_secs(30);

/// How long `cursor-agent models` may run.
pub(crate) const MODELS_WAIT: Duration = Duration::from_secs(30);

/// How long a keychain read is reused.
const KEYCHAIN_TTL: Duration = Duration::from_secs(10);

/// How long a non-44 keychain denial blocks another spawn.
const KEYCHAIN_DENIED: Duration = Duration::from_secs(10 * 60);

/// How long a keychain read may run.
const KEYCHAIN_READ_WAIT: Duration = Duration::from_secs(10);

/// `security(1)` exit status meaning the item is absent.
const KEYCHAIN_ABSENT: i32 = 44;

/// macOS `security(1)` used for keychain reads.
const SECURITY: &str = "/usr/bin/security";

/// Absolute `security` path on every OS, including Windows where a
/// leading `/` is not absolute. Tests with `force_keychain` spawn this.
fn security_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\usr\bin\security")
    } else {
        PathBuf::from(SECURITY)
    }
}

/// Cached macOS keychain token reads for one subscription instance.
#[derive(Default)]
pub(crate) struct KeychainCache {
    token: String,
    read_at: Option<Duration>,
    denied_until: Option<Duration>,
    flight: Option<Arc<Flight>>,
}

#[derive(Clone)]
enum KeychainOutcome {
    Token(String),
    Empty,
}

struct Flight {
    inner: Mutex<FlightInner>,
}

enum FlightInner {
    Pending(Vec<Waker>),
    Done(KeychainOutcome),
}

struct FlightWait {
    flight: Arc<Flight>,
}

impl Flight {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FlightInner::Pending(Vec::new())),
        })
    }

    fn finish(&self, outcome: KeychainOutcome) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *inner, FlightInner::Done(outcome));
        if let FlightInner::Pending(wakers) = previous {
            for waker in wakers {
                waker.wake();
            }
        }
    }
}

impl Future for FlightWait {
    type Output = KeychainOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self
            .flight
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &mut *inner {
            FlightInner::Done(outcome) => Poll::Ready(outcome.clone()),
            FlightInner::Pending(wakers) => {
                if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                    wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

fn cache_lock(cache: &Mutex<KeychainCache>) -> std::sync::MutexGuard<'_, KeychainCache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

impl KeychainCache {
    fn cached_token(&self, now: Duration) -> Option<String> {
        let at = self.read_at?;
        (now.saturating_sub(at) < KEYCHAIN_TTL).then(|| self.token.clone())
    }

    fn is_denied(&self, now: Duration) -> bool {
        self.denied_until.is_some_and(|until| now < until)
    }

    fn store_success(&mut self, now: Duration, token: String) {
        self.read_at = Some(now);
        self.token = token;
        self.denied_until = None;
        self.flight = None;
    }

    fn store_absent(&mut self) {
        self.denied_until = None;
        self.flight = None;
    }

    fn store_denial(&mut self, now: Duration) {
        self.read_at = None;
        self.token.clear();
        self.denied_until = Some(now + KEYCHAIN_DENIED);
        self.flight = None;
    }

    fn store_timeout(&mut self, now: Duration) {
        self.read_at = Some(now);
        self.token.clear();
        self.flight = None;
    }
}

/// Reads cursor-agent's token from the keychain when allowed.
///
/// Spawn, timeout, denial, or a missing item yield an empty string so callers
/// can fall back to `auth.json`. This call must not fail the surrounding
/// operation.
pub(crate) async fn keychain_token(
    host: &HostContext,
    cache: &Mutex<KeychainCache>,
    fresh: bool,
    enabled: bool,
) -> String {
    if !enabled {
        return String::new();
    }
    let now = host.clock().monotonic();
    enum Start {
        Cached(String),
        Empty,
        Wait(Arc<Flight>),
        Lead(Arc<Flight>),
    }
    let start = {
        let mut guard = cache_lock(cache);
        if !fresh && let Some(token) = guard.cached_token(now) {
            Start::Cached(token)
        } else if !fresh && guard.is_denied(now) {
            Start::Empty
        } else if let Some(flight) = &guard.flight {
            Start::Wait(Arc::clone(flight))
        } else {
            let flight = Flight::new();
            guard.flight = Some(Arc::clone(&flight));
            Start::Lead(flight)
        }
    };
    let flight = match start {
        Start::Cached(token) => return token,
        Start::Empty => return String::new(),
        Start::Wait(flight) => {
            return match (FlightWait { flight }).await {
                KeychainOutcome::Token(token) => token,
                KeychainOutcome::Empty => String::new(),
            };
        }
        Start::Lead(flight) => flight,
    };
    let outcome = read_keychain(host).await;
    {
        let now = host.clock().monotonic();
        let mut guard = cache_lock(cache);
        match &outcome {
            ReadResult::Success(token) => guard.store_success(now, token.clone()),
            ReadResult::Absent => guard.store_absent(),
            ReadResult::Denied => guard.store_denial(now),
            ReadResult::Timeout => guard.store_timeout(now),
            ReadResult::Failed => {
                guard.flight = None;
            }
        }
    }
    let shared = match &outcome {
        ReadResult::Success(token) => KeychainOutcome::Token(token.clone()),
        ReadResult::Absent | ReadResult::Denied | ReadResult::Timeout | ReadResult::Failed => {
            KeychainOutcome::Empty
        }
    };
    flight.finish(shared.clone());
    match shared {
        KeychainOutcome::Token(token) => token,
        KeychainOutcome::Empty => String::new(),
    }
}

enum ReadResult {
    Success(String),
    Absent,
    Denied,
    Timeout,
    Failed,
}

async fn read_keychain(host: &HostContext) -> ReadResult {
    let Ok(request) = SpawnRequest::new(
        security_program(),
        [
            "find-generic-password",
            "-s",
            "cursor-access-token",
            "-a",
            "cursor-user",
            "-w",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        vec![],
        std::env::temp_dir(),
    ) else {
        return ReadResult::Failed;
    };
    let Ok(child) = host.spawn_process().spawn_piped(&request).await else {
        return ReadResult::Failed;
    };
    let Ok(mut stdout) = child.stdout() else {
        return ReadResult::Failed;
    };
    match timeout(KEYCHAIN_READ_WAIT, async {
        let token = collect(&mut stdout, 4096)
            .await
            .unwrap_or_default()
            .trim()
            .to_owned();
        let code = child.exit_code().await.ok();
        (token, code)
    })
    .await
    {
        None => {
            drop(child);
            ReadResult::Timeout
        }
        Some((_, Some(KEYCHAIN_ABSENT))) => ReadResult::Absent,
        Some((_, Some(code))) if code != 0 => ReadResult::Denied,
        Some((token, _)) => ReadResult::Success(token),
    }
}

/// Live access token: keychain when enabled, else auth.json.
pub(crate) async fn live_access(
    host: &HostContext,
    agent_home: &Path,
    fresh: bool,
    cache: &Mutex<KeychainCache>,
    keychain_enabled: bool,
) -> Result<Option<String>, PluginError> {
    let keychain = keychain_token(host, cache, fresh, keychain_enabled).await;
    if !keychain.is_empty() {
        return Ok(Some(keychain));
    }
    Ok(auth::read_agent_access(agent_home))
}

/// Spawn request for `cursor-agent` with `args` in `dir`.
pub(crate) fn request(
    program: &Path,
    args: &[&str],
    dir: &Path,
) -> Result<SpawnRequest, PluginError> {
    let env = vec![(
        OsString::from("CURSOR_CONFIG_DIR"),
        dir.as_os_str().to_owned(),
    )];
    SpawnRequest::new(program, args.iter().map(OsString::from).collect(), env, dir)
}

/// Collects stdout until the child exits or the cap is hit.
pub(crate) async fn collect(stdout: &mut BodyStream, cap: usize) -> Option<String> {
    let mut out = Vec::new();
    while let Some(result) = stdout.next_chunk().await {
        let Ok(chunk) = result else {
            break;
        };
        if out.len() + chunk.len() > cap {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
}

/// Resolves the cursor-agent program: config override or PATH.
pub(crate) fn program(configured: Option<&PathBuf>) -> Result<PathBuf, PluginError> {
    if let Some(path) = configured {
        return Ok(path.clone());
    }
    hofund_plugin_sdk::process::find_program(hofund_plugin_api::BinaryName::new(COMMAND))
        .ok_or_else(|| {
            PluginError::config(
                "cursor-agent is not installed: it is not on PATH; install it or set `command`",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::security_program;

    #[test]
    fn security_program_is_absolute_and_named_security() {
        let path = security_program();
        assert!(
            path.is_absolute(),
            "keychain program must be absolute on this OS, got {}",
            path.display()
        );
        assert!(
            path.file_name().is_some_and(|name| name == "security"),
            "keychain program must be named security, got {}",
            path.display()
        );
    }
}
