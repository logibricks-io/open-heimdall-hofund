//! Agent connect, disconnect, and token rotation for the control plane.
//!
//! Every adapter call reads or writes files under the user's home directory,
//! so it runs on the blocking pool. Connect, disconnect, and token rotation
//! hold one gate so a rotation cannot interleave with a connect that read
//! the previous token. The gate guard moves into the blocking task, so a
//! cancelled request keeps the gate until its file work has finished.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_agent::{AgentAdapter, Catalog, ConfigFile, GatewayTarget, Session};
use hofund_control_proto::{
    AgentAction, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentPlanRequest, AgentPlanResponse, AgentRefreshResult, AgentState,
    AgentStatusEntry, AgentStatusResponse, AgentSummary, ControlError, ControlErrorKind,
    FieldIssue, PlannedChange, TokenRotateResponse,
};
use hofund_plugin_api::AgentDescription;
use hofund_secrets::TokenFile;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};

/// Replaces bearer tokens in text bound for the wire or the logs.
const REDACTED: &str = "[redacted]";

/// The domain prefix of a plan revision. A later change to the hashed fields
/// changes this prefix.
const PLAN_REVISION_DOMAIN: &[u8] = b"hofund-agent-plan-v1\0";

/// Gateway facts an adapter writes, minus the token, which is read from the
/// running token file on each call.
#[derive(Clone)]
struct TargetParts {
    base_url: String,
    models: Vec<String>,
    token_path: PathBuf,
    config_path: PathBuf,
}

impl TargetParts {
    fn token(&self) -> Result<String, ControlError> {
        TokenFile::new(&self.token_path)
            .load_or_create()
            .map_err(|error| internal(format!("token file: {error}")))
    }

    fn target(&self, token: String) -> GatewayTarget {
        GatewayTarget::new(self.base_url.clone(), token, self.models.clone())
    }
}

pub(crate) struct AgentOps {
    descriptions: &'static [&'static AgentDescription],
    gate: Arc<Mutex<()>>,
    token_generation: watch::Sender<u64>,
    session: Option<Session>,
    parts: TargetParts,
}

