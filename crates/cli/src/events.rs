//! `hofund events tail`: print daemon events, starting the daemon when none
//! runs.
//!
//! This is the one command that starts a background daemon. When discovery
//! finds no daemon and every daemon lock is free, `hofundd --config <path>`
//! is started detached: in its own process group (a new process group with
//! no console on Windows), stdin closed, and stdout and stderr appended to
//! `.hofund/daemon-<config-id>.log` beside the config (discarded when that
//! directory cannot be written). It inherits this process's environment.
//! The daemon outlives the tail; `hofund shutdown` stops it. The tail waits
//! up to [`START_WAIT`] for discovery and control health and otherwise
//! reports a daemon that is still starting, leaving it running.
//!
//! Each event is one JSON line on stdout. When the stream ends, for example
//! after a token rotation, the tail rediscovers the daemon (rereading the
//! token) and resumes with `Last-Event-ID`, for up to [`RECONNECT_WAIT`]. A
//! `shutdown_started` event ends the tail with success; a daemon that goes
//! away without one ends it with an error. A daemon that stopped is never
//! started again.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hofund_control_client::hofund_control_proto::{EventEnvelope, EventPayload, Topic};
use hofund_control_client::{
    Client, ClientError, DiscoveryOptions, held_daemon_locks, resolve_config_id,
};

use crate::connect::{self, Target};

/// How long a started daemon has to answer discovery and health.
pub(crate) const START_WAIT: Duration = Duration::from_secs(10);

/// How long the tail looks for the daemon again after a stream ends.
pub(crate) const RECONNECT_WAIT: Duration = Duration::from_secs(10);

/// The delay between discovery attempts while waiting.
const POLL: Duration = Duration::from_millis(100);

/// `hofund events tail [--requests]`.
///
/// # Errors
///
/// Returns a message when no daemon can be reached or started, or when the
/// daemon goes away without announcing a shutdown.
pub(crate) async fn tail(config: &Path, requests: bool) -> Result<String, String> {
    let topics: &[Topic] = if requests {
        &[Topic::Control, Topic::Requests]
    } else {
        &[Topic::Control]
    };
    let mut client = match connect::select(config).await? {
        Target::Daemon(client) => client,
        Target::NoDaemon => autostart(config).await?,
    };
    let mut last: Option<String> = None;
    let mut out = std::io::stdout();
    loop {
        let mut stream = client
            .events(topics, last.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        while let Some(item) = stream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(error) => {
                    eprintln!("hofund: {error}; reconnecting");
                    break;
                }
            };
            if write_event(&mut out, &event).is_err() {
                // Nobody reads the output any more.
                return Ok(String::new());
            }
            if matches!(event.payload, EventPayload::ShutdownStarted(_)) {
                return Ok(String::new());
            }
        }
        last = stream.last_event_id().map(str::to_owned);
        client = wait_for(|| rediscover(config), || None, || false, RECONNECT_WAIT)
            .await
            .map_err(|_| {
                format!(
                    "the daemon for {} went away and did not come back within {} s",
                    config.display(),
                    RECONNECT_WAIT.as_secs()
                )
            })?;
    }
}

/// Writes `event` as one JSON line.
fn write_event(out: &mut impl Write, event: &EventEnvelope) -> std::io::Result<()> {
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    writeln!(out, "{line}")?;
    out.flush()
}

/// Discovers the daemon with the current token file.
async fn rediscover(config: &Path) -> Result<Client, ClientError> {
    Client::discover(
        config,
        connect::local_token(config),
        DiscoveryOptions::default(),
    )
    .await
}

/// Why waiting for a daemon ended without one.
#[derive(Debug, PartialEq, Eq)]
enum WaitError {
    /// The started process exited, with this status, and no other daemon
    /// holds a lock.
    Exited(String),
    /// Nothing answered within the wait.
    TimedOut,
}

/// Polls `discover` until it finds a daemon or `wait` passes. `exited`
/// reports the started process's exit status once it has exited; that ends
/// the wait unless `held` says another daemon holds a lock and may still
/// answer.
async fn wait_for<D, F, E, H>(
    mut discover: D,
    mut exited: E,
    mut held: H,
    wait: Duration,
) -> Result<Client, WaitError>
where
    D: FnMut() -> F,
    F: Future<Output = Result<Client, ClientError>>,
    E: FnMut() -> Option<String>,
    H: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Ok(client) = discover().await {
            return Ok(client);
        }
        if let Some(status) = exited()
            && !held()
        {
            return Err(WaitError::Exited(status));
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(WaitError::TimedOut);
        }
        tokio::time::sleep(POLL.min(deadline - now)).await;
    }
}

