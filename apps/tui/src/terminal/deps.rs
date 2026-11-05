//! Live [`HandoffDeps`] using control-client and the CLI spawn pattern.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hofund_control_client::hofund_control_proto::StatusResponse;
use hofund_control_client::{
    Client, ClientError, DiscoveryOptions, held_daemon_locks, probe_model_health,
};

use super::spawn;
use crate::gateway::{HandoffDeps, probe_timeout};

/// Production handoff dependencies.
pub struct LiveHandoff {
    _config: PathBuf,
    client: Mutex<Option<Client>>,
}

impl LiveHandoff {
    /// Builds deps for `config`.
    #[must_use]
    pub fn new(config: &Path) -> Self {
        Self {
            _config: config.to_path_buf(),
            client: Mutex::new(None),
        }
    }

    /// Returns the attached client when handoff succeeded.
    #[must_use]
    pub fn client(&self) -> Option<Client> {
        self.client.lock().expect("client").clone()
    }

    fn store_client(&self, client: Client) {
        *self.client.lock().expect("client") = Some(client);
    }
}

impl HandoffDeps for LiveHandoff {
    fn held_locks(&self, config: &Path) -> Result<Vec<PathBuf>, String> {
        let runtime = DiscoveryOptions::default().runtime;
        held_daemon_locks(config, &runtime)
            .map(|locks| locks.into_iter().map(|lock| lock.path).collect())
            .map_err(|error| error.to_string())
    }

    async fn discover_attached(&self, config: &Path, token: &str) -> Result<Client, ClientError> {
        let client =
            Client::discover(config, token.to_owned(), DiscoveryOptions::default()).await?;
        client.health().await?;
        self.store_client(client.clone());
        Ok(client)
    }

    async fn probe_model_port(&self, listen: SocketAddr) -> Result<(), ClientError> {
        probe_model_health(listen, probe_timeout()).await
    }

    fn spawn_daemon(&self, config: &Path) -> Result<u32, String> {
        spawn::spawn_detached(config)
    }

    async fn fetch_status(&self, client: &Client) -> Result<StatusResponse, ClientError> {
        client.status().await
    }

    fn process_alive(&self, pid: u32) -> bool {
        spawn::process_alive(pid)
    }
}
