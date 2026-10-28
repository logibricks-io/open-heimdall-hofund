use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_control_proto::{HealthResponse, ListenSource, ShutdownState, StatusResponse};
use hofund_gateway::control::ControlState;
use sha2::{Digest, Sha256};

pub(crate) struct DaemonControl {
    config_path: PathBuf,
    configured: SocketAddr,
    running: SocketAddr,
    started_at_ms: u64,
    runtime_revision: String,
}

impl DaemonControl {
    pub(crate) fn new(
        config_path: &Path,
        configured: SocketAddr,
        running: SocketAddr,
        started_at_ms: u64,
        runtime_revision: String,
    ) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            configured,
            running,
            started_at_ms,
            runtime_revision,
        }
    }
}

impl ControlState for DaemonControl {
    fn health(&self) -> HealthResponse {
        let mut health = HealthResponse::default();
        health.daemon_version = env!("CARGO_PKG_VERSION").to_owned();
        health
    }

    fn status(&self) -> StatusResponse {
        let mut status = StatusResponse::default();
        status.pid = std::process::id();
        status.started_at_ms = self.started_at_ms;
        status.generation = 1;
        status.disk_revision = std::fs::read(&self.config_path)
            .map(|bytes| revision(&bytes))
            .unwrap_or_default();
        status.runtime_revision = self.runtime_revision.clone();
        status.in_flight = 0;
        status.shutdown = ShutdownState::Running;
        status.model_listener.configured = self.configured.to_string();
        status.model_listener.effective = self.configured.to_string();
        status.model_listener.running = Some(self.running.to_string());
        status.model_listener.effective_source = ListenSource::Config;
        status
    }
}

pub(crate) fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) fn revision(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
