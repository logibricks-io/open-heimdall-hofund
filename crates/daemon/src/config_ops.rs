//! The configuration transaction behind the control plane's config routes.
//!
//! Every write and reload runs under one in-process gate and the sidecar file
//! lock. Steps that can fail (revision check, validation, provider
//! construction, the write itself) all run before anything is published, so a
//! failure leaves the file, the router snapshot, the generation and the
//! cooldowns unchanged.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hofund_config::{Config, ConfigError};
use hofund_control_proto::{
    ConfigRejectedEvent, ConfigReloadedEvent, ConfigResponse, ConfigValidationResponse,
    ControlError, ControlErrorKind, EventPayload, FieldIssue,
};
use hofund_gateway::control::ControlFailure;

use crate::config_write::{SidecarLock, WriteFailure, atomic_write, primary_lock_path};
use crate::control::revision;
use crate::events::EventBus;
use crate::runtime::Runtime;

/// Reason code for a changed `listen` before live relisten exists.
pub(crate) const PENDING_L6D: &str = "pending_l6d";

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
}

impl ConfigOps {
    pub(crate) fn new(
        path: &Path,
        lock_fallback: Option<PathBuf>,
        runtime: Arc<Runtime>,
        bus: Arc<EventBus>,
        config: Arc<Config>,
        runtime_revision: String,
    ) -> Self {
        let live = LiveConfig {
            config: Arc::clone(&config),
            runtime_revision,
            generation: 1,
            pending_restart: Vec::new(),
            not_applied: Vec::new(),
        };
        Self {
            gate: tokio::sync::Mutex::new(()),
            path: path.to_path_buf(),
            lock_fallback,
            runtime,
            bus,
            started: config,
            live: RwLock::new(Arc::new(live)),
        }
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
                let (pending, _) = restart_diff(&self.started, &config);
                response.pending_restart = pending;
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
        let _gate = self.gate.lock().await;
        let ops = Arc::clone(self);
        let expected = expected.to_owned();
        run_blocking(move || ops.apply_locked(&expected, text)).await
    }

    pub(crate) async fn reload(
        self: &Arc<Self>,
        expected: Option<&str>,
        origin: ReloadOrigin,
    ) -> Result<ConfigResponse, ControlFailure> {
        let _gate = self.gate.lock().await;
        let ops = Arc::clone(self);
        let expected = expected.map(str::to_owned);
        run_blocking(move || ops.reload_locked(expected.as_deref(), origin)).await
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
        let candidate = self.runtime.prepare(&config).map_err(|error| {
            vec![FieldIssue::new(
                "providers",
                "build_failed",
                scrub(&error.to_string(), text),
            )]
        })?;
        Ok((config, candidate))
    }

    fn apply_locked(&self, expected: &str, text: String) -> Result<ConfigResponse, ControlFailure> {
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
        let live = self.publish(candidate, written.clone())?;
        Ok(response(&live, written, text))
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
    ) -> Result<ConfigResponse, ControlFailure> {
        let _lock = self.lock()?;
        let (disk_revision, text) = self.read_disk()?;
        if expected.is_some_and(|expected| expected != disk_revision) {
            return Err(stale_revision());
        }
        let current = self.live();
        if origin == ReloadOrigin::Watcher && current.runtime_revision == disk_revision {
            return Ok(response(&current, disk_revision, text));
        }
        let (_, candidate) = match self.check(&text) {
            Ok(checked) => checked,
            Err(issues) => {
                self.reject(&disk_revision, issues.clone());
                return Err(invalid(issues));
            }
        };
        let live = self.publish(candidate, disk_revision.clone())?;
        Ok(response(&live, disk_revision, text))
    }

    fn publish(
        &self,
        candidate: crate::runtime::Candidate,
        runtime_revision: String,
    ) -> Result<Arc<LiveConfig>, ControlFailure> {
        let config = Arc::clone(candidate.config());
        let changed = self
            .runtime
            .publish(candidate)
            .map_err(|error| internal(format!("publication failed: {error}")))?;
        let (pending_restart, not_applied) = restart_diff(&self.started, &config);
        let mut live = self.live.write().expect("live config");
        let next = Arc::new(LiveConfig {
            config,
            runtime_revision,
            generation: live.generation + 1,
            pending_restart,
            not_applied,
        });
        *live = Arc::clone(&next);
        drop(live);
        let mut event = ConfigReloadedEvent::default();
        event.disk_revision.clone_from(&next.runtime_revision);
        event.runtime_revision.clone_from(&next.runtime_revision);
        event.generation = next.generation;
        event.changed_members = changed;
        event.pending_restart.clone_from(&next.pending_restart);
        event.not_applied.clone_from(&next.not_applied);
        self.bus.emit(EventPayload::ConfigReloaded(event));
        Ok(next)
    }
}

/// Settings that only take effect at startup, and live settings that cannot be
/// applied yet, compared against the configuration the process started with.
pub(crate) fn restart_diff(running: &Config, candidate: &Config) -> (Vec<String>, Vec<FieldIssue>) {
    let mut pending = Vec::new();
    if running.token_file != candidate.token_file {
        pending.push("token_file".to_owned());
    }
    if running.usage_file != candidate.usage_file {
        pending.push("usage_file".to_owned());
    }
    let mut not_applied = Vec::new();
    if running.listen != candidate.listen {
        not_applied.push(FieldIssue::new(
            "listen",
            PENDING_L6D,
            "the model listener moves to a new address after a restart",
        ));
    }
    (pending, not_applied)
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
    use std::sync::Arc;

    use hofund_control_proto::{ControlErrorKind, EventEnvelope, EventPayload};
    use hofund_gateway::control::ControlFailure;
    use hofund_plugin_api::PluginError;
    use hofund_router::Snapshot;

    use super::{ConfigOps, ReloadOrigin, restart_diff};
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
        let runtime = Runtime::new(Arc::new(RegistryAdapter), &path, None);
        let snapshot = runtime.initial_snapshot(&config, None).expect("snapshot");
        let bus = Arc::new(EventBus::new());
        let ops = Arc::new(ConfigOps::new(
            &path,
            Some(runtime_dir.path().join("config-test.lock")),
            Arc::clone(&runtime),
            Arc::clone(&bus),
            Arc::new(config),
            revision(text.as_bytes()),
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
            [EventPayload::ConfigReloaded(event)] => {
                assert_eq!(event.generation, 2);
                assert_eq!(event.changed_members, [member_id("second").to_string()]);
                assert_eq!(event.runtime_revision, response.runtime_revision);
            }
            other => panic!("expected config_reloaded, got {other:?}"),
        }
        assert!(!format!("{:?}", fixture.bus.control_frames()).contains(SECRET));
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
    fn listen_change_is_reported_not_applied_until_relisten_exists() {
        let running = hofund_config::load("listen = \"127.0.0.1:0\"\n").expect("running");
        let moved = hofund_config::load("listen = \"127.0.0.1:1\"\n").expect("moved");
        let (pending, not_applied) = restart_diff(&running, &moved);
        assert!(pending.is_empty());
        assert_eq!(not_applied.len(), 1);
        assert_eq!(not_applied[0].field, "listen");
        assert_eq!(not_applied[0].reason, super::PENDING_L6D);
        assert_eq!(restart_diff(&running, &running), (Vec::new(), Vec::new()));
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
