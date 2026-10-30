//! Runtime selection: a running daemon, or one-shot `hofundd`.
//!
//! Ordinary discovery comes first. When it finds nothing, the CLI probes the
//! daemon lock at every lookup location without creating anything. All locks
//! free means no daemon owns the config, so the command runs through one-shot
//! `hofundd`. Any lock held means a daemon is starting or stuck: the CLI
//! retries discovery with backoff for a bounded wait and then fails, naming
//! the lock and its holder. It never falls back to one-shot while a lock is
//! held, because a one-shot write could race the daemon.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use hofund_control_client::{Client, ClientError, DiscoveryOptions, HeldLock, held_daemon_locks};

/// The explicit control endpoint variable.
const CONTROL_ENV: &str = "HOFUND_CONTROL";

/// How long the CLI waits for discovery while a daemon lock is held.
pub(crate) const HELD_LOCK_WAIT: Duration = Duration::from_secs(5);

/// The first delay between discovery attempts while a lock is held.
const FIRST_RETRY: Duration = Duration::from_millis(50);

/// The longest delay between discovery attempts.
const MAX_RETRY: Duration = Duration::from_millis(800);

/// Where the control plane for a command lives.
pub(crate) enum Target {
    /// A running daemon answered discovery.
    Daemon(Client),
    /// No daemon owns the config: every lock is free. Commands run through
    /// one-shot `hofundd` or report their documented offline behavior.
    NoDaemon,
}

/// Picks a running daemon or one-shot `hofundd` for `config`.
///
/// # Errors
///
/// Returns a message when the config path cannot be resolved, when an
/// explicit endpoint does not answer, or when the daemon lock stays held past
/// [`HELD_LOCK_WAIT`].
pub(crate) async fn select(config: &Path) -> Result<Target, String> {
    let options = DiscoveryOptions::default();
    let runtime = options.runtime.clone();
    let token = local_token(config);
    if std::env::var_os(CONTROL_ENV).is_some() {
        // An explicit endpoint never falls through, not even to one-shot.
        return Client::discover(config, token, options)
            .await
            .map(daemon)
            .map_err(|error| format!("{CONTROL_ENV}: {error}"));
    }
    choose(
        || {
            let options = options.clone();
            let token = token.clone();
            async move { Client::discover(config, token, options).await }
        },
        || held_daemon_locks(config, &runtime),
        HELD_LOCK_WAIT,
    )
    .await
}

/// The selection policy, with discovery and the lock probe injected so
/// tests can drive each branch.
pub(crate) async fn choose<D, F, L>(
    mut discover: D,
    mut held: L,
    wait: Duration,
) -> Result<Target, String>
where
    D: FnMut() -> F,
    F: Future<Output = Result<Client, ClientError>>,
    L: FnMut() -> Result<Vec<HeldLock>, ClientError>,
{
    if let Some(client) = found(discover().await)? {
        return Ok(daemon(client));
    }
    let mut locks = held().map_err(|error| error.to_string())?;
    if locks.is_empty() {
        return Ok(Target::NoDaemon);
    }
    let deadline = tokio::time::Instant::now() + wait;
    let mut delay = FIRST_RETRY;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(held_error(&locks, wait));
        }
        tokio::time::sleep(delay.min(deadline - now)).await;
        delay = (delay * 2).min(MAX_RETRY);
        if let Some(client) = found(discover().await)? {
            return Ok(daemon(client));
        }
        locks = held().map_err(|error| error.to_string())?;
        if locks.is_empty() {
            // The holder exited without publishing discovery; nothing owns
            // the config now, so one-shot is safe.
            return Ok(Target::NoDaemon);
        }
    }
}

/// A discovered client, `None` when discovery found nothing usable, or the
/// error when a daemon answered and refused, which retrying cannot fix.
fn found(result: Result<Client, ClientError>) -> Result<Option<Client>, String> {
    match result {
        Ok(client) => Ok(Some(client)),
        Err(error @ ClientError::Server { .. }) => Err(error.to_string()),
        Err(_) => Ok(None),
    }
}

fn daemon(client: Client) -> Target {
    Target::Daemon(client)
}

/// The error for a lock still held after the wait.
pub(crate) fn held_error(locks: &[HeldLock], wait: Duration) -> String {
    let mut message = String::from("the daemon is starting or unresponsive:");
    for lock in locks {
        let pid = lock
            .pid
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
        message.push_str(&format!(
            "\n  lock {} is held by PID {pid}",
            lock.path.display()
        ));
    }
    message.push_str(&format!(
        "\nno control endpoint answered within {} s. Wait for it to finish \
         starting, or stop that process, then retry.",
        wait.as_secs()
    ));
    message
}

/// A one-shot client for `config`, using the `hofundd` beside this binary or
/// on `PATH`.
///
/// # Errors
///
/// Returns the actionable missing-binary message from [`crate::serve::locate`].
pub(crate) fn one_shot_client(config: &Path) -> Result<Client, String> {
    crate::serve::locate_installed().map(|exe| Client::one_shot(exe, config))
}

