//! The configuration transaction behind the control plane's config routes.
//!
//! Every write and reload runs under one in-process gate and the sidecar file
//! lock. Steps that can fail (revision check, validation, provider
//! construction, the write itself) all run before anything is published, so a
//! failure leaves the file, the router snapshot, the generation and the
//! cooldowns unchanged.
//!
//! After publication the model listener follows the new `listen` (L6d). A
//! failed move does not undo the publication; it is reported in
//! `not_applied` and as `listener_relisten_failed`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hofund_config::{Config, ConfigError, VisibleCatalog};
use hofund_control_proto::{
    ConfigRejectedEvent, ConfigReloadedEvent, ConfigResponse, ConfigValidationResponse,
    ControlError, ControlErrorKind, EventPayload, FieldIssue, ListenSource,
    ListenerRelistenFailedEvent, SnapshotPublishedEvent,
};
use hofund_gateway::control::ControlFailure;
use hofund_gateway::{Relisten, RelistenError};
use hofund_router::Snapshot;
use tokio::sync::watch;

use crate::config_write::{SidecarLock, WriteFailure, atomic_write, primary_lock_path};
use crate::control::revision;
use crate::events::EventBus;
use crate::runtime::Runtime;

/// Reason code for a configured `listen` that `HOFUND_ADDR` overrides.
pub(crate) const ENV_OVERRIDE: &str = "env_override";
/// Reason code for a `listen` the model listener could not bind; the old
/// address is still served.
pub(crate) const BIND_FAILED: &str = "bind_failed";
/// Reason code for a relisten that lost the old address too. The daemon exits.
pub(crate) const RESTORE_FAILED: &str = "restore_failed";
/// Reason code for a relisten after the model listener stopped.
pub(crate) const LISTENER_STOPPED: &str = "listener_stopped";
const ENV_OVERRIDE_MESSAGE: &str = "HOFUND_ADDR overrides the configured listener";

/// The model listener as the configuration transaction sees it.
pub(crate) struct ModelListener {
    /// `HOFUND_ADDR`, resolved at startup. When set the listener never moves.
    env: Option<SocketAddr>,
    /// Moves the running listener. `None` in unit tests without a server.
    handle: Option<Relisten>,
    /// Reports a lost listener; the daemon then shuts down and exits non-zero.
    fatal: watch::Sender<Option<String>>,
}

/// The listener addresses that status and snapshots report.
pub(crate) struct ListenerView {
    pub(crate) configured: SocketAddr,
    pub(crate) effective: SocketAddr,
    pub(crate) running: Option<SocketAddr>,
    pub(crate) source: ListenSource,
}

impl ModelListener {
    pub(crate) fn new(
        env: Option<SocketAddr>,
        handle: Option<Relisten>,
        fatal: watch::Sender<Option<String>>,
    ) -> Self {
        Self { env, handle, fatal }
    }

    fn view(&self, config: &Config) -> ListenerView {
        let (effective, source) = match self.env {
            Some(env) => (env, ListenSource::Environment),
            None => (config.listen, ListenSource::Config),
        };
        ListenerView {
            configured: config.listen,
            effective,
            running: self.handle.as_ref().map(Relisten::local_addr),
            source,
        }
    }

    /// The `listen` issue while `HOFUND_ADDR` overrides a different value.
    fn env_issue(&self, config: &Config) -> Option<FieldIssue> {
        let env = self.env?;
        (env != config.listen)
            .then(|| FieldIssue::new("listen", ENV_OVERRIDE, ENV_OVERRIDE_MESSAGE))
    }

    /// Moves the listener to `config`'s `listen` and returns the `listen`
    /// issue to report, plus whether `listener_relisten_failed` is due.
    async fn follow(&self, config: &Config) -> Option<(FieldIssue, bool)> {
        if self.env.is_some() {
            return self.env_issue(config).map(|issue| (issue, false));
        }
        let handle = self.handle.as_ref()?;
        match handle.relisten(config.listen).await {
            Ok(_) => None,
            Err(error @ RelistenError::Bind { .. }) => Some((
                FieldIssue::new("listen", BIND_FAILED, error.to_string()),
                true,
            )),
            Err(error @ RelistenError::RestoreFailed { .. }) => {
                let message = format!("the model listener is lost: {error}");
                tracing::error!(%error, "the model listener could not be restored; shutting down");
                self.fatal.send_replace(Some(message.clone()));
                Some((FieldIssue::new("listen", RESTORE_FAILED, message), true))
            }
            Err(error) => Some((
                FieldIssue::new("listen", LISTENER_STOPPED, error.to_string()),
                false,
            )),
        }
    }
}

/// A candidate the runtime has published, waiting for the listener step.
struct Staged {
    config: Arc<Config>,
    runtime_revision: String,
    disk_revision: String,
    text: String,
    changed: Vec<String>,
}