impl AgentOps {
    pub(crate) fn new(
        descriptions: &'static [&'static AgentDescription],
        config_path: &Path,
        base_url: String,
        models: Vec<String>,
        token_path: PathBuf,
    ) -> Self {
        Self {
            descriptions,
            gate: Arc::new(Mutex::new(())),
            token_generation: watch::channel(0).0,
            session: None,
            parts: TargetParts {
                base_url,
                models,
                token_path,
                config_path: config_path.to_path_buf(),
            },
        }
    }

    /// Uses `session` for every adapter call instead of the process
    /// environment. Tests point it at a scratch home directory.
    pub(crate) fn with_session(mut self, session: Option<Session>) -> Self {
        self.session = session;
        self
    }

    /// Publishes token rotations on `sender`. L6 subscribes to close event
    /// streams that authenticated with the previous token.
    pub(crate) fn with_token_generation(mut self, sender: Option<watch::Sender<u64>>) -> Self {
        if let Some(sender) = sender {
            self.token_generation = sender;
        }
        self
    }

    /// Runs `work` on the blocking pool with a fresh catalog and session.
    async fn blocking<T, F>(&self, work: F) -> Result<T, ControlError>
    where
        T: Send + 'static,
        F: FnOnce(&Catalog, &Session) -> Result<T, ControlError> + Send + 'static,
    {
        let descriptions = self.descriptions;
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let catalog = hofund_agent::catalog(descriptions)
                .map_err(|error| internal(format!("agent catalog: {error}")))?;
            let session = session.unwrap_or_else(Session::from_process_env);
            work(&catalog, &session)
        })
        .await
        .map_err(|_| internal("agent task failed".to_owned()))?
    }

    /// Runs `work` like [`Self::blocking`] while holding the agent gate. The
    /// guard is owned by the blocking task, not by this future, so dropping
    /// the future (a client disconnect or a timeout) does not release the
    /// gate before `work` returns.
    async fn gated<T, F>(&self, work: F) -> Result<T, ControlError>
    where
        T: Send + 'static,
        F: FnOnce(&Catalog, &Session) -> Result<T, ControlError> + Send + 'static,
    {
        let guard = Arc::clone(&self.gate).lock_owned().await;
        self.blocking(move |catalog, session| {
            let _guard = guard;
            work(catalog, session)
        })
        .await
    }

    pub(crate) async fn list(&self) -> Result<AgentListResponse, ControlError> {
        self.blocking(|catalog, session| {
            let mut response = AgentListResponse::default();
            response.agents = catalog
                .adapters()
                .into_iter()
                .map(|adapter| {
                    let mut summary = AgentSummary::default();
                    summary.id = adapter.id().to_owned();
                    summary.display_name = adapter.display_name().to_owned();
                    summary.installed = adapter.detect(session);
                    summary
                })
                .collect();
            Ok(response)
        })
        .await
    }

    pub(crate) async fn status(&self) -> Result<AgentStatusResponse, ControlError> {
        let parts = self.parts.clone();
        self.blocking(move |catalog, session| {
            let token = parts.token()?;
            let target = parts.target(token.clone());
            let mut response = AgentStatusResponse::default();
            for adapter in catalog.adapters() {
                let status = adapter
                    .status(session, &target)
                    .map_err(|error| agent_failure(adapter, &error, &[&token]))?;
                let mut entry = AgentStatusEntry::default();
                entry.id = status.id().to_owned();
                entry.state = if status.connected() {
                    AgentState::Connected
                } else {
                    AgentState::Disconnected
                };
                entry.model = adapter
                    .configured_model(session)
                    .ok()
                    .flatten()
                    .map(|model| scrub(&model, &[&token]));
                entry.paths = status
                    .paths()
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                response.agents.push(entry);
            }
            Ok(response)
        })
        .await
    }

    pub(crate) async fn plan(
        &self,
        id: String,
        request: AgentPlanRequest,
    ) -> Result<AgentPlanResponse, ControlError> {
        let force = known_action(request.action)? == AgentAction::Disconnect && request.force;
        let parts = self.parts.clone();
        self.blocking(move |catalog, session| {
            let adapter = find(catalog, &id)?;
            let token = parts.token()?;
            let target = parts.target(token.clone());
            let files = read_files(adapter, &token, session)?;
            let mut response = AgentPlanResponse::default();
            response.agent = id.clone();
            response.action = request.action;
            response.plan_revision = plan_revision(&id, request.action, force, &target, &files);
            response.changes = match request.action {
                AgentAction::Disconnect => files
                    .iter()
                    .filter(|file| file.bytes.is_some())
                    .map(|file| planned(&file.path, String::new()))
                    .collect(),
                _ => adapter
                    .plan(session, &target)
                    .map_err(|error| agent_failure(adapter, &error, &[&token]))?
                    .changes()
                    .iter()
                    .map(|change| planned(change.path(), redact_diff(change.diff(), &token)))
                    .collect(),
            };
            Ok(response)
        })
        .await
    }

    pub(crate) async fn connect(
        &self,
        id: String,
        request: AgentConnectRequest,
    ) -> Result<AgentChangeResponse, ControlError> {
        let parts = self.parts.clone();
        self.gated(move |catalog, session| {
            let adapter = find(catalog, &id)?;
            let token = parts.token()?;
            let target = parts.target(token.clone());
            check_revision(
                adapter,
                session,
                &target,
                AgentAction::Connect,
                false,
                request.expected_plan_revision.as_deref(),
            )?;
            let plan = adapter
                .apply(session, &target)
                .map_err(|error| agent_failure(adapter, &error, &[&token]))?;
            let mut response = AgentChangeResponse::default();
            response.agent = id;
            response.action = AgentAction::Connect;
            response.changed_paths = plan
                .changes()
                .iter()
                .map(|change| change.path().display().to_string())
                .collect();
            response.warnings = unrouted_warning(adapter, session, &parts)
                .map(|warning| scrub(&warning, &[&token]))
                .into_iter()
                .collect();
            Ok(response)
        })
        .await
    }

    pub(crate) async fn disconnect(
        &self,
        id: String,
        request: AgentDisconnectRequest,
    ) -> Result<AgentChangeResponse, ControlError> {
        let parts = self.parts.clone();
        self.gated(move |catalog, session| {
            let adapter = find(catalog, &id)?;
            let token = parts.token()?;
            let target = parts.target(token.clone());
            check_revision(
                adapter,
                session,
                &target,
                AgentAction::Disconnect,
                request.force,
                request.expected_plan_revision.as_deref(),
            )?;
            let before = read_files(adapter, &token, session)?;
            let outcome = adapter
                .disconnect(session, request.force)
                .map_err(|error| agent_failure(adapter, &error, &[&token]))?;
            let after = read_files(adapter, &token, session)?;
            let mut response = AgentChangeResponse::default();
            response.agent = id;
            response.action = AgentAction::Disconnect;
            response.changed_paths = changed_paths(&before, &after);
            response.warnings = outcome
                .warnings()
                .iter()
                .map(|warning| scrub(warning, &[&token]))
                .collect();
            Ok(response)
        })
        .await
    }

    pub(crate) async fn rotate_token(&self) -> Result<TokenRotateResponse, ControlError> {
        let parts = self.parts.clone();
        let generation = self.token_generation.clone();
        self.gated(move |catalog, session| {
            let file = TokenFile::new(&parts.token_path);
            let previous_token = parts.token()?;
            let previous = parts.target(previous_token.clone());
            let mut results = Vec::new();
            let mut connected = Vec::new();
            for adapter in catalog.adapters() {
                match adapter.status(session, &previous) {
                    Ok(status) if status.connected() => connected.push(adapter),
                    Ok(_) => {}
                    Err(error) => {
                        let message = scrub(&error.to_string(), &[&previous_token]);
                        results.push(refresh_result(adapter.id(), Some(message)));
                    }
                }
            }
            let token = file.rotate().map_err(|error| {
                internal(scrub(
                    &format!("token rotation failed: {error}"),
                    &[&previous_token],
                ))
            })?;
            generation.send_modify(|value| *value = value.wrapping_add(1));
            let updated = parts.target(token.clone());
            for adapter in connected {
                let message = adapter
                    .refresh_token(session, &updated)
                    .err()
                    .map(|error| scrub(&error.to_string(), &[&previous_token, &token]));
                results.push(refresh_result(adapter.id(), message));
            }
            let order = |id: &str| {
                catalog
                    .adapters()
                    .iter()
                    .position(|adapter| adapter.id() == id)
            };
            results.sort_by_key(|result| order(&result.id));
            for result in results.iter().filter(|result| !result.ok) {
                tracing::warn!(
                    agent = %result.id,
                    message = result.message.as_deref().unwrap_or_default(),
                    "token rotated; agent still needs connect"
                );
            }
            let mut response = TokenRotateResponse::default();
            response.token = token;
            response.agents = results;
            Ok(response)
        })
        .await
    }
}

