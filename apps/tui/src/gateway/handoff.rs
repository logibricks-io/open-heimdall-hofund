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

/// Updates attach state without clearing `spawned` for our own child pid.
pub fn record_attach<D: HandoffDeps>(
    state: &mut HandoffState,
    daemon_pid: u32,
    client: Client,
    deps: &D,
) {
    if state.spawned
        && let Some(child) = state.child_pid
    {
        if daemon_pid == 0 {
            if deps.process_alive(child) {
                state.client = Some(client);
                return;
            }
        } else if daemon_pid == child {
            state.client = Some(client);
            return;
        } else if deps.process_alive(daemon_pid) {
            state.spawned = false;
            state.child_pid = None;
        }
    }

    let own = state.spawned && state.child_pid == Some(daemon_pid) && daemon_pid != 0;
    if !own {
        state.spawned = false;
        state.child_pid = None;
    }
    state.client = Some(client);
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
pub async fn run_handoff_tick<
    D: HandoffDeps,
    R: FnMut() -> String,
    N: for<'a> FnMut(&'a HandoffState),
>(
    deps: &D,
    config: &Path,
    read_token: &mut R,
    listen: SocketAddr,
    state: &mut HandoffState,
    notify: &mut N,
) -> HandoffOutcome {
    let locks = match deps.held_locks(config) {
        Ok(locks) => locks,
        Err(message) => {
            return HandoffOutcome::Update(GatewayPresence::Error(message));
        }
    };
    if !locks.is_empty() {
        let token = read_token();
        match deps.discover_attached(config, &token).await {
            Ok(client) => {
                let status = deps.fetch_status(&client).await.ok();
                let daemon_pid = status.as_ref().map(|s| s.pid).unwrap_or(0);
                record_attach(state, daemon_pid, client.clone(), deps);
                let line = status_line_from_status(status.as_ref(), &client, deps).await;
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
            notify(state);
            match wait_for_daemon(deps, config, read_token).await {
                Ok(client) => {
                    let status = deps.fetch_status(&client).await.ok();
                    let daemon_pid = status.as_ref().map(|s| s.pid).unwrap_or(pid);
                    record_attach(state, daemon_pid, client, deps);
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

async fn wait_for_daemon<D: HandoffDeps, R: FnMut() -> String>(
    deps: &D,
    config: &Path,
    read_token: &mut R,
) -> Result<Client, String> {
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT;
    while tokio::time::Instant::now() < deadline {
        let token = read_token();
        if let Ok(client) = deps.discover_attached(config, &token).await {
            return Ok(client);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "daemon did not answer within {} s",
        SPAWN_WAIT.as_secs()
    ))
}

async fn status_line_from_status<D: HandoffDeps>(
    status: Option<&StatusResponse>,
    client: &Client,
    deps: &D,
) -> String {
    match status {
        Some(status) => {
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
        None => match deps.fetch_status(client).await {
            Ok(status) => {
                let listener = status
                    .model_listener
                    .running
                    .as_deref()
                    .unwrap_or("not running");
                format!("daemon pid {} · listener {listener}", status.pid)
            }
            Err(error) => error.to_string(),
        },
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
    use super::{
        HandoffDeps, HandoffState, QuitDecision, decide_quit, record_attach, run_handoff_tick,
    };
    use hofund_control_client::hofund_control_proto::StatusResponse;
    use hofund_control_client::{Client, ClientError};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use crate::state::GatewayPresence;

    struct FakeDeps {
        locks: Mutex<Vec<PathBuf>>,
        model_in_use: Mutex<bool>,
        spawn_count: Mutex<u32>,
        alive: Mutex<bool>,
        lock_probe_fails: Mutex<bool>,
        discover_calls: AtomicU32,
        discover_succeeds: Mutex<bool>,
        status_fails: Mutex<bool>,
        status_pid: u32,
    }

    impl FakeDeps {
        fn new() -> Self {
            Self {
                locks: Mutex::new(Vec::new()),
                model_in_use: Mutex::new(false),
                spawn_count: Mutex::new(0),
                alive: Mutex::new(true),
                lock_probe_fails: Mutex::new(false),
                discover_calls: AtomicU32::new(0),
                discover_succeeds: Mutex::new(false),
                status_fails: Mutex::new(false),
                status_pid: 999,
            }
        }
    }

    impl HandoffDeps for FakeDeps {
        fn held_locks(&self, _config: &Path) -> Result<Vec<PathBuf>, String> {
            if *self.lock_probe_fails.lock().expect("lock") {
                return Err("lock probe failed".to_owned());
            }
            Ok(self.locks.lock().expect("locks").clone())
        }

        async fn discover_attached(
            &self,
            _config: &Path,
            _token: &str,
        ) -> Result<Client, ClientError> {
            self.discover_calls.fetch_add(1, Ordering::SeqCst);
            if *self.discover_succeeds.lock().expect("discover") {
                Ok(Client::tcp("127.0.0.1:9".parse().expect("addr"), "t"))
            } else {
                Err(ClientError::ConnectionUnavailable("no daemon".to_owned()))
            }
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
            if *self.status_fails.lock().expect("status") {
                return Err(ClientError::ConnectionUnavailable(
                    "status failed".to_owned(),
                ));
            }
            let mut status = StatusResponse::default();
            status.pid = self.status_pid;
            Ok(status)
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
    async fn lock_held_never_spawns() {
        let deps = FakeDeps::new();
        *deps.locks.lock().expect("locks") = vec![PathBuf::from("/tmp/x.lock")];
        let mut state = HandoffState::default();
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let outcome = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(matches!(
            outcome,
            super::HandoffOutcome::Update(GatewayPresence::Error(_))
        ));
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }

    #[tokio::test]
    async fn lock_probe_failure_does_not_spawn() {
        let deps = FakeDeps::new();
        *deps.lock_probe_fails.lock().expect("f") = true;
        let mut state = HandoffState::default();
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }

    #[tokio::test]
    async fn spawned_stays_after_second_tick_when_pid_matches() {
        let mut deps = FakeDeps::new();
        deps.status_pid = 999;
        *deps.discover_succeeds.lock().expect("d") = true;
        let mut state = HandoffState::default();
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(state.spawned);
        *deps.locks.lock().expect("locks") = vec![PathBuf::from("/tmp/x.lock")];
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(state.spawned);
        assert_eq!(state.child_pid, Some(999));
    }

    #[tokio::test]
    async fn wait_for_daemon_rereads_token() {
        tokio::time::pause();
        let deps = Arc::new(FakeDeps::new());
        let deps_bg = Arc::clone(&deps);
        let reads = Arc::new(AtomicU32::new(0));
        let reads_bg = Arc::clone(&reads);
        let mut token = move || {
            reads_bg.fetch_add(1, Ordering::SeqCst);
            "t".to_owned()
        };
        let worker = tokio::spawn(async move {
            super::wait_for_daemon(deps_bg.as_ref(), Path::new("/tmp/x.toml"), &mut token).await
        });
        tokio::time::advance(Duration::from_millis(50)).await;
        *deps.discover_succeeds.lock().expect("d") = true;
        tokio::time::advance(Duration::from_millis(200)).await;
        let _ = worker.await.expect("join");
        assert!(reads.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn status_failure_keeps_spawned_when_child_still_alive() {
        let mut deps = FakeDeps::new();
        deps.status_pid = 999;
        *deps.discover_succeeds.lock().expect("d") = true;
        let mut state = HandoffState::default();
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(state.spawned);
        *deps.locks.lock().expect("locks") = vec![PathBuf::from("/tmp/x.lock")];
        *deps.status_fails.lock().expect("status") = true;
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(state.spawned);
        assert_eq!(state.child_pid, Some(999));
    }

    #[tokio::test]
    async fn dead_recorded_spawn_pid_allows_another_spawn() {
        let deps = FakeDeps::new();
        *deps.alive.lock().expect("alive") = false;
        let mut state = HandoffState {
            spawned: true,
            child_pid: Some(999),
            client: None,
        };
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let _ = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 1);
    }

    #[test]
    fn record_attach_clears_spawned_when_status_pid_is_not_our_child() {
        let deps = FakeDeps::new();
        *deps.alive.lock().expect("alive") = false;
        let mut state = HandoffState {
            spawned: true,
            child_pid: Some(999),
            client: None,
        };
        record_attach(
            &mut state,
            100,
            Client::tcp("127.0.0.1:9".parse().expect("addr"), "t"),
            &deps,
        );
        assert!(!state.spawned);
        assert_eq!(state.child_pid, None);
    }

    #[test]
    fn record_attach_other_alive_daemon_clears_spawned_claim() {
        let deps = FakeDeps::new();
        let mut state = HandoffState {
            spawned: true,
            child_pid: Some(999),
            client: None,
        };
        record_attach(
            &mut state,
            888,
            Client::tcp("127.0.0.1:9".parse().expect("addr"), "t"),
            &deps,
        );
        assert!(!state.spawned);
        assert_eq!(state.child_pid, None);
    }

    #[tokio::test]
    async fn handoff_tick_idle_while_spawned_child_still_alive() {
        let deps = FakeDeps::new();
        let mut state = HandoffState {
            spawned: true,
            child_pid: Some(999),
            client: None,
        };
        let mut token = || "t".to_owned();
        let mut notify = |_: &HandoffState| {};
        let outcome = run_handoff_tick(
            &deps,
            Path::new("/tmp/x.toml"),
            &mut token,
            listen(),
            &mut state,
            &mut notify,
        )
        .await;
        assert!(matches!(outcome, super::HandoffOutcome::Idle));
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }

    #[tokio::test]
    async fn stopped_handoff_before_interval_does_not_respawn() {
        tokio::time::pause();
        let deps = Arc::new(FakeDeps::new());
        *deps.alive.lock().expect("alive") = false;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let deps_bg = Arc::clone(&deps);
        let worker = tokio::spawn(async move {
            let mut watch_interval = tokio::time::interval(super::WATCH_INTERVAL);
            watch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut stop = stop_rx;
            let mut state = HandoffState {
                spawned: true,
                child_pid: Some(999),
                client: None,
            };
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    changed = stop.changed() => {
                        if changed.is_ok() && *stop.borrow() {
                            break;
                        }
                    }
                    _ = watch_interval.tick() => {
                        if *stop.borrow() {
                            break;
                        }
                        let mut token = || "t".to_owned();
                        let mut notify = |_: &HandoffState| {};
                        let _ = run_handoff_tick(
                            deps_bg.as_ref(),
                            Path::new("/tmp/x.toml"),
                            &mut token,
                            listen(),
                            &mut state,
                            &mut notify,
                        )
                        .await;
                    }
                }
            }
        });
        let _ = stop_tx.send(true);
        worker.await.expect("handoff worker should exit");
        tokio::time::advance(super::WATCH_INTERVAL + std::time::Duration::from_secs(1)).await;
        assert_eq!(*deps.spawn_count.lock().expect("spawn"), 0);
    }
}