/// Starts `hofundd` for `config` detached and waits for it to answer.
async fn autostart(config: &Path) -> Result<Client, String> {
    let exe = crate::serve::locate_installed()?;
    let mut command = crate::serve::command(&exe, config);
    command.stdin(Stdio::null());
    let output = match open_log(config) {
        Some((path, file)) => match file.try_clone() {
            Ok(copy) => {
                command.stdout(file).stderr(copy);
                format!("its output goes to {}", path.display())
            }
            Err(_) => discard(&mut command),
        },
        None => discard(&mut command),
    };
    detach(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", exe.display()))?;
    let pid = child.id();
    eprintln!(
        "hofund: started hofundd (pid {pid}) for {}; {output}. It keeps running \
         after events tail exits; hofund shutdown stops it.",
        config.display()
    );
    let runtime = DiscoveryOptions::default().runtime;
    wait_for(
        || rediscover(config),
        || exit_status(&mut child),
        || held_daemon_locks(config, &runtime).is_ok_and(|locks| !locks.is_empty()),
        START_WAIT,
    )
    .await
    .map_err(|error| match error {
        WaitError::Exited(status) => {
            format!("hofundd (pid {pid}) exited ({status}) before it answered; {output}")
        }
        WaitError::TimedOut => format!(
            "hofundd (pid {pid}) did not answer within {} s and may still be starting; \
             {output}. Run hofund status to check on it.",
            START_WAIT.as_secs()
        ),
    })
}

fn discard(command: &mut Command) -> String {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    "its output is discarded".to_owned()
}

fn exit_status(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(Some(status)) => Some(status.to_string()),
        Ok(None) => None,
        Err(error) => Some(format!("cannot wait for it: {error}")),
    }
}

/// The daemon log beside the config, `.hofund/daemon-<config-id>.log`,
/// opened for appending; `None` when it cannot be created.
fn open_log(config: &Path) -> Option<(PathBuf, std::fs::File)> {
    let resolved = std::fs::canonicalize(config).ok()?;
    let dir = resolved.parent()?.join(".hofund");
    let path = dir.join(format!("daemon-{}.log", resolve_config_id(config).ok()?));
    let mut builder = std::fs::DirBuilder::new();
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        builder.mode(0o700);
        options.mode(0o600);
    }
    match builder.create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let file = options.open(&path).ok()?;
    Some((path, file))
}

/// Keeps the daemon out of the terminal's process group, so Ctrl-C on the
/// tail does not stop it.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Starts the daemon without a console, in its own process group.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::{WaitError, wait_for, write_event};
    use hofund_control_client::hofund_control_proto::EventEnvelope;
    use hofund_control_client::{Client, ClientError};
    use std::cell::Cell;
    use std::time::Duration;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime")
    }

    fn found() -> Client {
        Client::tcp("127.0.0.1:9".parse().expect("addr"), "t")
    }

    fn missing() -> ClientError {
        ClientError::ConnectionUnavailable("nothing".to_owned())
    }

    /// Discovery that fails `misses` times, then finds a daemon.
    fn after(
        misses: usize,
        calls: &Cell<usize>,
    ) -> impl Future<Output = Result<Client, ClientError>> {
        calls.set(calls.get() + 1);
        let hit = calls.get() > misses;
        async move { if hit { Ok(found()) } else { Err(missing()) } }
    }

    #[test]
    fn the_wait_ends_when_discovery_answers() {
        let calls = Cell::new(0);
        let runtime = runtime();
        let started = runtime.block_on(async { tokio::time::Instant::now() });
        let result = runtime.block_on(wait_for(
            || after(3, &calls),
            || None,
            || false,
            Duration::from_secs(10),
        ));
        assert!(result.is_ok());
        assert_eq!(calls.get(), 4);
        let waited = runtime.block_on(async { tokio::time::Instant::now() }) - started;
        assert_eq!(waited, Duration::from_millis(300));
    }

    #[test]
    fn a_started_daemon_that_exits_ends_the_wait() {
        let calls = Cell::new(0);
        let result = runtime().block_on(wait_for(
            || after(usize::MAX, &calls),
            || Some("exit status: 1".to_owned()),
            || false,
            Duration::from_secs(10),
        ));
        assert_eq!(
            result.err(),
            Some(WaitError::Exited("exit status: 1".to_owned()))
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn an_exit_while_another_daemon_holds_the_lock_keeps_waiting() {
        // Two tails raced: this one's daemon lost the lock, the other's is
        // still starting and answers later.
        let calls = Cell::new(0);
        let result = runtime().block_on(wait_for(
            || after(5, &calls),
            || Some("exit status: 1".to_owned()),
            || true,
            Duration::from_secs(10),
        ));
        assert!(result.is_ok());
        assert_eq!(calls.get(), 6);
    }

    #[test]
    fn nothing_answering_times_out_at_the_deadline() {
        let calls = Cell::new(0);
        let runtime = runtime();
        let started = runtime.block_on(async { tokio::time::Instant::now() });
        let result = runtime.block_on(wait_for(
            || after(usize::MAX, &calls),
            || None,
            || false,
            Duration::from_millis(250),
        ));
        assert_eq!(result.err(), Some(WaitError::TimedOut));
        let waited = runtime.block_on(async { tokio::time::Instant::now() }) - started;
        assert_eq!(waited, Duration::from_millis(250));
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn each_event_is_one_flushed_json_line() {
        let mut out = Vec::new();
        let event = EventEnvelope::default();
        write_event(&mut out, &event).expect("write");
        write_event(&mut out, &event).expect("write");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        assert!(
            lines[0].contains("\"payload\":{\"event\":\"snapshot\""),
            "{}",
            lines[0]
        );
    }
}
