//! Clean quit: agent check, shutdown, and stderr notes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_control_client::hofund_control_proto::{AgentState, ShutdownRequest};
use hofund_control_client::{Client, DiscoveryOptions, held_daemon_locks};

use crate::gateway::{QuitDecision, SPAWN_WAIT, decide_quit};
use crate::runtime::HandoffSnapshot;
use crate::terminal::spawn;

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

/// Runs the section 4 quit table.
pub async fn perform_quit(
    config: &Path,
    handoff: &HandoffSnapshot,
    client: Option<&Client>,
) -> Result<(), String> {
    let any_connected = match client {
        Some(client) => any_connected_agent(client).await?,
        None => false,
    };
    let decision = decide_quit(handoff.spawned, any_connected);
    match decision {
        QuitDecision::LeaveRunning => {
            let status_pid = if let Some(client) = client {
                client.status().await.ok().map(|s| s.pid)
            } else {
                None
            };
            let pid = status_pid.or(handoff.child_pid).unwrap_or(0);
            let log = spawn::daemon_log_path(config)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| ".hofund/daemon-<id>.log".to_owned());
            eprintln!(
                "hofund-tui: leaving hofundd running (pid {pid}). \
                 Log: {log}. \
                 When you are ready, use the service install commands to manage the daemon."
            );
        }
        QuitDecision::ShutdownSpawned => {
            let Some(client) = client else {
                return Ok(());
            };
            client
                .shutdown(ShutdownRequest::default())
                .await
                .map_err(|error| error.to_string())?;
            wait_for_lock_free(config, SPAWN_WAIT).await?;
        }
        QuitDecision::ExitAttached => {}
    }
    Ok(())
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
    use crate::gateway::{QuitDecision, decide_quit};

    #[test]
    fn three_quit_paths() {
        assert_eq!(decide_quit(true, true), QuitDecision::LeaveRunning);
        assert_eq!(decide_quit(true, false), QuitDecision::ShutdownSpawned);
        assert_eq!(decide_quit(false, false), QuitDecision::ExitAttached);
    }
}
