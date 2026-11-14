//! Refreshing sign-ins: cursor-agent status and stored OAuth checks.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::timeout;

use crate::cli::{self, KeychainCache, STATUS_WAIT};
use crate::connect;
use crate::endpoints::Endpoints;
use crate::tokens::{self, REFRESH};

/// How long a renew waits for the per-account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// One in-flight `cursor-agent status` per subscription, like the reference `cliRefresh`.
#[derive(Default)]
pub(crate) struct StatusCoalescer {
    flight: Mutex<Option<Arc<StatusFlight>>>,
}

enum StatusOutcome {
    Ok,
    Err(Arc<PluginError>),
}

struct StatusFlight {
    inner: Mutex<StatusFlightInner>,
}

enum StatusFlightInner {
    Pending(Vec<Waker>),
    Done(StatusOutcome),
}

struct StatusFlightWait {
    flight: Arc<StatusFlight>,
}

impl StatusFlight {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(StatusFlightInner::Pending(Vec::new())),
        })
    }

    fn finish(&self, outcome: StatusOutcome) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *inner, StatusFlightInner::Done(outcome));
        if let StatusFlightInner::Pending(wakers) = previous {
            for waker in wakers {
                waker.wake();
            }
        }
    }
}

impl Future for StatusFlightWait {
    type Output = StatusOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self
            .flight
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &mut *inner {
            StatusFlightInner::Done(outcome) => Poll::Ready(match outcome {
                StatusOutcome::Ok => StatusOutcome::Ok,
                StatusOutcome::Err(error) => StatusOutcome::Err(Arc::clone(error)),
            }),
            StatusFlightInner::Pending(wakers) => {
                if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                    wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

impl StatusCoalescer {
    /// Runs `cursor-agent status` once while concurrent callers share the same run.
    pub(crate) async fn agent_status(
        &self,
        host: &HostContext,
        program: &Path,
        dir: &Path,
    ) -> Result<(), PluginError> {
        self.coalesce(|| run_agent_status(host, program, dir)).await
    }

    async fn coalesce<F, Fut>(&self, run: F) -> Result<(), PluginError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), PluginError>>,
    {
        enum Start {
            Wait(Arc<StatusFlight>),
            Lead(Arc<StatusFlight>),
        }
        let start = {
            let mut guard = self.flight.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(flight) = guard.as_ref() {
                Start::Wait(Arc::clone(flight))
            } else {
                let flight = StatusFlight::new();
                *guard = Some(Arc::clone(&flight));
                Start::Lead(flight)
            }
        };
        match start {
            Start::Wait(flight) => match (StatusFlightWait { flight }).await {
                StatusOutcome::Ok => Ok(()),
                StatusOutcome::Err(error) => Err(plugin_error_from_shared(&error)),
            },
            Start::Lead(flight) => {
                let outcome = run().await;
                let shared = match &outcome {
                    Ok(()) => StatusOutcome::Ok,
                    Err(error) => StatusOutcome::Err(shared_status_error(error)),
                };
                flight.finish(shared);
                {
                    let mut guard = self.flight.lock().unwrap_or_else(PoisonError::into_inner);
                    *guard = None;
                }
                outcome
            }
        }
    }
}

/// Runs `cursor-agent status` so the CLI refreshes an almost-expired token.
async fn run_agent_status(
    host: &HostContext,
    program: &Path,
    dir: &Path,
) -> Result<(), PluginError> {
    let request = cli::request(program, &["status"], dir)?;
    let child = host.spawn_process().spawn(&request).await?;
    match timeout(STATUS_WAIT, host.spawn_process().wait(child)).await {
        Some(Ok(0)) => Ok(()),
        Some(Ok(_)) => Err(PluginError::auth(
            "cursor-agent status did not finish successfully",
        )),
        Some(Err(error)) => Err(error),
        None => Err(PluginError::timeout(
            "cursor-agent status did not finish before the time limit",
        )),
    }
}

/// Renews a stored OAuth access token when it is near expiry.
pub(crate) async fn renew_agent(
    host: &HostContext,
    account: &AccountId,
    program: Option<&Path>,
    agent_home: &Path,
    keychain: &Mutex<KeychainCache>,
    keychain_enabled: bool,
    status: &StatusCoalescer,
) -> Result<SecretString, PluginError> {
    let _lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let now = host.clock().now();
    let mut token = cli::live_access(host, agent_home, false, keychain, keychain_enabled)
        .await?
        .unwrap_or_default();
    if token.is_empty() || tokens::near_expiry(now, tokens::jwt_expiry_ms(&token)) {
        let program = program.ok_or_else(|| {
            PluginError::config(
                "cursor-agent is not installed: it is not on PATH; install it or set `command`",
            )
        })?;
        status.agent_status(host, program, agent_home).await?;
        token = cli::live_access(host, agent_home, true, keychain, keychain_enabled)
            .await?
            .unwrap_or_default();
    }
    if token.is_empty() {
        return Err(PluginError::auth(
            "cursor-agent is not signed in; run cursor-agent login",
        ));
    }
    if let Some(exp) = tokens::jwt_expiry_ms(&token) {
        let expires = tokens::ms_from_now(
            std::time::UNIX_EPOCH,
            i64::try_from(exp).unwrap_or(i64::MAX),
        );
        if now >= expires {
            return Err(PluginError::auth(
                "cursor-agent's sign-in has run out; sign in again",
            ));
        }
    }
    Ok(SecretString::from(token))
}

/// Renews a stored OAuth access token when it is near expiry.
pub(crate) async fn oauth_check(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = tokens::parse_blob(lease.secret())?;
    if tokens::is_agent(&blob) || tokens::is_api_key(&blob) {
        return Ok(());
    }
    let now = host.clock().now();
    let expires = tokens::effective_expires_ms(&blob);
    if !tokens::near_expiry(now, expires) {
        return Ok(());
    }
    let refresh = blob
        .secret(REFRESH)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no refresh token"))?;
    if secret_text(&refresh) == "none" {
        return Err(PluginError::auth(
            "the stored Cursor sign-in has no refresh token",
        ));
    }
    let url = endpoints.oauth_refresh();
    let body = format!(
        r#"{{"refreshToken":"{}"}}"#,
        escape_json(secret_text(&refresh))
    );
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .bytes(body.into_bytes())
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::auth(format!(
            "Cursor refused to renew the sign-in (HTTP {})",
            reply.status()
        )));
    }
    let value: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Cursor sent unreadable refresh JSON"))?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("Cursor sent back no access token"))?;
    let access_secret = SecretString::from(access.to_owned());
    let next_refresh = value
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(SecretString::from)
        .unwrap_or(refresh);
    let label = blob.text(tokens::LABEL).unwrap_or("Cursor").to_owned();
    let plan = blob.text(tokens::PLAN);
    let expires_ms = tokens::jwt_expiry_ms(access)
        .unwrap_or_else(|| tokens::unix_ms(host.clock().now()) as u64 + 30 * 24 * 3600 * 1000);
    let updated = tokens::blob_oauth(&access_secret, &next_refresh, expires_ms, &label, plan);
    lease.commit(Some(label), updated.to_secret()).await?;
    let _ = connect::who_is(host, endpoints, &access_secret).await;
    Ok(())
}

