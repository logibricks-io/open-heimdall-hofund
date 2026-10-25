//! Gateway and agent actions for the menu-bar app.
//!
//! The UI asks this type to start the daemon, stop it, and connect or
//! disconnect an agent. Those calls go to `hofund-daemon` and `hofund-agent`.
//! The agent list is the catalog, in catalog order, so the menu does not
//! branch on a product name.

use std::path::PathBuf;

use hofund_agent::{GatewayTarget, Session, adapters, find};
use hofund_daemon::{
    Running, configured_listen, ensure_config_file, local_gateway, route_models, usage_report,
};
use serde::Serialize;

use crate::open::open_config_file;

/// One row in the agent list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AgentRow {
    /// Catalog id.
    pub id: String,
    /// Name shown in the menu and the window.
    pub display_name: String,
    /// The binary is on `PATH`, or a managed file already exists.
    pub installed: bool,
    /// The managed files match the local gateway.
    pub connected: bool,
}

/// What the menu and the window render.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Snapshot {
    /// The gateway process is accepting connections.
    pub running: bool,
    /// Bound address while running, otherwise the address in the config file.
    pub listen: String,
    /// The latest action error, or empty.
    pub message: String,
    /// Every catalog adapter, in catalog order.
    pub agents: Vec<AgentRow>,
}

/// One menu row derived from a [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuEntry {
    /// Stable id. Agent rows use the prefix `agent:`.
    pub id: String,
    /// Text shown for the row. Empty for a separator.
    pub label: String,
    /// Whether the row can be activated.
    pub enabled: bool,
    /// `Some` when the row is a check item.
    pub checked: Option<bool>,
}

/// Menu rows for `snapshot`, including one check item per catalog agent.
pub(crate) fn menu_entries(snapshot: &Snapshot) -> Vec<MenuEntry> {
    let state = if snapshot.running {
        "running"
    } else {
        "stopped"
    };
    let mut entries = vec![
        MenuEntry {
            id: "status".to_owned(),
            label: format!("Gateway: {state}"),
            enabled: false,
            checked: None,
        },
        MenuEntry {
            id: "listen".to_owned(),
            label: format!("Listen: {}", snapshot.listen),
            enabled: false,
            checked: None,
        },
    ];
    if !snapshot.message.is_empty() {
        entries.push(MenuEntry {
            id: "notice".to_owned(),
            label: snapshot.message.clone(),
            enabled: false,
            checked: None,
        });
    }
    entries.push(MenuEntry {
        id: "gateway-start".to_owned(),
        label: "Start".to_owned(),
        enabled: !snapshot.running,
        checked: None,
    });
    entries.push(MenuEntry {
        id: "gateway-stop".to_owned(),
        label: "Stop".to_owned(),
        enabled: snapshot.running,
        checked: None,
    });
    entries.push(separator("separator-agents"));
    for agent in &snapshot.agents {
        entries.push(MenuEntry {
            id: format!("agent:{}", agent.id),
            label: agent.display_name.clone(),
            enabled: true,
            checked: Some(agent.connected),
        });
    }
    entries.push(separator("separator-actions"));
    entries.push(MenuEntry {
        id: "usage".to_owned(),
        label: "Usage".to_owned(),
        enabled: true,
        checked: None,
    });
    entries.push(MenuEntry {
        id: "open-config".to_owned(),
        label: "Open config".to_owned(),
        enabled: true,
        checked: None,
    });
    entries.push(MenuEntry {
        id: "quit".to_owned(),
        label: "Quit".to_owned(),
        enabled: true,
        checked: None,
    });
    entries
}

fn separator(id: &str) -> MenuEntry {
    MenuEntry {
        id: id.to_owned(),
        label: String::new(),
        enabled: false,
        checked: None,
    }
}

/// The gateway process and the config path the menu is editing.
pub(crate) struct Control {
    config_path: PathBuf,
    server: Option<Running>,
    bound: Option<String>,
    message: String,
}