/// What the blocking part of a reload produced.
enum Reloaded {
    /// Nothing to publish; respond with this.
    Unchanged(ConfigResponse),
    Staged(Staged),
}

/// The configuration currently serving traffic.
#[derive(Clone, Debug)]
pub(crate) struct LiveConfig {
    pub(crate) config: Arc<Config>,
    pub(crate) runtime_revision: String,
    pub(crate) generation: u64,
    pub(crate) pending_restart: Vec<String>,
    pub(crate) not_applied: Vec<FieldIssue>,
}

/// Who asked for a reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReloadOrigin {
    /// `POST /config/reload`: always re-publishes, even at an equal revision.
    Explicit,
    /// The file watcher (L6c): skips a revision that is already published.
    Watcher,
}

/// Listener and model-list state shared with config edits.
pub(crate) struct ConfigOpsRouting {
    pub(crate) listener: ModelListener,
    pub(crate) visible_catalog: Arc<RwLock<VisibleCatalog>>,
    pub(crate) snapshot: Snapshot,
}

pub(crate) struct ConfigOps {
    gate: tokio::sync::Mutex<()>,
    path: PathBuf,
    lock_fallback: Option<PathBuf>,
    runtime: Arc<Runtime>,
    bus: Arc<EventBus>,
    /// The configuration the process started with. Restart-only settings are
    /// compared against it, so reverting an edit clears `pending_restart`.
    started: Arc<Config>,
    live: RwLock<Arc<LiveConfig>>,
    routing: ConfigOpsRouting,
}

impl ConfigOps {
    pub(crate) fn new(
        path: &Path,
        lock_fallback: Option<PathBuf>,
        runtime: Arc<Runtime>,
        bus: Arc<EventBus>,
        config: Arc<Config>,
        runtime_revision: String,
        routing: ConfigOpsRouting,
    ) -> Self {
        let live = LiveConfig {
            config: Arc::clone(&config),
            runtime_revision,
            generation: 1,
            pending_restart: Vec::new(),
            not_applied: routing.listener.env_issue(&config).into_iter().collect(),
        };
        Self {
            gate: tokio::sync::Mutex::new(()),
            path: path.to_path_buf(),
            lock_fallback,
            runtime,
            bus,
            started: config,
            live: RwLock::new(Arc::new(live)),
            routing,
        }
    }

    /// Route model ids an agent is shown, after visibility filters.
    pub(crate) fn agent_route_models(
        &self,
        agent: Option<&str>,
        active_model: Option<&str>,
    ) -> Vec<String> {
        self.agent_models(agent, active_model).0
    }

    /// Shown and hidden route ids for one agent.
    ///
    /// `active_model` stays in the shown list even when a deny list names it.
    pub(crate) fn agent_models(
        &self,
        agent: Option<&str>,
        active_model: Option<&str>,
    ) -> (Vec<String>, Vec<String>) {
        crate::agent_visible::models_for_agent(
            &self.routing.visible_catalog,
            &self.routing.snapshot,
            agent,
            active_model,
        )
    }

    async fn apply_edited_config(
        self: &Arc<Self>,
        edit: impl FnOnce(&str) -> Result<String, ConfigError>,
    ) -> Result<ConfigResponse, ControlFailure> {
        let current = self.get().await?;
        let text = edit(&current.text).map_err(|error| {
            ControlFailure::from(ControlError::new(
                ControlErrorKind::InvalidConfig,
                error.to_string(),
                "config",
            ))
        })?;
        self.apply(&current.disk_revision, text).await
    }

    pub(crate) async fn set_agent_visible(
        self: &Arc<Self>,
        agent: &str,
        names: Vec<String>,
    ) -> Result<ConfigResponse, ControlFailure> {
        let agent = agent.to_owned();
        self.apply_edited_config(move |text| {
            hofund_config::edit_agent_visible(text, &agent, &names)
        })
        .await
    }

    pub(crate) async fn set_agent_hidden_models(
        self: &Arc<Self>,
        agent: &str,
        ids: Vec<String>,
    ) -> Result<ConfigResponse, ControlFailure> {
        let agent = agent.to_owned();
        self.apply_edited_config(move |text| {
            hofund_config::edit_agent_hidden_models(text, &agent, &ids)
        })
        .await
    }

    pub(crate) async fn set_agent_picked_only(
        self: &Arc<Self>,
        agent: &str,
        only: bool,
    ) -> Result<ConfigResponse, ControlFailure> {
        let agent = agent.to_owned();
        self.apply_edited_config(move |text| {
            hofund_config::edit_agent_picked_only(text, &agent, only)
        })
        .await
    }

    /// The listener addresses for the published configuration.
    pub(crate) fn listener(&self) -> ListenerView {
        self.routing.listener.view(&self.live().config)
    }

