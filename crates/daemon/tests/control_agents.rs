//! Agent connect, disconnect, and token rotation over the control plane.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, AgentAction, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentPlanRequest, AgentPlanResponse, AgentState, AgentStatusResponse,
    ControlErrorKind, TokenRotateRequest, TokenRotateResponse, decode_error_envelope,
    decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use reqwest::StatusCode;
use serde::Serialize;
use tokio::sync::watch;

const CONFIG: &str = "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

struct Scratch(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("hofund-daemon-agents-{label}-"))
            .tempdir()
            .expect("scratch dir");
        let path = dir.path().to_path_buf();
        std::fs::create_dir_all(path.join("home")).expect("scratch home");
        Self(path, dir)
    }
}

struct Daemon {
    scratch: Scratch,
    running: Running,
    generation: watch::Receiver<u64>,
    client: reqwest::Client,
}

impl Daemon {
    async fn start(label: &str) -> Self {
        let scratch = Scratch::new(label);
        let config = scratch.0.join("hofund.toml");
        std::fs::write(&config, CONFIG).expect("config");
        let (sender, generation) = watch::channel(0);
        let session = Session::new(scratch.0.join("home")).with_path("");
        let running = start_with_options(
            &config,
            StartOptions {
                control_listen: Some("127.0.0.1:0".parse().expect("control address")),
                control_runtime: Some(ControlRuntime::new(None, scratch.0.clone())),
                agent_session: Some(session),
                token_generation: Some(sender),
                ..StartOptions::default()
            },
        )
        .await
        .expect("daemon");
        Self {
            scratch,
            running,
            generation,
            client: reqwest::Client::new(),
        }
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.scratch.0.join("hofund.token"))
            .expect("token")
            .trim()
            .to_owned()
    }

    fn settings(&self) -> PathBuf {
        self.scratch.0.join("home/.claude/settings.json")
    }

    fn write_settings(&self, text: &str) {
        let path = self.settings();
        std::fs::create_dir_all(path.parent().expect("settings parent")).expect("claude dir");
        std::fs::write(path, text).expect("settings");
    }

    fn read_settings(&self) -> String {
        std::fs::read_to_string(self.settings()).expect("settings")
    }

    fn url(&self, path: &str) -> String {
        format!(
            "http://{}/v1/hofund/{path}",
            self.running.control_local_addr().expect("control")
        )
    }

    async fn get(&self, path: &str, token: &str) -> (StatusCode, Vec<u8>) {
        let response = self
            .client
            .get(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .expect("control GET");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn post(&self, path: &str, token: &str, body: &impl Serialize) -> (StatusCode, Vec<u8>) {
        let response = self
            .client
            .post(self.url(path))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .expect("control POST");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn plan(&self, id: &str, action: AgentAction, force: bool) -> AgentPlanResponse {
        let (status, body) = self
            .post(
                &format!("agents/{id}/plan"),
                &self.token(),
                &AgentPlanRequest::new(action, force),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        decode_response(API_VERSION, &body).expect("plan")
    }

    async fn connect(&self, id: &str, revision: Option<String>) -> (StatusCode, Vec<u8>) {
        self.post(
            &format!("agents/{id}/connect"),
            &self.token(),
            &AgentConnectRequest::new(revision),
        )
        .await
    }

    async fn status(&self) -> AgentStatusResponse {
        let (status, body) = self.get("agents/status", &self.token()).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        decode_response(API_VERSION, &body).expect("agent status")
    }

    async fn rotate(&self, token: &str) -> (StatusCode, Vec<u8>) {
        self.post("token/rotate", token, &TokenRotateRequest::new())
            .await
    }
}

fn state_of(status: &AgentStatusResponse, id: &str) -> AgentState {
    status
        .agents
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.state)
        .expect("agent status row")
}

fn error_kind(body: &[u8]) -> (ControlErrorKind, Vec<String>, String) {
    let envelope = decode_error_envelope(API_VERSION, body).expect("error envelope");
    let codes = envelope
        .error
        .details
        .iter()
        .map(|issue| issue.reason.clone())
        .collect();
    (envelope.error.kind, codes, envelope.error.message)
}

#[tokio::test]
async fn agent_list_reports_catalog_and_detection() {
    let daemon = Daemon::start("list").await;
    daemon.write_settings("{}\n");
    let (status, body) = daemon.get("agents", &daemon.token()).await;
    assert_eq!(status, StatusCode::OK);
    let list: AgentListResponse = decode_response(API_VERSION, &body).expect("agent list");
    let ids: Vec<&str> = list.agents.iter().map(|agent| agent.id.as_str()).collect();
    assert!(ids.starts_with(&["claude-code", "codex", "gemini-cli", "opencode"]));
    assert_eq!(list.agents[0].display_name, "Claude Code");
    assert!(list.agents[0].installed, "a settings file means installed");
    assert!(!list.agents[1].installed, "no codex file and an empty PATH");
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn agent_status_reflects_connected_files() {
    let daemon = Daemon::start("status").await;
    assert_eq!(
        state_of(&daemon.status().await, "claude-code"),
        AgentState::Disconnected
    );
    let (status, body) = daemon.connect("claude-code", None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let change: AgentChangeResponse = decode_response(API_VERSION, &body).expect("connect");
    assert_eq!(change.action, AgentAction::Connect);
    assert_eq!(
        change.changed_paths,
        [daemon.settings().display().to_string()]
    );
    let after = daemon.status().await;
    assert_eq!(state_of(&after, "claude-code"), AgentState::Connected);
    assert_eq!(state_of(&after, "codex"), AgentState::Disconnected);
    let row = after
        .agents
        .iter()
        .find(|entry| entry.id == "claude-code")
        .expect("claude row");
    assert_eq!(row.model.as_deref(), Some("m"));
    assert!(row.paths.contains(&daemon.settings().display().to_string()));
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn agent_plan_redacts_token_in_diff() {
    let daemon = Daemon::start("plan").await;
    daemon.write_settings("{\"theme\": \"dark\"}\n");
    let plan = daemon
        .plan("claude-code", AgentAction::Connect, false)
        .await;
    assert_eq!(plan.agent, "claude-code");
    assert_eq!(plan.plan_revision.len(), 64);
    assert_eq!(plan.changes.len(), 1);
    let diff = &plan.changes[0].diff;
    assert!(diff.contains("[redacted]"), "{diff}");
    assert!(!diff.contains(&daemon.token()));
    assert_eq!(
        daemon.read_settings(),
        "{\"theme\": \"dark\"}\n",
        "plan writes nothing"
    );
    let disconnect = daemon
        .plan("claude-code", AgentAction::Disconnect, true)
        .await;
    assert_eq!(disconnect.changes.len(), 1);
    assert!(disconnect.changes[0].diff.is_empty());
    assert_ne!(disconnect.plan_revision, plan.plan_revision);
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn agent_plan_revision_prevents_stale_connect() {
    let daemon = Daemon::start("stale").await;
    daemon.write_settings("{}\n");
    let plan = daemon
        .plan("claude-code", AgentAction::Connect, false)
        .await;
    daemon.write_settings("{\"theme\": \"light\"}\n");
    let (status, body) = daemon
        .connect("claude-code", Some(plan.plan_revision.clone()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (kind, codes, _) = error_kind(&body);
    assert_eq!(kind, ControlErrorKind::Conflict);
    assert_eq!(codes, ["stale_plan"]);
    assert_eq!(daemon.read_settings(), "{\"theme\": \"light\"}\n");
    let fresh = daemon
        .plan("claude-code", AgentAction::Connect, false)
        .await;
    let (status, _) = daemon
        .connect("claude-code", Some(fresh.plan_revision))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state_of(&daemon.status().await, "claude-code"),
        AgentState::Connected
    );
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn agent_disconnect_restores_preconnect_bytes() {
    let daemon = Daemon::start("disconnect").await;
    let original = "{\"theme\": \"dark\"}\n";
    daemon.write_settings(original);
    let (status, _) = daemon.connect("claude-code", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(daemon.read_settings(), original);
    let plan = daemon
        .plan("claude-code", AgentAction::Disconnect, false)
        .await;
    let (status, body) = daemon
        .post(
            "agents/claude-code/disconnect",
            &daemon.token(),
            &AgentDisconnectRequest::new(false, Some(plan.plan_revision)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let change: AgentChangeResponse = decode_response(API_VERSION, &body).expect("disconnect");
    assert_eq!(change.action, AgentAction::Disconnect);
    assert_eq!(
        change.changed_paths,
        [daemon.settings().display().to_string()]
    );
    assert_eq!(daemon.read_settings(), original);
    let (status, body) = daemon
        .post(
            "agents/claude-code/disconnect",
            &daemon.token(),
            &AgentDisconnectRequest::new(false, None),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let again: AgentChangeResponse = decode_response(API_VERSION, &body).expect("second");
    assert!(again.changed_paths.is_empty());
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn unknown_agent_404() {
    let daemon = Daemon::start("unknown").await;
    let token = daemon.token();
    for (path, body) in [
        (
            "agents/nope/plan",
            serde_json::to_value(AgentPlanRequest::new(AgentAction::Connect, false)),
        ),
        (
            "agents/nope/connect",
            serde_json::to_value(AgentConnectRequest::new(None)),
        ),
        (
            "agents/nope/disconnect",
            serde_json::to_value(AgentDisconnectRequest::new(false, None)),
        ),
    ] {
        let (status, response) = daemon.post(path, &token, &body.expect("body")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (kind, _, message) = error_kind(&response);
        assert_eq!(kind, ControlErrorKind::NotFound);
        assert!(message.contains("nope"));
    }
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn token_rotate_updates_connected_agents_and_old_token_fails() {
    let daemon = Daemon::start("rotate").await;
    let (status, _) = daemon.connect("claude-code", None).await;
    assert_eq!(status, StatusCode::OK);
    let old = daemon.token();
    let (status, body) = daemon.rotate(&old).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let rotated: TokenRotateResponse = decode_response(API_VERSION, &body).expect("rotate");
    assert_ne!(rotated.token, old);
    assert_eq!(rotated.token, daemon.token());
    assert_eq!(rotated.agents.len(), 1);
    assert_eq!(rotated.agents[0].id, "claude-code");
    assert!(rotated.agents[0].ok);
    let settings = daemon.read_settings();
    assert!(settings.contains(&rotated.token));
    assert!(!settings.contains(&old));
    assert_eq!(
        state_of(&daemon.status().await, "claude-code"),
        AgentState::Connected
    );
    let (status, _) = daemon.get("status", &old).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old token on the control route"
    );
    let model = daemon
        .client
        .get(format!("http://{}/v1/models", daemon.running.local_addr))
        .bearer_auth(&old)
        .send()
        .await
        .expect("model route");
    assert_eq!(
        model.status(),
        StatusCode::UNAUTHORIZED,
        "old token on the model route"
    );
    let (status, _) = daemon.get("status", &rotated.token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(*daemon.generation.borrow(), 1);
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn token_rotate_bumps_generation_once_and_not_on_failure() {
    let daemon = Daemon::start("generation").await;
    let (status, _) = daemon.rotate(&daemon.token()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(*daemon.generation.borrow(), 1);
    let blocker = daemon.scratch.0.join("hofund.token.tmp");
    std::fs::create_dir_all(&blocker).expect("blocking directory");
    let before = daemon.token();
    let (status, body) = daemon.rotate(&before).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (kind, _, message) = error_kind(&body);
    assert_eq!(kind, ControlErrorKind::Internal);
    assert!(!message.contains(&before));
    assert_eq!(daemon.token(), before, "a failed rotation keeps the token");
    assert_eq!(*daemon.generation.borrow(), 1, "no bump on failure");
    std::fs::remove_dir(&blocker).expect("unblock");
    let (status, _) = daemon.rotate(&before).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(*daemon.generation.borrow(), 2);
    daemon.running.shutdown().await;
}

/// Every formatted tracing event in this test binary.
fn captured() -> &'static Mutex<String> {
    static LOG: OnceLock<Mutex<String>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(String::new()))
}

struct Capture;

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = String::new();
        span.record(&mut Fields(&mut line));
        captured().lock().expect("log").push_str(&line);
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::new();
        values.record(&mut Fields(&mut line));
        captured().lock().expect("log").push_str(&line);
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        line.push('\n');
        captured().lock().expect("log").push_str(&line);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn token_never_in_logs_or_errors() {
    let _ = tracing::subscriber::set_global_default(Capture);
    let daemon = Daemon::start("secrets").await;
    let (status, _) = daemon.connect("claude-code", None).await;
    assert_eq!(status, StatusCode::OK);
    let old = daemon.token();
    // A truncated file that still carries the token fails to parse.
    daemon.write_settings(&format!("{{\"env\": {{\"ANTHROPIC_AUTH_TOKEN\": \"{old}\""));
    let mut bodies = Vec::new();
    let (status, body) = daemon.get("agents/status", &old).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    bodies.push(body);
    let (status, body) = daemon
        .post(
            "agents/claude-code/plan",
            &old,
            &AgentPlanRequest::new(AgentAction::Connect, false),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    bodies.push(body);
    let (status, body) = daemon.rotate(&old).await;
    assert_eq!(status, StatusCode::OK);
    let rotated: TokenRotateResponse = decode_response(API_VERSION, &body).expect("rotate");
    let failure = rotated
        .agents
        .iter()
        .find(|agent| agent.id == "claude-code")
        .expect("claude result");
    assert!(!failure.ok);
    let message = failure.message.clone().unwrap_or_default();
    assert!(!message.is_empty());
    for body in &bodies {
        let text = String::from_utf8_lossy(body);
        assert!(!text.contains(&old), "{text}");
    }
    assert!(
        !message.contains(&old) && !message.contains(&rotated.token),
        "{message}"
    );
    assert!(!format!("{rotated:?}").contains(&rotated.token));
    let log = captured().lock().expect("log").clone();
    assert!(log.contains("claude-code"), "the refresh failure is logged");
    assert!(!log.contains(&old), "{log}");
    assert!(!log.contains(&rotated.token), "{log}");
    daemon.running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rotate_and_connect_serialize_on_gate() {
    let daemon = Daemon::start("gate").await;
    for round in 0..6 {
        let token = daemon.token();
        let connect = async {
            let (status, body) = daemon
                .post(
                    "agents/claude-code/connect",
                    &token,
                    &AgentConnectRequest::new(None),
                )
                .await;
            if status == StatusCode::UNAUTHORIZED {
                // The rotation finished before this request authenticated.
                return daemon.connect("claude-code", None).await;
            }
            (status, body)
        };
        let ((connect_status, _), (rotate_status, _)) =
            tokio::join!(connect, daemon.rotate(&token));
        assert_eq!(connect_status, StatusCode::OK, "round {round}");
        assert_eq!(rotate_status, StatusCode::OK, "round {round}");
        assert!(
            daemon.read_settings().contains(&daemon.token()),
            "round {round}: the agent holds the current token"
        );
        assert_eq!(
            state_of(&daemon.status().await, "claude-code"),
            AgentState::Connected
        );
        let (status, _) = daemon
            .post(
                "agents/claude-code/disconnect",
                &daemon.token(),
                &AgentDisconnectRequest::new(true, None),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(*daemon.generation.borrow(), 6);
    daemon.running.shutdown().await;
}

#[tokio::test]
async fn token_rotate_scrubs_pre_rotation_status_errors() {
    let daemon = Daemon::start("status-error").await;
    let old = daemon.token();
    // A malformed TOML file whose parse error quotes the line holding the token.
    let codex = daemon.scratch.0.join("home/.codex/config.toml");
    std::fs::create_dir_all(codex.parent().expect("codex parent")).expect("codex dir");
    std::fs::write(&codex, format!("token = \"{old}\" trailing\n")).expect("codex config");
    let (status, body) = daemon.rotate(&old).await;
    assert_eq!(status, StatusCode::OK);
    let rotated: TokenRotateResponse = decode_response(API_VERSION, &body).expect("rotate");
    let failure = rotated
        .agents
        .iter()
        .find(|agent| agent.id == "codex")
        .expect("codex result");
    assert!(!failure.ok);
    let message = failure.message.clone().unwrap_or_default();
    assert!(message.contains("[redacted]"), "{message}");
    assert!(!message.contains(&old), "{message}");
    assert!(!String::from_utf8_lossy(&body).contains(&old));
    daemon.running.shutdown().await;
}
