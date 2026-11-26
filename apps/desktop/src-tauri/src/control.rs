//! Gateway and agent actions for the menu-bar app.
//!
//! The UI asks this type to start the daemon, stop it, and connect or
//! disconnect an agent. Those calls go to `hofund-daemon` and `hofund-agent`.
//! The agent list is the catalog, in catalog order, so the menu does not
//! branch on a product name.

use std::path::PathBuf;

use hofund_agent::{GatewayTarget, Session, catalog};
use hofund_daemon::{
    QuotaAlertEvent, Running, StartOptions, agent_descriptions, configured_listen,
    configured_subscription_risks, control_event_bus, ensure_config_file, local_gateway,
    route_models, start_with_options, subscription_risk_lines, usage_report,
};
use serde::Serialize;

use crate::open::open_config_file;

/// Builds stopped-gateway risk lines off the control mutex.
///
/// Plugin builds happen here. Account hooks do not.
pub(crate) async fn load_stopped_risks(path: PathBuf) -> Vec<String> {
    tokio::task::spawn_blocking(move || configured_subscription_risks(&path).unwrap_or_default())
        .await
        .unwrap_or_default()
}

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
    /// Newest routing decision while running, otherwise empty.
    pub route: String,
    /// Request and token totals since local midnight while running, otherwise empty.
    pub today: String,
    /// Retained quota card lines while running, otherwise empty.
    pub quotas: Vec<String>,
    /// Risk lines for subscriptions whose new enrollment is withdrawn.
    pub risks: Vec<String>,
    /// Way-out warnings. Empty when the last reading was fine or unknown.
    pub warnings: Vec<String>,
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
    let mut entries = Vec::new();
    for (index, warning) in snapshot.warnings.iter().enumerate() {
        entries.push(MenuEntry {
            id: format!("way-out:{index}"),
            label: warning.clone(),
            enabled: false,
            checked: None,
        });
    }
    if snapshot.running && !snapshot.warnings.is_empty() {
        entries.push(MenuEntry {
            id: "way-out-check".to_owned(),
            label: "Check again".to_owned(),
            enabled: true,
            checked: None,
        });
    }
    entries.push(MenuEntry {
        id: "status".to_owned(),
        label: format!("Gateway: {state}"),
        enabled: false,
        checked: None,
    });
    entries.push(MenuEntry {
        id: "listen".to_owned(),
        label: format!("Listen: {}", snapshot.listen),
        enabled: false,
        checked: None,
    });
    for (id, label) in [("route", &snapshot.route), ("today", &snapshot.today)] {
        if !label.is_empty() {
            entries.push(MenuEntry {
                id: id.to_owned(),
                label: label.clone(),
                enabled: false,
                checked: None,
            });
        }
    }
    if !snapshot.quotas.is_empty() {
        entries.push(separator("separator-quotas"));
        for (index, quota) in snapshot.quotas.iter().enumerate() {
            entries.push(MenuEntry {
                id: format!("quota:{index}"),
                label: quota.clone(),
                enabled: false,
                checked: None,
            });
        }
    }
    if !snapshot.risks.is_empty() {
        entries.push(separator("separator-risks"));
        for (index, risk) in snapshot.risks.iter().enumerate() {
            entries.push(MenuEntry {
                id: format!("risk:{index}"),
                label: risk.clone(),
                enabled: false,
                checked: None,
            });
        }
    }
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

fn agent_catalog() -> Result<hofund_agent::Catalog, String> {
    catalog(agent_descriptions()).map_err(|error| error.to_string())
}

/// The gateway process and the config path the menu is editing.
pub(crate) struct Control {
    config_path: PathBuf,
    server: Option<Running>,
    bound: Option<String>,
    message: String,
    tray: hofund_daemon::TrayStatus,
    quota_alerts: tokio::sync::mpsc::UnboundedSender<QuotaAlertEvent>,
    quota_watch: Option<tokio::task::JoinHandle<()>>,
    quota_relay: Option<tokio::task::JoinHandle<()>>,
    /// Risk lines last computed while the gateway was stopped.
    stopped_risks: Vec<String>,
}

impl Control {
    /// Actions against `config_path`. The file is created on start and on open.
    pub(crate) fn open(
        config_path: impl Into<PathBuf>,
        quota_alerts: tokio::sync::mpsc::UnboundedSender<QuotaAlertEvent>,
    ) -> Self {
        Self {
            config_path: config_path.into(),
            server: None,
            bound: None,
            message: String::new(),
            tray: hofund_daemon::TrayStatus::default(),
            quota_alerts,
            quota_watch: None,
            quota_relay: None,
            stopped_risks: Vec::new(),
        }
    }