fn known_action(action: AgentAction) -> Result<AgentAction, ControlError> {
    match action {
        AgentAction::Connect | AgentAction::Disconnect => Ok(action),
        _ => {
            let message = "unknown agent action; expected connect or disconnect";
            let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "control");
            error
                .details
                .push(FieldIssue::new("action", "unknown_variant", message));
            Err(error)
        }
    }
}

fn find<'a>(catalog: &'a Catalog, id: &str) -> Result<&'a dyn AgentAdapter, ControlError> {
    catalog.find(id).ok_or_else(|| {
        ControlError::new(
            ControlErrorKind::NotFound,
            format!("unknown agent `{id}`"),
            "control",
        )
    })
}

fn read_files(
    adapter: &dyn AgentAdapter,
    token: &str,
    session: &Session,
) -> Result<Vec<ConfigFile>, ControlError> {
    adapter
        .read(session)
        .map_err(|error| agent_failure(adapter, &error, &[token]))
}

fn check_revision(
    adapter: &dyn AgentAdapter,
    session: &Session,
    target: &GatewayTarget,
    action: AgentAction,
    force: bool,
    expected: Option<&str>,
) -> Result<(), ControlError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let files = read_files(adapter, target.token(), session)?;
    let current = plan_revision(adapter.id(), action, force, target, &files);
    if current == expected {
        return Ok(());
    }
    let message = "the agent files changed since the plan; plan again";
    let mut error = ControlError::new(ControlErrorKind::Conflict, message, "control");
    error.details.push(FieldIssue::new(
        "expected_plan_revision",
        "stale_plan",
        message,
    ));
    Err(error)
}