    /// The published configuration.
    pub(crate) fn live(&self) -> Arc<LiveConfig> {
        Arc::clone(&self.live.read().expect("live config"))
    }

    /// Revision of the bytes on disk now, or empty when unreadable.
    pub(crate) fn disk_revision(&self) -> String {
        std::fs::read(&self.path)
            .map(|bytes| revision(&bytes))
            .unwrap_or_default()
    }

    pub(crate) async fn get(&self) -> Result<ConfigResponse, ControlFailure> {
        let bytes = std::fs::read(&self.path).map_err(|error| read_failure(&self.path, &error))?;
        let text = String::from_utf8(bytes).map_err(|_| not_utf8())?;
        Ok(response(&self.live(), revision(text.as_bytes()), text))
    }

    /// Validates `text` as a complete candidate. Nothing is written or
    /// published.
    pub(crate) async fn validate(&self, text: &str) -> ConfigValidationResponse {
        let mut response = ConfigValidationResponse::default();
        match self.check(text) {
            Ok((config, _candidate)) => {
                response.valid = true;
                response.pending_restart = restart_diff(&self.started, &config);
            }
            Err(issues) => {
                response.valid = false;
                response.issues = issues;
            }
        }
        response
    }

    pub(crate) async fn apply(
        self: &Arc<Self>,
        expected: &str,
        text: String,
    ) -> Result<ConfigResponse, ControlFailure> {
        self.apply_with_key_moves(expected, text, Vec::new()).await
    }

    pub(crate) async fn apply_with_key_moves(
        self: &Arc<Self>,
        expected: &str,
        text: String,
        key_moves: Vec<(hofund_plugin_api::MemberId, hofund_plugin_api::MemberId)>,
    ) -> Result<ConfigResponse, ControlFailure> {
        let _gate = self.gate.lock().await;
        let ops = Arc::clone(self);
        let expected = expected.to_owned();
        let staged = run_blocking(move || ops.apply_locked(&expected, text, key_moves)).await?;
        Ok(self.finish(staged).await)
    }

    pub(crate) async fn reload(
        self: &Arc<Self>,
        expected: Option<&str>,
        origin: ReloadOrigin,
    ) -> Result<ConfigResponse, ControlFailure> {
        let _gate = self.gate.lock().await;
        let ops = Arc::clone(self);
        let expected = expected.map(str::to_owned);
        match run_blocking(move || ops.reload_locked(expected.as_deref(), origin)).await? {
            Reloaded::Unchanged(response) => Ok(response),
            Reloaded::Staged(staged) => Ok(self.finish(staged).await),
        }
    }

    fn lock(&self) -> Result<SidecarLock, ControlFailure> {
        let primary = primary_lock_path(&self.path);
        let lock =
            SidecarLock::acquire(&primary, self.lock_fallback.as_deref()).map_err(|error| {
                internal(format!(
                    "cannot lock the configuration in {}: {error}",
                    primary.parent().unwrap_or(Path::new(".")).display()
                ))
            })?;
        if lock.path() != primary {
            tracing::debug!(lock = %lock.path().display(), "configuration lock uses the runtime directory");
        }
        Ok(lock)
    }

    fn read_disk(&self) -> Result<(String, String), ControlFailure> {
        let bytes = std::fs::read(&self.path).map_err(|error| read_failure(&self.path, &error))?;
        let disk_revision = revision(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| not_utf8())?;
        Ok((disk_revision, text))
    }

    fn check(&self, text: &str) -> Result<(Config, crate::runtime::Candidate), Vec<FieldIssue>> {
        let config = hofund_config::load(text).map_err(|error| vec![config_issue(&error, text)])?;
        if let Err(error) = crate::account_discovery::import_if_configured(&self.path, &config) {
            return Err(vec![FieldIssue::new(
                "subscriptions",
                "discover_failed",
                scrub(&error.to_string(), text),
            )]);
        }
        let candidate = self.runtime.prepare(&config).map_err(|error| {
            vec![FieldIssue::new(
                "providers",
                "build_failed",
                scrub(&error.to_string(), text),
            )]
        })?;
        Ok((config, candidate))
    }

    fn apply_locked(
        &self,
        expected: &str,
        text: String,
        key_moves: Vec<(hofund_plugin_api::MemberId, hofund_plugin_api::MemberId)>,
    ) -> Result<Staged, ControlFailure> {
        let _lock = self.lock()?;
        let (disk_revision, _) = self.read_disk()?;
        if disk_revision != expected {
            return Err(stale_revision());
        }
        let written = revision(text.as_bytes());
        let (_, candidate) = self.check(&text).map_err(|issues| {
            self.reject(&written, issues.clone());
            invalid(issues)
        })?;
        atomic_write(&self.path, text.as_bytes()).map_err(|failure| {
            self.reject(
                &written,
                vec![FieldIssue::new(
                    "config",
                    "write_failed",
                    format!("cannot write in {}", failure.directory.display()),
                )],
            );
            write_failure(failure)
        })?;
        self.publish(candidate, written.clone(), text, key_moves)
    }

