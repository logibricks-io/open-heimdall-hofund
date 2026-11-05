//! Daemon discovery, spawn, and clean-quit decisions.

#![allow(async_fn_in_trait)] // HandoffDeps is internal; Send bounds added when the trait stabilizes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_control_client::hofund_control_proto::StatusResponse;
use hofund_control_client::{Client, ClientError};

use crate::gateway::StatusServeFields;
use crate::state::GatewayPresence;

/// Probe budget for model port checks (section 4).
const PROBE_TIMEOUT: Duration = Duration::from_millis(700);

/// How long to wait for a spawned daemon to answer.
pub const SPAWN_WAIT: Duration = Duration::from_secs(10);

/// Background rediscovery interval.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(15);

/// Injectable dependencies for handoff tests.
pub trait HandoffDeps: Send + Sync {
    /// Returns held daemon lock paths without creating files.
    fn held_locks(&self, config: &Path) -> Result<Vec<PathBuf>, String>;

    /// Attempts discovery + health + status when a lock is held.
    async fn discover_attached(&self, config: &Path, token: &str) -> Result<Client, ClientError>;

    /// Probes the model listener from config.
    async fn probe_model_port(&self, listen: SocketAddr) -> Result<(), ClientError>;

    /// Spawns `hofundd` detached; returns child pid.
    fn spawn_daemon(&self, config: &Path) -> Result<u32, String>;

    /// Reads daemon status for display.
    async fn fetch_status(&self, client: &Client) -> Result<StatusResponse, ClientError>;

    /// Whether `pid` is still running.
    fn process_alive(&self, pid: u32) -> bool;
}

/// Recorded handoff state for this UI process.
#[derive(Debug, Default)]
pub struct HandoffState {
    /// Whether this UI started the daemon.
    pub spawned: bool,
    /// Child pid when `spawned`.
    pub child_pid: Option<u32>,
    /// Active control client, when attached.
    pub client: Option<Client>,
}

/// Result of one handoff tick.
#[derive(Clone, Debug)]
pub enum HandoffOutcome {
    /// Attached or still starting; update presence line.
    Update(GatewayPresence),
    /// No change.
    Idle,
}

/// Runs steps 1–4 of section 4 once.
pub async fn run_handoff_tick<D: HandoffDeps>(
    deps: &D,
    config: &Path,
    token: &str,
    listen: SocketAddr,
    state: &mut HandoffState,
) -> HandoffOutcome {
    if !deps.held_locks(config).unwrap_or_default().is_empty() {
        match deps.discover_attached(config, token).await {
            Ok(client) => {
                let line = status_line(deps, &client).await;
                state.spawned = false;
                state.child_pid = None;
                state.client = Some(client);
                return HandoffOutcome::Update(GatewayPresence::Attached { line });
            }
            Err(error) => {
                return HandoffOutcome::Update(GatewayPresence::Error(error.to_string()));
            }
        }
    }

    if deps.probe_model_port(listen).await.is_ok() {
        return HandoffOutcome::Update(GatewayPresence::Error(
            "model port is in use and this config's daemon lock is free".to_owned(),
        ));
    }

    let may_spawn = !state.spawned || state.child_pid.is_some_and(|pid| !deps.process_alive(pid));
    if !may_spawn {
        return HandoffOutcome::Idle;
    }

    match deps.spawn_daemon(config) {
        Ok(pid) => {
            state.spawned = true;
            state.child_pid = Some(pid);
            match wait_for_daemon(deps, config, token).await {
                Ok(client) => {
                    state.client = Some(client);
                    HandoffOutcome::Update(GatewayPresence::Spawned {
                        line: format!("served while the UI is open (pid {pid})"),
                        pid,
                    })
                }
                Err(message) => HandoffOutcome::Update(GatewayPresence::Error(message)),
            }
        }
        Err(message) => HandoffOutcome::Update(GatewayPresence::Error(message)),
    }
}

async fn wait_for_daemon<D: HandoffDeps>(
    deps: &D,
    config: &Path,
    token: &str,
) -> Result<Client, String> {
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(client) = deps.discover_attached(config, token).await {
            return Ok(client);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "daemon did not answer within {} s",
        SPAWN_WAIT.as_secs()
    ))
}