impl Control {
    /// Actions against `config_path`. The file is created on start and on open.
    pub(crate) fn open(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            server: None,
            bound: None,
            message: String::new(),
        }
    }

    /// Binds the gateway. A second start while one is running does nothing.
    pub(crate) async fn start(&mut self) -> Result<(), String> {
        if self.server.is_some() {
            self.message.clear();
            return Ok(());
        }
        if let Err(error) = ensure_config_file(&self.config_path) {
            return self.fail(error.to_string());
        }
        match hofund_daemon::start(&self.config_path).await {
            Ok(running) => {
                self.bound = Some(running.local_addr.to_string());
                self.server = Some(running);
                self.message.clear();
                Ok(())
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Stops the gateway. Stopping when it is already stopped does nothing.
    pub(crate) async fn stop(&mut self) -> Result<(), String> {
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
        self.bound = None;
        self.message.clear();
        Ok(())
    }

    /// Connects or disconnects the catalog agent `id`.
    pub(crate) fn set_agent(
        &mut self,
        session: &Session,
        id: &str,
        connect: bool,
    ) -> Result<(), String> {
        let result = (|| -> Result<Vec<String>, String> {
            let Some(adapter) = find(id) else {
                return Err(format!("unknown agent `{id}`"));
            };
            if connect {
                let local = local_gateway(&self.config_path).map_err(|error| error.to_string())?;
                let models = route_models(&self.config_path).map_err(|error| error.to_string())?;
                let target = GatewayTarget::new(local.base_url, local.token, models);
                adapter
                    .apply(session, &target)
                    .map_err(|error| error.to_string())?;
            } else {
                let outcome = adapter
                    .disconnect(session, false)
                    .map_err(|error| error.to_string())?;
                return Ok(outcome.warnings().to_vec());
            }
            Ok(Vec::new())
        })();
        let warnings = result.as_ref().ok().cloned().unwrap_or_default();
        self.store(result.map(|_| ()))?;
        if !warnings.is_empty() {
            self.message = warnings.join("\n");
        }
        Ok(())
    }

    /// The usage table from the daemon.
    pub(crate) fn usage_summary(&mut self) -> Result<String, String> {
        let result = usage_report(&self.config_path).map_err(|error| error.to_string());
        self.store(result)
    }

    /// Creates the config file when it is missing, then opens it.
    pub(crate) fn open_config(&mut self) -> Result<(), String> {
        let result = open_config_file(&self.config_path);
        self.store(result)
    }

    /// Current status. Agent rows follow [`adapters`] order.
    pub(crate) fn snapshot(&mut self, session: &Session) -> Snapshot {
        let target = local_gateway(&self.config_path).ok().map(|local| {
            GatewayTarget::new(
                local.base_url,
                local.token,
                route_models(&self.config_path).unwrap_or_default(),
            )
        });
        let mut agents = Vec::new();
        for adapter in adapters() {
            let connected = match &target {
                Some(target) => match adapter.status(session, target) {
                    Ok(status) => status.connected(),
                    Err(error) => {
                        if self.message.is_empty() {
                            self.message = error.to_string();
                        }
                        false
                    }
                },
                None => false,
            };
            agents.push(AgentRow {
                id: adapter.id().to_owned(),
                display_name: adapter.display_name().to_owned(),
                installed: adapter.detect(session),
                connected,
            });
        }
        Snapshot {
            running: self.server.is_some(),
            listen: self.listen_label(),
            message: self.message.clone(),
            agents,
        }
    }

    fn listen_label(&self) -> String {
        if let Some(bound) = &self.bound {
            return bound.clone();
        }
        configured_listen(&self.config_path).unwrap_or_else(|| "unavailable".to_owned())
    }

    fn fail(&mut self, message: String) -> Result<(), String> {
        self.message.clone_from(&message);
        Err(message)
    }

    fn store<T>(&mut self, result: Result<T, String>) -> Result<T, String> {
        match result {
            Ok(value) => {
                self.message.clear();
                Ok(value)
            }
            Err(message) => {
                self.message.clone_from(&message);
                Err(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use hofund_agent::{Session, adapters};

    use super::{Control, menu_entries};

    fn scratch(name: &str) -> PathBuf {
        static TICK: AtomicU64 = AtomicU64::new(0);
        let tick = TICK.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "hofund-desktop-{}-{nanos}-{tick}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[tokio::test]
    async fn start_and_stop_report_the_bound_address() {
        let dir = scratch("bind");
        let path = dir.join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
        let mut control = Control::open(&path);
        let session = Session::new(&dir);
        control.start().await.expect("start");
        let snapshot = control.snapshot(&session);
        assert!(snapshot.running);
        assert!(snapshot.listen.starts_with("127.0.0.1:"));
        assert_ne!(snapshot.listen, "127.0.0.1:0");
        let table = control.usage_summary().expect("usage");
        assert!(table.contains("MODEL"));
        control.stop().await.expect("stop");
        let snapshot = control.snapshot(&session);
        assert!(!snapshot.running);
        assert_eq!(snapshot.listen, "127.0.0.1:0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_menu_lists_every_catalog_agent() {
        let dir = scratch("menu");
        let path = dir.join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:9\"\n").expect("config");
        let mut control = Control::open(&path);
        let snapshot = control.snapshot(&Session::new(&dir));
        let entries = menu_entries(&snapshot);
        assert!(
            entries
                .iter()
                .any(|entry| entry.id == "gateway-start" && entry.enabled)
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.id == "gateway-stop" && !entry.enabled)
        );
        assert!(entries.iter().any(|entry| entry.id == "usage"));
        assert!(entries.iter().any(|entry| entry.id == "open-config"));
        assert!(entries.iter().any(|entry| entry.id == "quit"));
        let toggles: Vec<_> = entries
            .iter()
            .filter(|entry| entry.id.starts_with("agent:"))
            .collect();
        let catalog = adapters();
        assert_eq!(toggles.len(), catalog.len());
        for (entry, adapter) in toggles.iter().zip(catalog) {
            assert_eq!(entry.id, format!("agent:{}", adapter.id()));
            assert_eq!(entry.label, adapter.display_name());
            assert_eq!(entry.checked, Some(false));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn connecting_an_agent_round_trips() {
        let dir = scratch("agent");
        let home = dir.join("home");
        fs::create_dir_all(&home).expect("home");
        let path = dir.join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:3425\"\n").expect("config");
        let mut control = Control::open(&path);
        let session = Session::new(&home);
        let id = adapters()[0].id();
        control.set_agent(&session, id, true).expect("connect");
        let snapshot = control.snapshot(&session);
        let row = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .expect("row");
        assert!(row.connected);
        control.set_agent(&session, id, false).expect("disconnect");
        let snapshot = control.snapshot(&session);
        let row = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .expect("row");
        assert!(!row.connected);
        let error = control
            .set_agent(&session, "not-an-agent", true)
            .expect_err("unknown");
        assert!(error.contains("unknown agent"));
        let _ = fs::remove_dir_all(&dir);
    }
}
