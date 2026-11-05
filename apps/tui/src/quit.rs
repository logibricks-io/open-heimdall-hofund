//! Clean quit: agent check, shutdown, and stderr notes.

#![allow(async_fn_in_trait)] // QuitShutdown is internal to the TUI crate.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_config::load_path;
use hofund_control_client::hofund_control_proto::{AgentState, ShutdownRequest};
use hofund_control_client::{Client, DiscoveryOptions, held_daemon_locks};

use crate::gateway::{QuitDecision, SPAWN_WAIT, decide_quit};
use crate::runtime::HandoffSnapshot;
use crate::terminal::deps::LiveHandoff;
use crate::terminal::spawn;

/// What the terminal runner should do after the UI tears down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuitAction {
    /// Nothing else to print.
    None,
    /// Print this line on stderr after raw mode is off.
    LeaveRunningNotice(String),
}

/// Discovery and shutdown for the quit table.
pub trait QuitShutdown: Send + Sync {
    /// Reads the bearer token from the config token file.
    fn read_token(&self, config: &Path) -> String;

    /// Attaches to the daemon when no client is cached yet.
    async fn discover(&self, config: &Path, token: &str) -> Result<Client, String>;

    /// Posts shutdown to the control API.
    async fn shutdown(&self, client: &Client) -> Result<(), String>;

    /// Waits until the daemon lock is released or the budget expires.
    async fn wait_after_shutdown(&self, config: &Path) -> Result<(), String>;
}

impl QuitShutdown for LiveHandoff {
    fn read_token(&self, config: &Path) -> String {
        read_token_file(config)
    }

    async fn discover(&self, config: &Path, token: &str) -> Result<Client, String> {
        self.rediscover(config, token).await
    }

    async fn shutdown(&self, client: &Client) -> Result<(), String> {
        client
            .shutdown(ShutdownRequest::default())
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn wait_after_shutdown(&self, config: &Path) -> Result<(), String> {
        wait_for_lock_free(config, SPAWN_WAIT).await
    }
}

/// Whether any agent reports `Connected`.
pub async fn any_connected_agent(client: &Client) -> Result<bool, String> {
    let response = client
        .agent_status()
        .await
        .map_err(|error| error.to_string())?;
    Ok(response
        .agents
        .iter()
        .any(|entry| entry.state == AgentState::Connected))
}

/// Picks the pid shown when leaving a spawned daemon running (status wins over snapshot).
#[must_use]
pub(crate) fn resolve_leave_running_pid(status_pid: Option<u32>, child_pid: Option<u32>) -> u32 {
    status_pid.or(child_pid).unwrap_or(0)
}

/// Builds the leave-running stderr line (pure, testable).
#[must_use]
pub fn leave_running_notice(pid: u32, log_path: &str) -> String {
    format!(
        "hofund-tui: leaving hofundd running (pid {pid}). \
         Log: {log_path}. \
         When you are ready, use the service install commands to manage the daemon."
    )
}

/// Runs the section 4 quit table.
pub async fn perform_quit(
    config: &Path,
    handoff: &HandoffSnapshot,
    client: Option<Client>,
    deps: &impl QuitShutdown,
) -> Result<QuitAction, String> {
    let any_connected = match client.as_ref() {
        Some(client) => any_connected_agent(client).await?,
        None => false,
    };
    let decision = decide_quit(handoff.spawned, any_connected);
    match decision {
        QuitDecision::LeaveRunning => {
            let status_pid = if let Some(client) = client.as_ref() {
                client.status().await.ok().map(|s| s.pid)
            } else {
                None
            };
            let pid = resolve_leave_running_pid(status_pid, handoff.child_pid);
            let log = spawn::daemon_log_path(config)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| ".hofund/daemon-<id>.log".to_owned());
            Ok(QuitAction::LeaveRunningNotice(leave_running_notice(
                pid, &log,
            )))
        }
        QuitDecision::ShutdownSpawned => {
            let token = deps.read_token(config);
            let client = match client {
                Some(client) => client,
                None => deps.discover(config, &token).await.map_err(|error| {
                    format!("hofund-tui: spawned daemon but cannot attach for shutdown: {error}")
                })?,
            };
            deps.shutdown(&client).await?;
            deps.wait_after_shutdown(config).await?;
            Ok(QuitAction::None)
        }
        QuitDecision::ExitAttached => Ok(QuitAction::None),
    }
}