    /// Emits `config_rejected` for a candidate that was not published.
    fn reject(&self, rejected: &str, issues: Vec<FieldIssue>) {
        let mut event = ConfigRejectedEvent::default();
        rejected.clone_into(&mut event.revision);
        event.issues = issues;
        self.bus.emit(EventPayload::ConfigRejected(event));
    }

    fn reload_locked(
        &self,
        expected: Option<&str>,
        origin: ReloadOrigin,
    ) -> Result<Reloaded, ControlFailure> {
        let _lock = self.lock()?;
        let (disk_revision, text) = self.read_disk()?;
        if expected.is_some_and(|expected| expected != disk_revision) {
            return Err(stale_revision());
        }
        let current = self.live();
        if origin == ReloadOrigin::Watcher && current.runtime_revision == disk_revision {
            return Ok(Reloaded::Unchanged(response(&current, disk_revision, text)));
        }
        let (_, candidate) = match self.check(&text) {
            Ok(checked) => checked,
            Err(issues) => {
                self.reject(&disk_revision, issues.clone());
                return Err(invalid(issues));
            }
        };
        self.publish(candidate, disk_revision.clone(), text, Vec::new())
            .map(Reloaded::Staged)
    }

    /// Publishes the router state. The live record and the events wait for
    /// [`Self::finish`], so they can report the listener step.
    fn publish(
        &self,
        candidate: crate::runtime::Candidate,
        runtime_revision: String,
        text: String,
        key_moves: Vec<(hofund_plugin_api::MemberId, hofund_plugin_api::MemberId)>,
    ) -> Result<Staged, ControlFailure> {
        let config = Arc::clone(candidate.config());
        let changed = self
            .runtime
            .publish(candidate, &key_moves)
            .map_err(|error| internal(format!("publication failed: {error}")))?;
        Ok(Staged {
            config,
            disk_revision: runtime_revision.clone(),
            runtime_revision,
            text,
            changed,
        })
    }

    /// Moves the listener, then records and announces the publication.
    async fn finish(&self, staged: Staged) -> ConfigResponse {
        let listen = self.routing.listener.follow(&staged.config).await;
        let mut live = self.live.write().expect("live config");
        let next = Arc::new(LiveConfig {
            pending_restart: restart_diff(&self.started, &staged.config),
            not_applied: listen.iter().map(|(issue, _)| issue.clone()).collect(),
            config: staged.config,
            runtime_revision: staged.runtime_revision,
            generation: live.generation + 1,
        });
        *live = Arc::clone(&next);
        drop(live);
        crate::agent_visible::rebuild_catalog(
            &self.routing.visible_catalog,
            Arc::clone(&next.config),
            &self.routing.snapshot,
        );
        let mut event = ConfigReloadedEvent::default();
        event.disk_revision.clone_from(&next.runtime_revision);
        event.runtime_revision.clone_from(&next.runtime_revision);
        event.generation = next.generation;
        event.changed_members = staged.changed;
        event.pending_restart.clone_from(&next.pending_restart);
        event.not_applied.clone_from(&next.not_applied);
        self.bus.emit(EventPayload::ConfigReloaded(event));
        let mut published = SnapshotPublishedEvent::default();
        published.generation = next.generation;
        published.routes = u32::try_from(next.config.routes.len()).unwrap_or(u32::MAX);
        published.members = u32::try_from(next.config.providers.len()).unwrap_or(u32::MAX);
        self.bus.emit(EventPayload::SnapshotPublished(published));
        if let Some((issue, true)) = listen {
            let mut event = ListenerRelistenFailedEvent::default();
            event.issue = issue;
            event.running = self
                .routing
                .listener
                .view(&next.config)
                .running
                .map(|running| running.to_string())
                .unwrap_or_default();
            event.disk_revision.clone_from(&next.runtime_revision);
            event.runtime_revision.clone_from(&next.runtime_revision);
            event.generation = next.generation;
            self.bus.emit(EventPayload::ListenerRelistenFailed(event));
        }
        response(&next, staged.disk_revision, staged.text)
    }
}

/// Settings that only take effect at startup, compared against the
/// configuration the process started with.
pub(crate) fn restart_diff(running: &Config, candidate: &Config) -> Vec<String> {
    let mut pending = Vec::new();
    if running.token_file != candidate.token_file {
        pending.push("token_file".to_owned());
    }
    if running.usage_file != candidate.usage_file {
        pending.push("usage_file".to_owned());
    }
    pending
}