fn escape_json(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shared_status_error(error: &PluginError) -> Arc<PluginError> {
    Arc::new(PluginError::auth(error.to_string()))
}

fn plugin_error_from_shared(error: &Arc<PluginError>) -> PluginError {
    PluginError::auth(error.to_string())
}

#[cfg(test)]
mod status_coalesce_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hofund_kit_subscription::{block_on_paused, join_all, yield_times};

    use super::StatusCoalescer;
    use hofund_plugin_api::PluginError;

    #[test]
    fn concurrent_callers_share_one_run() {
        let coalescer = StatusCoalescer::default();
        let runs = Arc::new(AtomicUsize::new(0));
        block_on_paused(async {
            let left = {
                let coalescer = &coalescer;
                let runs = Arc::clone(&runs);
                Box::pin(async move {
                    coalescer
                        .coalesce(|| async {
                            runs.fetch_add(1, Ordering::SeqCst);
                            yield_times(2).await;
                            Ok(())
                        })
                        .await
                        .expect("first");
                })
            };
            let right = {
                let coalescer = &coalescer;
                let runs = Arc::clone(&runs);
                Box::pin(async move {
                    coalescer
                        .coalesce(|| async {
                            runs.fetch_add(1, Ordering::SeqCst);
                            yield_times(1).await;
                            Ok(())
                        })
                        .await
                        .expect("second");
                })
            };
            join_all(vec![left, right]).await;
        });
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "concurrent status coalescing must run the leader only once"
        );
    }

    #[test]
    fn leader_failure_is_shared() {
        let coalescer = StatusCoalescer::default();
        let runs = Arc::new(AtomicUsize::new(0));
        block_on_paused(async {
            let left = {
                let coalescer = &coalescer;
                let runs = Arc::clone(&runs);
                Box::pin(async move {
                    let err = coalescer
                        .coalesce(|| async {
                            runs.fetch_add(1, Ordering::SeqCst);
                            yield_times(1).await;
                            Err(PluginError::auth("status failed"))
                        })
                        .await
                        .expect_err("failed");
                    assert!(err.to_string().contains("status failed"));
                })
            };
            let right = {
                let coalescer = &coalescer;
                Box::pin(async move {
                    let err = coalescer
                        .coalesce(|| async {
                            yield_times(0).await;
                            Ok(())
                        })
                        .await
                        .expect_err("waiter sees failure");
                    assert!(err.to_string().contains("status failed"));
                })
            };
            join_all(vec![left, right]).await;
        });
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