    /// Config path for a stopped-gateway risk refresh.
    ///
    /// `None` while the gateway is running. The caller builds the lines
    /// without holding the control mutex, then [`Self::store_stopped_risks`].
    pub(crate) fn stopped_risk_path(&self) -> Option<PathBuf> {
        self.server.is_none().then(|| self.config_path.clone())
    }

    /// Stores lines built while the gateway was stopped.
    ///
    /// A gateway that started during the build keeps the live enrollment
    /// lines. The cache is left unchanged in that case.
    pub(crate) fn store_stopped_risks(&mut self, risks: Vec<String>) {
        if self.server.is_none() {
            self.stopped_risks = risks;
        }
    }

    /// Binds the gateway. A second start while one is running does nothing.
    pub(crate) async fn start(&mut self) -> Result<(), String> {
        if self.server.is_some() {
            self.message.clear();
            self.refresh_tray().await;
            return Ok(());
        }
        if let Err(error) = ensure_config_file(&self.config_path) {
            return self.fail(error.to_string());
        }
        let options = StartOptions {
            control_enabled: true,
            hofund_serves: Some("desktop".to_owned()),
            hofund_owner_pid: Some(std::process::id()),
            ..StartOptions::default()
        };
        match start_with_options(&self.config_path, options).await {
            Ok(running) => {
                self.bound = Some(running.local_addr.to_string());
                if let Some(bus) = control_event_bus(&running) {
                    let (watch, mut events) = bus.spawn_quota_alert_watch();
                    let forward = self.quota_alerts.clone();
                    let relay = tokio::spawn(async move {
                        while let Some(alert) = events.recv().await {
                            let _ = forward.send(alert);
                        }
                    });
                    self.quota_watch = Some(watch);
                    self.quota_relay = Some(relay);
                }
                self.server = Some(running);
                self.message.clear();
                self.refresh_tray().await;
                Ok(())
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Stops the gateway. Stopping when it is already stopped does nothing.
    pub(crate) async fn stop(&mut self) -> Result<(), String> {
        if let Some(task) = self.quota_relay.take() {
            task.abort();
        }
        if let Some(task) = self.quota_watch.take() {
            task.abort();
        }
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
        self.bound = None;
        self.message.clear();
        self.refresh_tray().await;
        Ok(())
    }

    /// Looks up the way out again while the gateway is running.
    pub(crate) async fn recheck_way_out(&mut self) -> Result<(), String> {
        let pending = self.server.as_ref().map(hofund_daemon::recheck_way_out);
        let result = match pending {
            Some(pending) => pending.await.map_err(|error| error.to_string()),
            None => Err("the gateway is stopped".to_owned()),
        };
        self.store(result)
    }

    /// Refreshes the retained route, daily usage, and quota text.
    pub(crate) async fn refresh_tray(&mut self) {
        // Build the future before awaiting so no `&Running` (not `Sync`)
        // is held across the await; Tauri commands need `Send` futures.
        let pending = self
            .server
            .as_ref()
            .map(|server| hofund_daemon::tray_status(server, &self.config_path));
        self.tray = match pending {
            Some(pending) => pending.await,
            None => hofund_daemon::TrayStatus::default(),
        };
    }

    /// Connects or disconnects the catalog agent `id`.
    pub(crate) fn set_agent(
        &mut self,
        session: &Session,
        id: &str,
        connect: bool,
    ) -> Result<(), String> {
        let result = (|| -> Result<Vec<String>, String> {
            let catalog = agent_catalog()?;
            let Some(adapter) = catalog.find(id) else {
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

    /// Current status. Agent rows follow [`hofund_agent::Catalog`] order.
    pub(crate) fn snapshot(&mut self, session: &Session) -> Snapshot {
        let target = local_gateway(&self.config_path).ok().map(|local| {
            GatewayTarget::new(
                local.base_url,
                local.token,
                route_models(&self.config_path).unwrap_or_default(),
            )
        });
        let mut agents = Vec::new();
        match agent_catalog() {
            Ok(catalog) => {
                for adapter in catalog.adapters() {
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
            }
            Err(error) => {
                if self.message.is_empty() {
                    self.message = error;
                }
            }
        }
        let running = self.server.is_some();
        Snapshot {
            running,
            listen: self.listen_label(),
            route: if running {
                self.tray.route.clone()
            } else {
                String::new()
            },
            today: if running {
                self.tray.today.clone()
            } else {
                String::new()
            },
            quotas: if running {
                self.tray.quotas.clone()
            } else {
                Vec::new()
            },
            risks: if let Some(server) = &self.server {
                subscription_risk_lines(server)
            } else {
                self.stopped_risks.clone()
            },
            warnings: self
                .server
                .as_ref()
                .map(hofund_daemon::way_out_warnings)
                .unwrap_or_default(),
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

    use hofund_agent::{Session, catalog};
    use hofund_daemon::agent_descriptions;

    use super::{Control, Snapshot, load_stopped_risks, menu_entries};

    fn agent_catalog() -> hofund_agent::Catalog {
        catalog(agent_descriptions()).expect("agent catalog")
    }

    /// Serializes tests that start a gateway. Without a Unix socket the
    /// control listener binds the fixed default TCP address, so two started
    /// gateways in one test binary would collide.
    static GATEWAY_START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn open(path: &std::path::Path) -> Control {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        Control::open(path, sender)
    }

    #[tokio::test]
    async fn start_and_stop_report_the_bound_address() {
        let _gateway = GATEWAY_START.lock().await;
        let dir = scratch();
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
        let mut control = open(&path);
        let session = Session::new(dir.path());
        control.start().await.expect("start");
        control.refresh_tray().await;
        let snapshot = control.snapshot(&session);
        assert!(snapshot.running);
        assert!(snapshot.listen.starts_with("127.0.0.1:"));
        assert_ne!(snapshot.listen, "127.0.0.1:0");
        assert_eq!(snapshot.route, "Route: no requests yet");
        assert_eq!(snapshot.today, "Today: no requests yet");
        assert!(snapshot.quotas.is_empty());
        let table = control.usage_summary().expect("usage");
        assert!(table.contains("MODEL"));
        control.stop().await.expect("stop");
        let snapshot = control.snapshot(&session);
        assert!(!snapshot.running);
        assert_eq!(snapshot.listen, "127.0.0.1:0");
        assert!(snapshot.route.is_empty());
        assert!(snapshot.today.is_empty());
        assert!(snapshot.quotas.is_empty());
    }

    #[test]
    fn the_menu_lists_every_catalog_agent() {
        let dir = scratch();
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:9\"\n").expect("config");
        let mut control = open(&path);
        let snapshot = control.snapshot(&Session::new(dir.path()));
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
        assert!(entries.iter().all(|entry| {
            !matches!(entry.id.as_str(), "route" | "today" | "separator-quotas")
                && !entry.id.starts_with("quota:")
        }));
        let toggles: Vec<_> = entries
            .iter()
            .filter(|entry| entry.id.starts_with("agent:"))
            .collect();
        let catalog = agent_catalog();
        let adapters = catalog.adapters();
        assert_eq!(toggles.len(), adapters.len());
        for (entry, adapter) in toggles.iter().zip(adapters) {
            assert_eq!(entry.id, format!("agent:{}", adapter.id()));
            assert_eq!(entry.label, adapter.display_name());
            assert_eq!(entry.checked, Some(false));
        }
    }

    #[test]
    fn the_menu_places_route_today_and_quotas_after_listen() {
        let snapshot = Snapshot {
            running: true,
            listen: "127.0.0.1:3425".to_owned(),
            route: "Route: demo → provider/member".to_owned(),
            today: "Today: 1 request, 12.3k tokens".to_owned(),
            quotas: vec!["Work: 42% · 18%".to_owned(), "Plan: 12 credits".to_owned()],
            risks: Vec::new(),
            warnings: Vec::new(),
            message: String::new(),
            agents: Vec::new(),
        };
        let entries = menu_entries(&snapshot);
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "status",
                "listen",
                "route",
                "today",
                "separator-quotas",
                "quota:0",
                "quota:1",
                "gateway-start",
                "gateway-stop",
                "separator-agents",
                "separator-actions",
                "usage",
                "open-config",
                "quit",
            ]
        );
        for (id, label) in [
            ("route", snapshot.route.as_str()),
            ("today", snapshot.today.as_str()),
            ("quota:0", snapshot.quotas[0].as_str()),
            ("quota:1", snapshot.quotas[1].as_str()),
        ] {
            let entry = entries.iter().find(|entry| entry.id == id).expect(id);
            assert_eq!(entry.label, label);
            assert!(!entry.enabled);
            assert_eq!(entry.checked, None);
        }
        let separator = entries
            .iter()
            .find(|entry| entry.id == "separator-quotas")
            .expect("quota separator");
        assert!(separator.label.is_empty());
        assert!(!separator.enabled);

        let empty = Snapshot {
            route: String::new(),
            today: String::new(),
            quotas: Vec::new(),
            ..snapshot
        };
        let entries = menu_entries(&empty);
        assert!(entries.iter().all(|entry| {
            !matches!(entry.id.as_str(), "route" | "today" | "separator-quotas")
                && !entry.id.starts_with("quota:")
        }));
    }

    #[test]
    fn d245_way_out_warning_precedes_status_and_check_again_needs_a_running_gateway() {
        let base = Snapshot {
            running: true,
            listen: "127.0.0.1:3425".to_owned(),
            route: String::new(),
            today: String::new(),
            quotas: Vec::new(),
            risks: Vec::new(),
            warnings: vec!["outside".to_owned()],
            message: String::new(),
            agents: Vec::new(),
        };
        let running = menu_entries(&base);
        assert_eq!(running[0].id, "way-out:0");
        assert_eq!(running[0].label, "outside");
        assert!(!running[0].enabled);
        assert_eq!(running[1].id, "way-out-check");
        assert_eq!(running[1].label, "Check again");
        assert!(running[1].enabled);
        assert_eq!(running[2].id, "status");

        let stopped = Snapshot {
            running: false,
            ..base
        };
        let entries = menu_entries(&stopped);
        assert_eq!(entries[0].id, "way-out:0");
        assert!(entries.iter().all(|entry| entry.id != "way-out-check"));
    }

    #[test]
    fn d221_the_menu_shows_a_saved_account_risk_line() {
        let snapshot = Snapshot {
            running: false,
            listen: "127.0.0.1:3425".to_owned(),
            route: String::new(),
            today: String::new(),
            quotas: Vec::new(),
            risks: vec!["saved-account risk line".to_owned()],
            warnings: Vec::new(),
            message: String::new(),
            agents: Vec::new(),
        };
        let entries = menu_entries(&snapshot);
        let risk = entries
            .iter()
            .find(|entry| entry.id == "risk:0")
            .expect("risk");
        assert_eq!(risk.label, "saved-account risk line");
        assert!(!risk.enabled);
        assert!(
            entries
                .iter()
                .any(|entry| entry.id == "separator-risks" && entry.label.is_empty())
        );
    }

    #[test]
    fn connecting_an_agent_round_trips() {
        let dir = scratch();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("home");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:3425\"\n").expect("config");
        let mut control = open(&path);
        let session = Session::new(&home);
        let catalog = agent_catalog();
        let id = catalog.adapters()[0].id();
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
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_does_not_build_plugins_or_run_account_hooks() {
        let _gateway = GATEWAY_START.lock().await;
        let dir = scratch();
        let path = dir.path().join("hofund.toml");
        fs::write(
            &path,
            "listen = \"127.0.0.1:0\"\n\
             [[subscriptions]]\n\
             id = \"antigravity\"\n\
             accounts = \"*\"\n\
             \n\
             [subscriptions.options]\n\
             client_id = \"desktop-test-client\"\n\
             client_secret = \"desktop-test-secret\"\n",
        )
        .expect("config");
        let mut control = open(&path);
        let session = Session::new(dir.path());
        let builds = hofund_daemon::subscription_plugin_builds();
        let hooks = hofund_daemon::subscription_account_hooks();
        let stopped = control.snapshot(&session);
        assert!(!stopped.running);
        assert!(stopped.risks.is_empty());
        assert_eq!(hofund_daemon::subscription_plugin_builds(), builds);
        assert_eq!(hofund_daemon::subscription_account_hooks(), hooks);
        assert!(!dir.path().join(".hofund").exists());

        let path = control.stopped_risk_path().expect("stopped");
        let risks = load_stopped_risks(path).await;
        control.store_stopped_risks(risks);
        let cached = control.snapshot(&session);
        assert!(cached.risks.iter().any(|line| line.contains("Antigravity")));
        assert!(hofund_daemon::subscription_plugin_builds() > builds);
        assert_eq!(hofund_daemon::subscription_account_hooks(), hooks);
        let after = hofund_daemon::subscription_plugin_builds();
        let again = control.snapshot(&session);
        assert_eq!(again.risks, cached.risks);
        assert_eq!(hofund_daemon::subscription_plugin_builds(), after);
        assert_eq!(hofund_daemon::subscription_account_hooks(), hooks);

        control.start().await.expect("start");
        let running_builds = hofund_daemon::subscription_plugin_builds();
        let running_hooks = hofund_daemon::subscription_account_hooks();
        let live = control.snapshot(&session);
        assert!(live.running);
        assert!(live.risks.iter().any(|line| line.contains("Antigravity")));
        assert_eq!(hofund_daemon::subscription_plugin_builds(), running_builds);
        assert_eq!(hofund_daemon::subscription_account_hooks(), running_hooks);
        control.stop().await.expect("stop");
    }
}