/// Hex SHA-256 over the plan inputs. File bytes enter only as their own
/// SHA-256, and the token is never hashed on its own.
pub(crate) fn plan_revision(
    id: &str,
    action: AgentAction,
    force: bool,
    target: &GatewayTarget,
    files: &[ConfigFile],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PLAN_REVISION_DOMAIN);
    field(&mut hasher, id.as_bytes());
    let action = match action {
        AgentAction::Disconnect => "disconnect",
        _ => "connect",
    };
    field(&mut hasher, action.as_bytes());
    hasher.update([u8::from(force)]);
    field(&mut hasher, target.base_url().as_bytes());
    count(&mut hasher, target.models().len());
    for model in target.models() {
        field(&mut hasher, model.as_bytes());
    }
    count(&mut hasher, files.len());
    for file in files {
        field(&mut hasher, file.path.as_os_str().as_encoded_bytes());
        match &file.bytes {
            None => hasher.update([0]),
            Some(bytes) => {
                hasher.update([1]);
                hasher.update(Sha256::digest(bytes));
            }
        }
    }
    hex(&hasher.finalize())
}

fn field(hasher: &mut Sha256, bytes: &[u8]) {
    count(hasher, bytes.len());
    hasher.update(bytes);
}

fn count(hasher: &mut Sha256, length: usize) {
    hasher.update(u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn changed_paths(before: &[ConfigFile], after: &[ConfigFile]) -> Vec<String> {
    after
        .iter()
        .filter(|file| {
            before
                .iter()
                .find(|old| old.path == file.path)
                .is_none_or(|old| old.bytes != file.bytes)
        })
        .map(|file| file.path.display().to_string())
        .collect()
}

fn planned(path: &Path, diff: String) -> PlannedChange {
    let mut change = PlannedChange::default();
    change.path = path.display().to_string();
    change.diff = diff;
    change
}

fn refresh_result(id: &str, failure: Option<String>) -> AgentRefreshResult {
    let mut result = AgentRefreshResult::default();
    result.id = id.to_owned();
    result.ok = failure.is_none();
    result.message = failure;
    result
}

/// The warning `hofund agent connect` prints when the tool's model has no route.
fn unrouted_warning(
    adapter: &dyn AgentAdapter,
    session: &Session,
    parts: &TargetParts,
) -> Option<String> {
    let model = adapter.configured_model(session).ok().flatten()?;
    if model_has_route(adapter.model_prefix(), &model, &parts.models) {
        return None;
    }
    Some(format!(
        "{} model `{model}` has no route in {}",
        adapter.id(),
        parts.config_path.display()
    ))
}

fn model_has_route(model_prefix: Option<&str>, model: &str, routes: &[String]) -> bool {
    if let Some(prefix) = model_prefix {
        return model
            .strip_prefix(prefix)
            .is_some_and(|model| routes.iter().any(|route| route == model));
    }
    routes.iter().any(|route| route == model)
}

/// Replaces the bearer token in a diff, as `hofund agent connect --dry-run` does.
pub(crate) fn redact_diff(diff: &str, token: &str) -> String {
    scrub(diff, &[token])
}

/// Replaces every non-empty token in `text`.
pub(crate) fn scrub(text: &str, tokens: &[&str]) -> String {
    tokens
        .iter()
        .filter(|token| !token.is_empty())
        .fold(text.to_owned(), |text, token| text.replace(token, REDACTED))
}

fn agent_failure(
    adapter: &dyn AgentAdapter,
    error: &hofund_agent::AgentError,
    tokens: &[&str],
) -> ControlError {
    internal(scrub(&format!("{}: {error}", adapter.id()), tokens))
}

fn internal(message: String) -> ControlError {
    ControlError::new(ControlErrorKind::Internal, message, "control")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hofund_agent::{ConfigFile, GatewayTarget};
    use hofund_control_proto::AgentAction;

    use super::{AgentOps, model_has_route, plan_revision, redact_diff, scrub};

    fn target(token: &str) -> GatewayTarget {
        GatewayTarget::new("http://127.0.0.1:3425", token, vec!["m".to_owned()])
    }

    fn file(bytes: Option<&[u8]>) -> Vec<ConfigFile> {
        vec![ConfigFile {
            path: PathBuf::from("/home/a/.claude/settings.json"),
            bytes: bytes.map(<[u8]>::to_vec),
        }]
    }

    #[test]
    fn revision_ignores_the_token_and_tracks_every_input() {
        let base = plan_revision("a", AgentAction::Connect, false, &target("t1"), &file(None));
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            plan_revision("a", AgentAction::Connect, false, &target("t2"), &file(None))
        );
        let variants = [
            plan_revision("b", AgentAction::Connect, false, &target("t1"), &file(None)),
            plan_revision(
                "a",
                AgentAction::Disconnect,
                false,
                &target("t1"),
                &file(None),
            ),
            plan_revision("a", AgentAction::Connect, true, &target("t1"), &file(None)),
            plan_revision(
                "a",
                AgentAction::Connect,
                false,
                &GatewayTarget::new("http://127.0.0.1:1", "t1", vec!["m".to_owned()]),
                &file(None),
            ),
            plan_revision(
                "a",
                AgentAction::Connect,
                false,
                &GatewayTarget::new("http://127.0.0.1:3425", "t1", Vec::new()),
                &file(None),
            ),
            plan_revision(
                "a",
                AgentAction::Connect,
                false,
                &target("t1"),
                &file(Some(b"")),
            ),
            plan_revision(
                "a",
                AgentAction::Connect,
                false,
                &target("t1"),
                &file(Some(b"{}")),
            ),
            plan_revision("a", AgentAction::Connect, false, &target("t1"), &[]),
        ];
        for (index, variant) in variants.iter().enumerate() {
            assert_ne!(&base, variant, "variant {index} must change the revision");
        }
        assert_ne!(variants[5], variants[6]);
    }

    #[test]
    fn scrub_replaces_each_nonempty_token() {
        assert_eq!(
            redact_diff("a tok b tok", "tok"),
            "a [redacted] b [redacted]"
        );
        assert_eq!(redact_diff("unchanged", ""), "unchanged");
        assert_eq!(
            scrub("old new", &["old", "", "new"]),
            "[redacted] [redacted]"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_gated_call_keeps_the_gate_until_its_work_finishes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let ops = Arc::new(AgentOps::new(
            hofund_plugin_registry::agents(),
            std::path::Path::new("unused.toml"),
            "http://127.0.0.1:3425".to_owned(),
            Vec::new(),
            PathBuf::from("unused-token"),
        ));
        let finished = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let first = {
            let ops = Arc::clone(&ops);
            let finished = Arc::clone(&finished);
            tokio::spawn(async move {
                ops.gated(move |_, _| {
                    started_tx.send(()).ok();
                    std::thread::sleep(Duration::from_millis(300));
                    finished.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
            })
        };
        tokio::task::spawn_blocking(move || started_rx.recv())
            .await
            .expect("join")
            .expect("first body started");
        first.abort();
        assert!(first.await.expect_err("aborted").is_cancelled());
        let seen = {
            let finished = Arc::clone(&finished);
            ops.gated(move |_, _| Ok(finished.load(Ordering::SeqCst)))
                .await
                .expect("second gated call")
        };
        assert!(seen, "the second gated call ran before the first body finished");
    }

    #[test]
    fn route_match_honors_the_model_prefix() {
        let routes = ["m".to_owned()];
        assert!(model_has_route(None, "m", &routes));
        assert!(!model_has_route(None, "x", &routes));
        assert!(model_has_route(Some("hofund/"), "hofund/m", &routes));
        assert!(!model_has_route(Some("hofund/"), "m", &routes));
    }
}