/// The bearer token a running daemon would accept, read without creating
/// the token file. A daemon creates the file before it publishes discovery,
/// so a missing file only means no daemon and an empty token is harmless.
fn local_token(config: &Path) -> String {
    hofund_config::load_path(config)
        .ok()
        .and_then(|loaded| {
            hofund_secrets::TokenFile::new(crate::local::beside(config, &loaded.token_file))
                .read()
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{HeldLock, Target, choose, held_error};
    use hofund_control_client::{Client, ClientError};
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::time::Duration;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime")
    }

    fn lock(pid: Option<u32>) -> HeldLock {
        HeldLock {
            path: PathBuf::from("/cfg/.hofund/daemon-0123456789abcdef.lock"),
            pid,
        }
    }

    fn found() -> Client {
        Client::tcp("127.0.0.1:9".parse().expect("addr"), "t")
    }

    fn missing() -> ClientError {
        ClientError::ConnectionUnavailable("nothing".to_owned())
    }

    #[test]
    fn discovered_daemon_wins_without_probing_locks() {
        let probes = Cell::new(0);
        let target = runtime()
            .block_on(choose(
                || async { Ok(found()) },
                || {
                    probes.set(probes.get() + 1);
                    Ok(vec![lock(None)])
                },
                Duration::from_secs(5),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(target, Target::Daemon(_)));
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn free_locks_select_one_shot() {
        let target = runtime()
            .block_on(choose(
                || async { Err(missing()) },
                || Ok(Vec::new()),
                Duration::from_secs(5),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(target, Target::NoDaemon));
    }

    #[test]
    fn cli_waits_for_discovery_when_daemon_lock_held() {
        let attempts = Cell::new(0);
        let rt = runtime();
        let started = rt.block_on(async { tokio::time::Instant::now() });
        let target = rt
            .block_on(choose(
                || {
                    attempts.set(attempts.get() + 1);
                    let ready = attempts.get() >= 4;
                    async move { if ready { Ok(found()) } else { Err(missing()) } }
                },
                || Ok(vec![lock(Some(42))]),
                Duration::from_secs(5),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        let waited = rt.block_on(async { tokio::time::Instant::now() }) - started;
        assert!(matches!(target, Target::Daemon(_)));
        assert_eq!(attempts.get(), 4);
        // Backoff 50 + 100 + 200 ms before the fourth attempt.
        assert_eq!(waited, Duration::from_millis(350));
    }

    #[test]
    fn held_lock_past_the_wait_fails_without_one_shot() {
        let attempts = Cell::new(0);
        let rt = runtime();
        let started = rt.block_on(async { tokio::time::Instant::now() });
        let error = rt
            .block_on(choose(
                || {
                    attempts.set(attempts.get() + 1);
                    async { Err(missing()) }
                },
                || Ok(vec![lock(None)]),
                Duration::from_secs(5),
            ))
            .err()
            .expect("held lock must fail, never select one-shot");
        let waited = rt.block_on(async { tokio::time::Instant::now() }) - started;
        assert_eq!(waited, Duration::from_secs(5));
        // The first attempt, then one after each of 50, 100, 200, 400, five
        // 800 ms steps, and the final 250 ms step to the 5 s deadline.
        assert_eq!(attempts.get(), 11);
        assert!(error.contains("starting or unresponsive"), "{error}");
        assert!(error.contains("PID unknown"), "{error}");
        assert!(error.contains("daemon-0123456789abcdef.lock"), "{error}");
    }

    #[test]
    fn lock_released_during_the_wait_selects_one_shot() {
        let probes = Cell::new(0);
        let target = runtime()
            .block_on(choose(
                || async { Err(missing()) },
                || {
                    probes.set(probes.get() + 1);
                    Ok(if probes.get() < 3 {
                        vec![lock(Some(7))]
                    } else {
                        Vec::new()
                    })
                },
                Duration::from_secs(5),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(target, Target::NoDaemon));
        assert_eq!(probes.get(), 3);
    }

    #[test]
    fn held_error_names_every_lock_and_pid() {
        let message = held_error(&[lock(Some(42)), lock(None)], Duration::from_secs(5));
        assert!(message.starts_with("the daemon is starting or unresponsive"));
        assert!(message.contains("held by PID 42"), "{message}");
        assert!(message.contains("held by PID unknown"), "{message}");
        assert!(message.contains("within 5 s"), "{message}");
    }

    #[test]
    fn probe_failure_is_reported() {
        let error = runtime()
            .block_on(choose(
                || async { Err(missing()) },
                || {
                    Err(ClientError::Discovery(
                        "cannot resolve config path x".to_owned(),
                    ))
                },
                Duration::from_secs(5),
            ))
            .err()
            .expect("probe failure");
        assert_eq!(error, "cannot resolve config path x");
    }
}