fn response(live: &LiveConfig, disk_revision: String, text: String) -> ConfigResponse {
    let mut response = ConfigResponse::default();
    response.disk_revision = disk_revision;
    response.runtime_revision.clone_from(&live.runtime_revision);
    response.generation = live.generation;
    response.text = text;
    response.pending_restart.clone_from(&live.pending_restart);
    response.not_applied.clone_from(&live.not_applied);
    response
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ControlFailure> + Send + 'static,
) -> Result<T, ControlFailure> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|_| Err(internal("configuration task failed".to_owned())))
}

/// One sanitized issue for a load failure. TOML syntax errors quote the
/// offending source line, which may hold a secret, so only the position line
/// and the final description are kept.
fn config_issue(error: &ConfigError, text: &str) -> FieldIssue {
    match error {
        ConfigError::Syntax(message) => {
            let mut lines = message
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty());
            let position = lines.next().unwrap_or("TOML parse error");
            let description = lines
                .next_back()
                .filter(|line| !line.starts_with('|') && !line.contains('|'))
                .unwrap_or("invalid TOML");
            FieldIssue::new("config", "syntax", format!("{position}: {description}"))
        }
        ConfigError::Invalid(message) => FieldIssue::new("config", "invalid", scrub(message, text)),
        ConfigError::Read(message) => FieldIssue::new("config", "read", scrub(message, text)),
    }
}

/// Removes any quoted value from `message` that appears verbatim in `text`
/// as a string value, so a validation or provider construction error cannot
/// echo a secret.
fn scrub(message: &str, text: &str) -> String {
    let mut scrubbed = message.to_owned();
    for value in text.split('"').skip(1).step_by(2) {
        if value.len() >= 8 && scrubbed.contains(value) {
            scrubbed = scrubbed.replace(value, "[redacted]");
        }
    }
    scrubbed
}

fn invalid(issues: Vec<FieldIssue>) -> ControlFailure {
    let mut error = ControlError::new(
        ControlErrorKind::InvalidConfig,
        "the configuration is not valid; nothing was changed",
        "control",
    );
    error.details = issues;
    error.into()
}

fn stale_revision() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::Conflict,
        "the configuration file changed on disk; read it again and retry",
        "control",
    )
    .into()
}

fn write_failure(failure: WriteFailure) -> ControlFailure {
    internal(format!(
        "cannot write the configuration in {}: {}",
        failure.directory.display(),
        failure.error
    ))
}

fn read_failure(path: &Path, error: &std::io::Error) -> ControlFailure {
    internal(format!(
        "cannot read the configuration at {}: {error}",
        path.display()
    ))
}

fn not_utf8() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::InvalidConfig,
        "the configuration file is not UTF-8",
        "control",
    )
    .into()
}