async fn status_line<D: HandoffDeps>(deps: &D, client: &Client) -> String {
    match deps.fetch_status(client).await {
        Ok(status) => {
            let listener = status
                .model_listener
                .running
                .as_deref()
                .unwrap_or("not running");
            let serves = status
                .serves()
                .map(|s| format!(" serves={s}"))
                .unwrap_or_default();
            format!("daemon pid {} · listener {listener}{serves}", status.pid)
        }
        Err(error) => error.to_string(),
    }
}

/// Clean quit action (section 4 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitDecision {
    /// Leave daemon running; print note on stderr.
    LeaveRunning,
    /// POST shutdown because this UI spawned it and no agent is connected.
    ShutdownSpawned,
    /// Exit without shutdown (attached daemon).
    ExitAttached,
}

/// Chooses quit behavior.
#[must_use]
pub fn decide_quit(spawned: bool, any_connected: bool) -> QuitDecision {
    if any_connected {
        QuitDecision::LeaveRunning
    } else if spawned {
        QuitDecision::ShutdownSpawned
    } else {
        QuitDecision::ExitAttached
    }
}

/// Timeout used by live probes.
#[must_use]
pub const fn probe_timeout() -> Duration {
    PROBE_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::{HandoffDeps, HandoffState, QuitDecision, decide_quit, run_handoff_tick};
    use hofund_control_client::hofund_control_proto::StatusResponse;
    use hofund_control_client::{Client, ClientError};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::state::GatewayPresence;

    struct FakeDeps {
        locks: Mutex<Vec<PathBuf>>,
        model_in_use: Mutex<bool>,
        spawn_count: Mutex<u32>,
        alive: Mutex<bool>,
    }

    impl FakeDeps {
        fn new() -> Self {
            Self {
                locks: Mutex::new(Vec::new()),
                model_in_use: Mutex::new(false),
                spawn_count: Mutex::new(0),
                alive: Mutex::new(true),
            }
        }
    }

    impl HandoffDeps for FakeDeps {
        fn held_locks(&self, _config: &Path) -> Result<Vec<PathBuf>, String> {
            Ok(self.locks.lock().expect("locks").clone())
        }

        async fn discover_attached(
            &self,
            _config: &Path,
            _token: &str,
        ) -> Result<Client, ClientError> {
            Err(ClientError::ConnectionUnavailable("no daemon".to_owned()))
        }

        async fn probe_model_port(&self, _listen: SocketAddr) -> Result<(), ClientError> {
            if *self.model_in_use.lock().expect("model") {
                Ok(())
            } else {
                Err(ClientError::ConnectionUnavailable("quiet".to_owned()))
            }
        }

        fn spawn_daemon(&self, _config: &Path) -> Result<u32, String> {
            *self.spawn_count.lock().expect("spawn") += 1;
            Ok(999)
        }

        async fn fetch_status(&self, _client: &Client) -> Result<StatusResponse, ClientError> {
            Ok(StatusResponse::default())
        }

        fn process_alive(&self, _pid: u32) -> bool {
            *self.alive.lock().expect("alive")
        }
    }

    fn listen() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9)
    }

    #[test]
    fn quit_table_matches_section_four() {
        assert_eq!(decide_quit(true, true), QuitDecision::LeaveRunning);
        assert_eq!(decide_quit(true, false), QuitDecision::ShutdownSpawned);
        assert_eq!(decide_quit(false, false), QuitDecision::ExitAttached);
    }

    #[tokio::test]
    async fn model_port_taken_does_not_spawn() {
        let deps = FakeDeps::new();
        *deps.model_in_use.lock().expect("model") = true;
        let mut state = HandoffState::default();
        let outcome =
            run_handoff_tick(&deps, Path::new("/tmp/x.toml"), "t", listen(), &mut state).await;
        assert!(matches!(
            outcome,
            super::HandoffOutcome::Update(GatewayPresence::Error(_))
        ));
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }

    #[tokio::test]
    async fn lock_free_quiet_port_spawns_once() {
        let deps = FakeDeps::new();
        let mut state = HandoffState::default();
        let _ = run_handoff_tick(&deps, Path::new("/tmp/x.toml"), "t", listen(), &mut state).await;
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 1);
    }
}