async fn wait_for_lock_free(config: &Path, wait: Duration) -> Result<(), String> {
    let runtime = DiscoveryOptions::default().runtime;
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        match held_daemon_locks(config, &runtime) {
            Ok(locks) if locks.is_empty() => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

fn read_token_file(config: &Path) -> String {
    load_path(config)
        .ok()
        .and_then(|loaded| {
            let base = config.parent().unwrap_or_else(|| Path::new("."));
            hofund_secrets::TokenFile::new(base.join(&loaded.token_file))
                .read()
                .ok()
        })
        .unwrap_or_default()
}

/// Log path beside the config (informational).
#[must_use]
pub fn daemon_log_display(config: &Path) -> PathBuf {
    spawn::daemon_log_path(config).unwrap_or_else(|| {
        config
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".hofund/daemon.log")
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{QuitShutdown, leave_running_notice, perform_quit, resolve_leave_running_pid};
    use crate::gateway::{HandoffDeps, HandoffState, run_handoff_tick};
    use crate::runtime::HandoffSnapshot;
    use hofund_control_client::{Client, ClientError};

    struct FakeQuitDeps {
        token: String,
        shutdown_calls: AtomicU32,
        discover_calls: AtomicU32,
        wait_calls: AtomicU32,
        discover_ok: Mutex<bool>,
    }

    impl FakeQuitDeps {
        fn new(token: impl Into<String>) -> Self {
            Self {
                token: token.into(),
                shutdown_calls: AtomicU32::new(0),
                discover_calls: AtomicU32::new(0),
                wait_calls: AtomicU32::new(0),
                discover_ok: Mutex::new(true),
            }
        }
    }

    impl QuitShutdown for FakeQuitDeps {
        fn read_token(&self, _config: &Path) -> String {
            self.token.clone()
        }

        async fn discover(&self, _config: &Path, _token: &str) -> Result<Client, String> {
            self.discover_calls.fetch_add(1, Ordering::SeqCst);
            if *self.discover_ok.lock().expect("discover") {
                Ok(Client::tcp("127.0.0.1:9".parse().expect("addr"), "t"))
            } else {
                Err("discover failed".to_owned())
            }
        }

        async fn shutdown(&self, _client: &Client) -> Result<(), String> {
            self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait_after_shutdown(&self, _config: &Path) -> Result<(), String> {
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct HandoffFake {
        locks: Mutex<Vec<PathBuf>>,
        discover_succeeds: Mutex<bool>,
        status_fails: Mutex<bool>,
        spawn_count: Mutex<u32>,
        alive: Mutex<bool>,
        status_pid: u32,
    }

    impl HandoffFake {
        fn new() -> Self {
            Self {
                locks: Mutex::new(Vec::new()),
                discover_succeeds: Mutex::new(false),
                status_fails: Mutex::new(false),
                spawn_count: Mutex::new(0),
                alive: Mutex::new(true),
                status_pid: 999,
            }
        }
    }

    impl HandoffDeps for HandoffFake {
        fn held_locks(&self, _config: &Path) -> Result<Vec<PathBuf>, String> {
            Ok(self.locks.lock().expect("locks").clone())
        }

        async fn discover_attached(
            &self,
            _config: &Path,
            _token: &str,
        ) -> Result<Client, ClientError> {
            if *self.discover_succeeds.lock().expect("discover") {
                Ok(Client::tcp("127.0.0.1:9".parse().expect("addr"), "t"))
            } else {
                Err(ClientError::ConnectionUnavailable("no daemon".to_owned()))
            }
        }

        async fn probe_model_port(&self, _listen: SocketAddr) -> Result<(), ClientError> {
            Err(ClientError::ConnectionUnavailable("quiet".to_owned()))
        }

        fn spawn_daemon(&self, _config: &Path) -> Result<u32, String> {
            *self.spawn_count.lock().expect("spawn") += 1;
            Ok(999)
        }

        async fn fetch_status(
            &self,
            _client: &Client,
        ) -> Result<hofund_control_client::hofund_control_proto::StatusResponse, ClientError>
        {
            if *self.status_fails.lock().expect("status") {
                return Err(ClientError::ConnectionUnavailable(
                    "status failed".to_owned(),
                ));
            }
            let mut status = hofund_control_client::hofund_control_proto::StatusResponse::default();
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
    fn leave_running_pid_prefers_status_over_child_snapshot() {
        assert_eq!(resolve_leave_running_pid(Some(4242), Some(999)), 4242);
        assert_eq!(resolve_leave_running_pid(None, Some(999)), 999);
    }

    #[test]
    fn leave_running_notice_contains_pid_log_and_service_line() {
        let text = leave_running_notice(4242, "/tmp/.hofund/daemon-test.log");
        assert!(text.contains("4242"));
        assert!(text.contains("/tmp/.hofund/daemon-test.log"));
        assert!(text.contains("service install commands"));
    }

    #[tokio::test]
    async fn quit_while_discovery_waiting_still_shuts_down() {
        tokio::time::pause();
        let deps = Arc::new(HandoffFake::new());
        let progress = Arc::new(Mutex::new(HandoffSnapshot::default()));
        let progress_bg = Arc::clone(&progress);
        let deps_bg = Arc::clone(&deps);
        let tick = tokio::spawn(async move {
            let mut state = HandoffState::default();
            let mut token = || "t".to_owned();
            let mut notify = |state: &HandoffState| {
                *progress_bg.lock().expect("progress") = HandoffSnapshot {
                    spawned: state.spawned,
                    child_pid: state.child_pid,
                };
            };
            let _ = run_handoff_tick(
                deps_bg.as_ref(),
                Path::new("/tmp/x.toml"),
                &mut token,
                listen(),
                &mut state,
                &mut notify,
            )
            .await;
        });
        tokio::time::advance(std::time::Duration::from_millis(150)).await;
        let snapshot = progress.lock().expect("progress").clone();
        assert!(snapshot.spawned);
        assert_eq!(snapshot.child_pid, Some(999));
        let quit_deps = FakeQuitDeps::new("t");
        perform_quit(Path::new("/tmp/x.toml"), &snapshot, None, &quit_deps)
            .await
            .expect("quit");
        assert_eq!(quit_deps.shutdown_calls.load(Ordering::SeqCst), 1);
        assert_eq!(quit_deps.wait_calls.load(Ordering::SeqCst), 1);
        tick.abort();
    }

    #[tokio::test]
    async fn shutdown_spawned_waits_after_control_shutdown() {
        let quit_deps = FakeQuitDeps::new("t");
        let snapshot = HandoffSnapshot {
            spawned: true,
            child_pid: Some(999),
        };
        perform_quit(Path::new("/tmp/x.toml"), &snapshot, None, &quit_deps)
            .await
            .expect("quit");
        assert_eq!(quit_deps.shutdown_calls.load(Ordering::SeqCst), 1);
        assert_eq!(quit_deps.wait_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn quit_after_spawn_without_discovery_still_shuts_down() {
        tokio::time::pause();
        let join = tokio::spawn(async {
            let deps = HandoffFake::new();
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
            (
                outcome,
                HandoffSnapshot {
                    spawned: state.spawned,
                    child_pid: state.child_pid,
                },
            )
        });
        tokio::time::advance(crate::gateway::SPAWN_WAIT + std::time::Duration::from_millis(200))
            .await;
        let (outcome, snapshot) = join.await.expect("join");
        assert!(matches!(
            outcome,
            crate::gateway::HandoffOutcome::Update(crate::state::GatewayPresence::Error(_))
        ));
        assert!(snapshot.spawned);
        let quit_deps = FakeQuitDeps::new("t");
        perform_quit(Path::new("/tmp/x.toml"), &snapshot, None, &quit_deps)
            .await
            .expect("quit");
        assert_eq!(quit_deps.shutdown_calls.load(Ordering::SeqCst), 1);
        assert_eq!(quit_deps.discover_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn status_failure_on_second_tick_keeps_spawned_and_quit_shuts_down() {
        let deps = HandoffFake::new();
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
        let snapshot = HandoffSnapshot {
            spawned: state.spawned,
            child_pid: state.child_pid,
        };
        let quit_deps = FakeQuitDeps::new("t");
        perform_quit(Path::new("/tmp/x.toml"), &snapshot, None, &quit_deps)
            .await
            .expect("quit");
        assert_eq!(quit_deps.shutdown_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn discover_failure_on_shutdown_is_an_error_not_success() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let quit_deps = FakeQuitDeps::new("t");
        *quit_deps.discover_ok.lock().expect("discover") = false;
        let snapshot = HandoffSnapshot {
            spawned: true,
            child_pid: Some(999),
        };
        let result = rt.block_on(perform_quit(
            Path::new("/tmp/x.toml"),
            &snapshot,
            None,
            &quit_deps,
        ));
        assert!(result.is_err());
        assert_eq!(quit_deps.shutdown_calls.load(Ordering::SeqCst), 0);
    }
}