fn internal(message: String) -> ControlFailure {
    ControlError::new(ControlErrorKind::Internal, message, "control").into()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use hofund_config::VisibleCatalog;
    use hofund_control_proto::{ControlErrorKind, EventEnvelope, EventPayload};
    use hofund_gateway::control::ControlFailure;
    use hofund_plugin_api::PluginError;
    use hofund_router::Snapshot;

    use super::{
        ConfigOps, ConfigOpsRouting, ENV_OVERRIDE, ModelListener, ReloadOrigin, restart_diff,
    };
    use crate::RegistryAdapter;
    use crate::control::revision;
    use crate::events::EventBus;
    use crate::membership::member_id;
    use crate::runtime::Runtime;

    const SECRET: &str = "sk-config-ops-secret-value";

    fn provider(id: &str, key: &str) -> String {
        format!(
            "[[providers]]\nid = \"{id}\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{key}\"\n"
        )
    }

    fn document(extra: &str) -> String {
        format!(
            "listen = \"127.0.0.1:0\"\n{}{}[[routes]]\nmodel = \"m\"\nupstreams = [\"first\", \"second\"]\n{extra}",
            provider("first", SECRET),
            provider("second", "second-provider-key"),
        )
    }

    struct Fixture {
        ops: Arc<ConfigOps>,
        runtime: Arc<Runtime>,
        bus: Arc<EventBus>,
        snapshot: Snapshot,
        path: std::path::PathBuf,
        dir: tempfile::TempDir,
        _runtime_dir: tempfile::TempDir,
    }

    fn fixture(text: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("dir");
        let runtime_dir = tempfile::tempdir().expect("runtime dir");
        let path = dir.path().join("hofund.toml");
        std::fs::write(&path, text).expect("seed");
        let config = hofund_config::load(text).expect("config");
        let config = Arc::new(config);
        let runtime = Runtime::new(Arc::new(RegistryAdapter), &path, None);
        let snapshot = runtime
            .initial_snapshot(config.as_ref(), None)
            .expect("snapshot");
        let bus = Arc::new(EventBus::new());
        let ops = Arc::new(ConfigOps::new(
            &path,
            Some(runtime_dir.path().join("config-test.lock")),
            Arc::clone(&runtime),
            Arc::clone(&bus),
            Arc::clone(&config),
            revision(text.as_bytes()),
            ConfigOpsRouting {
                listener: ModelListener::new(None, None, tokio::sync::watch::channel(None).0),
                visible_catalog: Arc::new(RwLock::new(VisibleCatalog::build(
                    Arc::clone(&config),
                    &snapshot.load().models(),
                ))),
                snapshot: snapshot.clone(),
            },
        ));
        Fixture {
            ops,
            runtime,
            bus,
            snapshot,
            path,
            dir,
            _runtime_dir: runtime_dir,
        }
    }

    fn events(bus: &EventBus) -> Vec<EventPayload> {
        bus.control_frames()
            .iter()
            .map(|json| {
                serde_json::from_str::<EventEnvelope>(json)
                    .expect("envelope")
                    .payload
            })
            .collect()
    }

    fn kind(failure: &ControlFailure) -> ControlErrorKind {
        failure.error.kind
    }

    fn cool(runtime: &Runtime, id: &str) {
        runtime
            .cooldown_book()
            .record_failure(&member_id(id), &PluginError::connect("down"));
    }

    /// Everything a failed transaction must leave untouched.
    fn assert_unchanged(fixture: &Fixture, original: &str, router: &Arc<hofund_router::Router>) {
        assert_eq!(
            std::fs::read_to_string(&fixture.path).expect("read"),
            original
        );
        let live = fixture.ops.live();
        assert_eq!(live.generation, 1);
        assert_eq!(live.runtime_revision, revision(original.as_bytes()));
        assert!(Arc::ptr_eq(router, &fixture.snapshot.load()));
        assert!(
            fixture
                .runtime
                .cooldown_book()
                .cooling(&member_id("first"))
                .is_some()
        );
        assert!(
            !events(&fixture.bus)
                .iter()
                .any(|event| matches!(event, EventPayload::ConfigReloaded(_)))
        );
    }

    #[tokio::test]
    async fn apply_bad_toml_leaves_file_snapshot_generation_and_cooldowns_unchanged() {
        let original = document("");
        let bad_toml = format!("{original}\n[[providers]\nid = \"{SECRET}\"\n");
        let bad_provider = format!(
            "{}[[providers]]\nid = \"third\"\npreset = \"openai\"\napi_key = \"third-key\"\n[providers.options]\nsecret_access_key = \"SENTINEL-OPTION-VALUE\"\n",
            original
        );
        for (label, candidate) in [("toml", bad_toml), ("provider", bad_provider)] {
            let fixture = fixture(&original);
            cool(&fixture.runtime, "first");
            let router = fixture.snapshot.load();
            let failure = fixture
                .ops
                .apply(&revision(original.as_bytes()), candidate.clone())
                .await
                .expect_err(label);
            assert_eq!(kind(&failure), ControlErrorKind::InvalidConfig, "{label}");
            assert!(!failure.error.details.is_empty(), "{label}");
            let rendered = format!("{:?}", failure.error);
            assert!(!rendered.contains(SECRET), "{label}: {rendered}");
            assert!(!rendered.contains("SENTINEL-OPTION-VALUE"), "{label}");
            assert_unchanged(&fixture, &original, &router);
            let rejected = events(&fixture.bus);
            assert!(
                matches!(
                    rejected.as_slice(),
                    [EventPayload::ConfigRejected(event)]
                        if event.revision == revision(candidate.as_bytes())
                ),
                "{label}: {rejected:?}"
            );
        }
    }

    #[tokio::test]
    async fn apply_stale_revision_leaves_file_snapshot_generation_and_cooldowns_unchanged() {
        let original = document("");
        let fixture = fixture(&original);
        cool(&fixture.runtime, "first");
        let router = fixture.snapshot.load();
        let failure = fixture
            .ops
            .apply(&"0".repeat(64), document("# candidate\n"))
            .await
            .expect_err("stale");
        assert_eq!(kind(&failure), ControlErrorKind::Conflict);
        assert_unchanged(&fixture, &original, &router);
        assert!(events(&fixture.bus).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_write_failure_leaves_file_snapshot_generation_and_cooldowns_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let original = document("");
        let fixture = fixture(&original);
        cool(&fixture.runtime, "first");
        let router = fixture.snapshot.load();
        let dir = fixture.dir.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        if std::fs::write(dir.join("probe"), "").is_ok() {
            // Privileged users bypass directory permissions.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            return;
        }
        let result = fixture
            .ops
            .apply(&revision(original.as_bytes()), document("# candidate\n"))
            .await;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let failure = result.expect_err("read-only directory");
        assert_eq!(kind(&failure), ControlErrorKind::Internal);
        assert!(
            failure.error.message.contains(
                &std::fs::canonicalize(dir)
                    .expect("dir")
                    .display()
                    .to_string()
            ),
            "{}",
            failure.error.message
        );
        assert_unchanged(&fixture, &original, &router);
        assert!(matches!(
            events(&fixture.bus).as_slice(),
            [EventPayload::ConfigRejected(_)]
        ));
    }

    #[tokio::test]
    async fn provider_fingerprint_change_clears_only_that_cooldown() {
        let original = document("");
        let fixture = fixture(&original);
        cool(&fixture.runtime, "first");
        cool(&fixture.runtime, "second");
        let router = fixture.snapshot.load();
        let candidate = original.replace("second-provider-key", "rotated-provider-key");
        let response = fixture
            .ops
            .apply(&revision(original.as_bytes()), candidate.clone())
            .await
            .unwrap_or_else(|failure| panic!("{}: {:?}", "apply", failure.error));
        assert_eq!(response.generation, 2);
        assert_eq!(response.disk_revision, revision(candidate.as_bytes()));
        assert_eq!(response.runtime_revision, response.disk_revision);
        assert_eq!(response.text, candidate);
        assert_eq!(
            std::fs::read(&fixture.path).expect("read"),
            candidate.as_bytes()
        );
        assert!(!Arc::ptr_eq(&router, &fixture.snapshot.load()));
        let book = fixture.runtime.cooldown_book();
        assert!(book.cooling(&member_id("first")).is_some());
        assert!(book.cooling(&member_id("second")).is_none());
        match events(&fixture.bus).as_slice() {
            [
                EventPayload::ConfigReloaded(event),
                EventPayload::SnapshotPublished(published),
            ] => {
                assert_eq!(event.generation, 2);
                assert_eq!(event.changed_members, [member_id("second").to_string()]);
                assert_eq!(event.runtime_revision, response.runtime_revision);
                assert_eq!(published.generation, 2);
                assert_eq!(published.routes, 1);
                assert_eq!(published.members, 2);
            }
            other => panic!("expected config_reloaded then snapshot_published, got {other:?}"),
        }
        assert!(!format!("{:?}", fixture.bus.control_frames()).contains(SECRET));
    }

    #[tokio::test]
    async fn k6_replacement_move_survives_config_publication() {
        let original = "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"first\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"first-key\"\n[[providers.keys]]\nkey = \"old-key\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"first\"]\n";
        let fixture = fixture(original);
        let old =
            crate::membership::key_member_id("first", &crate::provider_keys::key_id("old-key"));
        let new =
            crate::membership::key_member_id("first", &crate::provider_keys::key_id("new-key"));
        fixture
            .runtime
            .cooldown_book()
            .record_failure(&old, &PluginError::credit_status(402, "out of credit"));
        let candidate = original.replace("old-key", "new-key");
        fixture
            .ops
            .apply_with_key_moves(
                &revision(original.as_bytes()),
                candidate,
                vec![(old.clone(), new.clone())],
            )
            .await
            .unwrap_or_else(|failure| panic!("apply: {:?}", failure.error));
        let book = fixture.runtime.cooldown_book();
        assert!(book.cooling(&old).is_none());
        assert!(book.cooling(&new).is_some());
    }

    #[tokio::test]
    async fn explicit_reload_republishes_an_equal_revision_and_watcher_skips_it() {
        let original = document("");
        let fixture = fixture(&original);
        let expected = revision(original.as_bytes());
        let skipped = fixture
            .ops
            .reload(Some(&expected), ReloadOrigin::Watcher)
            .await
            .unwrap_or_else(|failure| panic!("{}: {:?}", "watcher", failure.error));
        assert_eq!(skipped.generation, 1);
        assert!(events(&fixture.bus).is_empty());
        let explicit = fixture
            .ops
            .reload(Some(&expected), ReloadOrigin::Explicit)
            .await
            .unwrap_or_else(|failure| panic!("{}: {:?}", "explicit", failure.error));
        assert_eq!(explicit.generation, 2);
        let stale = fixture
            .ops
            .reload(Some(&"f".repeat(64)), ReloadOrigin::Explicit)
            .await
            .expect_err("stale");
        assert_eq!(kind(&stale), ControlErrorKind::Conflict);
        assert_eq!(fixture.ops.live().generation, 2);
    }

    #[tokio::test]
    async fn invalid_external_edit_is_rejected_and_left_on_disk() {
        let original = document("");
        let fixture = fixture(&original);
        let edit = format!("{original}[[routes]]\nmodel = \"x\"\nupstreams = [\"missing\"]\n");
        std::fs::write(&fixture.path, &edit).expect("edit");
        let failure = fixture
            .ops
            .reload(None, ReloadOrigin::Watcher)
            .await
            .expect_err("invalid");
        assert_eq!(kind(&failure), ControlErrorKind::InvalidConfig);
        assert_eq!(std::fs::read_to_string(&fixture.path).expect("read"), edit);
        let live = fixture.ops.live();
        assert_eq!(live.generation, 1);
        assert_eq!(live.runtime_revision, revision(original.as_bytes()));
        assert_ne!(fixture.ops.disk_revision(), live.runtime_revision);
        assert!(matches!(
            events(&fixture.bus).as_slice(),
            [EventPayload::ConfigRejected(event)] if event.revision == revision(edit.as_bytes())
        ));
    }

    #[tokio::test]
    async fn validate_has_no_side_effects() {
        let original = document("");
        let fixture = fixture(&original);
        let router = fixture.snapshot.load();
        let candidate = original.replacen("listen", "token_file = \"other.token\"\nlisten", 1);
        let valid = fixture.ops.validate(&candidate).await;
        assert!(valid.valid, "{:?}", valid.issues);
        assert_eq!(valid.pending_restart, ["token_file"]);
        assert_eq!(fixture.ops.live().generation, 1);
        assert!(Arc::ptr_eq(&router, &fixture.snapshot.load()));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).expect("read"),
            original
        );
        assert!(events(&fixture.bus).is_empty());
    }

    #[tokio::test]
    async fn pending_restart_clears_when_edit_is_reverted() {
        let original = document("");
        let fixture = fixture(&original);
        let edited = original.replacen(
            "listen",
            "token_file = \"other.token\"\nusage_file = \"other.db\"\nlisten",
            1,
        );
        let response = fixture
            .ops
            .apply(&revision(original.as_bytes()), edited.clone())
            .await
            .unwrap_or_else(|failure| panic!("{}: {:?}", "edit", failure.error));
        assert_eq!(response.pending_restart, ["token_file", "usage_file"]);
        assert_eq!(fixture.ops.live().pending_restart, response.pending_restart);
        let reverted = fixture
            .ops
            .apply(&revision(edited.as_bytes()), original.clone())
            .await
            .unwrap_or_else(|failure| panic!("{}: {:?}", "revert", failure.error));
        assert!(reverted.pending_restart.is_empty());
        assert!(fixture.ops.live().pending_restart.is_empty());
        assert_eq!(reverted.generation, 3);
    }

    #[test]
    fn listen_is_live_not_a_restart_setting() {
        let running = hofund_config::load("listen = \"127.0.0.1:0\"\n").expect("running");
        let moved = hofund_config::load("listen = \"127.0.0.1:1\"\n").expect("moved");
        assert!(restart_diff(&running, &moved).is_empty());
        let mut token = hofund_config::load("listen = \"127.0.0.1:0\"\n").expect("token");
        "other.token".clone_into(&mut token.token_file);
        assert_eq!(
            restart_diff(&running, &token),
            vec!["token_file".to_owned()]
        );
    }

    #[tokio::test]
    async fn env_override_listener_never_moves_and_reports_a_differing_listen() {
        let env = "127.0.0.1:4000".parse().expect("env");
        let listener = ModelListener::new(Some(env), None, tokio::sync::watch::channel(None).0);
        let same = hofund_config::load("listen = \"127.0.0.1:4000\"\n").expect("same");
        let other = hofund_config::load("listen = \"127.0.0.1:4001\"\n").expect("other");

        assert!(listener.follow(&same).await.is_none());
        let (issue, event) = listener.follow(&other).await.expect("issue");
        assert_eq!(
            (issue.field.as_str(), issue.reason.as_str()),
            ("listen", ENV_OVERRIDE)
        );
        assert_eq!(
            issue.message,
            "HOFUND_ADDR overrides the configured listener"
        );
        assert!(!event, "an override is not a relisten failure");
        let view = listener.view(&other);
        assert_eq!(view.effective, env);
        assert_eq!(view.configured, other.listen);
        assert_eq!(view.source, hofund_control_proto::ListenSource::Environment);
    }

    #[test]
    fn syntax_issue_drops_the_quoted_source_line() {
        let text = format!("api_key = \"{SECRET}\n");
        let error = hofund_config::load(&text).expect_err("syntax");
        let issue = super::config_issue(&error, &text);
        assert_eq!(issue.reason, "syntax");
        assert!(!issue.message.contains(SECRET), "{}", issue.message);
        assert!(issue.message.contains("line 1"), "{}", issue.message);
    }

    #[test]
    fn scrub_redacts_quoted_values_from_the_document() {
        let text = format!("api_key = \"{SECRET}\"\nid = \"short\"\n");
        let scrubbed = super::scrub(&format!("bad key {SECRET} for short"), &text);
        assert_eq!(scrubbed, "bad key [redacted] for short");
    }
}
