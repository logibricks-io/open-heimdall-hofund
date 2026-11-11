//! Session control routes backed by an offline fake reader.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Local;
use hofund_control_proto::{
    API_VERSION, ControlErrorKind, SessionListResponse, SessionOverviewResponse,
    SessionProgressResponse, SessionStatsResponse, decode_error_envelope, decode_response,
};
use hofund_daemon::{
    CompiledPreset, ControlRuntime, DaemonRegistry, Running, StartOptions, start_linked,
};
use hofund_plugin_api::{
    AgentDescription, DynUpstreamFactory, InboundSurface, PluginError, ProtocolId, SessionFile,
    SessionLocator, SessionParse, SessionParser, SessionRecord, SessionStore, SessionTokens,
};
use hofund_sessions::consent::Consent;

struct FakeRegistry {
    stores: Vec<Arc<dyn SessionStore>>,
}

impl DaemonRegistry for FakeRegistry {
    fn factory(&self, _id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        None
    }

    fn is_kernel_factory(&self, _id: &str) -> bool {
        false
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        &[]
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        Vec::new()
    }

    fn find_preset(&self, _id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
        None
    }

    fn agents(&self) -> &'static [&'static AgentDescription] {
        &[]
    }

    fn surfaces(&self) -> Vec<Arc<dyn InboundSurface>> {
        Vec::new()
    }

    fn session_stores(&self) -> Vec<Arc<dyn SessionStore>> {
        self.stores.clone()
    }
}

struct FakeLocator {
    root: PathBuf,
}

impl SessionLocator for FakeLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.is_dir()
    }
}

struct FakeStore {
    locator: FakeLocator,
    last_ms: i64,
    date: String,
}

struct FakeParser {
    record: SessionRecord,
    last_ms: i64,
    date: String,
}

impl SessionParser for FakeParser {
    fn push(&mut self, _value: &serde_json::Value) -> Result<(), PluginError> {
        self.record.title = "Offline fixture".to_owned();
        self.record.cwd = "/work/project".to_owned();
        self.record.last_ms = self.last_ms;
        self.record
            .add_usage("model", SessionTokens::new(4, 3, 2, 1));
        self.record.add_day(hofund_plugin_api::SessionDay::new(
            self.date.clone(),
            "model",
            SessionTokens::new(4, 3, 2, 1),
        ));
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        Ok(SessionParse::new(self.record, "{}"))
    }
}

impl SessionStore for FakeStore {
    fn agent(&self) -> &str {
        "fake"
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        Ok(vec![SessionFile::new("one", root.join("one.jsonl"))])
    }

    fn parser(
        &self,
        file: &SessionFile,
        _state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        Ok(Box::new(FakeParser {
            record: SessionRecord::new("fake", "fake:one", &file.path),
            last_ms: self.last_ms,
            date: self.date.clone(),
        }))
    }
}

async fn start(dir: &Path) -> Running {
    let now = Local::now();
    let agent = dir.join("agent");
    std::fs::create_dir(&agent).expect("agent root");
    std::fs::write(agent.join("one.jsonl"), b"{\"fixture\":true}\n").expect("session fixture");
    let store = FakeStore {
        locator: FakeLocator { root: agent },
        last_ms: now.timestamp_millis(),
        date: now.date_naive().to_string(),
    };
    let registry = Arc::new(FakeRegistry {
        stores: vec![Arc::new(store)],
    });
    let config = dir.join("hofund.toml");
    std::fs::write(
        &config,
        "listen = \"127.0.0.1:0\"\n[usage.prices.\"*/model\"]\ninput = 1.0\noutput = 1.0\ncache_read = 1.0\ncache_write = 1.0\n",
    )
    .expect("config");
    let mut consent = Consent::new(dir);
    consent.settle();
    consent.set_reads(Some(true), []).expect("consent");
    start_linked(
        &config,
        registry,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

async fn get(running: &Running, dir: &Path, path: &str) -> reqwest::Response {
    let token = std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned();
    support::client()
        .get(format!(
            "http://{}{path}",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token)
        .send()
        .await
        .expect("control response")
}

#[tokio::test]
async fn list_stats_overview_and_progress_use_the_fake_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;

    let response = get(&running, dir.path(), "/v1/hofund/sessions").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let list: SessionListResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("list body")).expect("list");
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.sessions[0].id, "fake:one");
    assert_eq!(list.sessions[0].title, "Offline fixture");
    assert_eq!(list.sessions[0].resume, None);
    assert!(list.consent.asked && list.consent.all);
    assert_eq!(list.revision.len(), 64);

    let response = get(&running, dir.path(), "/v1/hofund/sessions/stats?days=1").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let stats: SessionStatsResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("stats body")).expect("stats");
    assert_eq!(stats.tokens.input, 4);
    assert_eq!(stats.tokens.output, 3);
    assert_eq!(stats.days.len(), 1);
    assert_eq!(stats.models[0].name, "model");
    assert_eq!(stats.folders[0].name, "/work/project");
    assert!(stats.cost > 0.0);

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/overview?days=1&agent=fake&model=model&cwd=%2Fwork%2Fproject",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let overview: SessionOverviewResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("overview body"))
            .expect("overview");
    assert_eq!(overview.count, 1);
    assert_eq!(overview.by_day.len(), 1);
    assert_eq!(overview.by_day[0].count, 1);
    assert_eq!(overview.top[0].id, "fake:one");
    assert!(overview.median_cost > 0.0);

    let response = get(&running, dir.path(), "/v1/hofund/sessions/progress").await;
    let progress: SessionProgressResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("progress body"))
            .expect("progress");
    assert_eq!(progress.files_total, 1);
    assert_eq!(progress.files_done, 1);
    assert!(!progress.running);

    support::shutdown(running).await;
}

#[tokio::test]
async fn bad_days_return_a_control_error_without_reading_the_index() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;
    let response = get(&running, dir.path(), "/v1/hofund/sessions?days=2").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let error = decode_error_envelope(API_VERSION, &response.bytes().await.expect("error body"))
        .expect("error");
    assert_eq!(error.error.kind, ControlErrorKind::BadRequest);
    support::shutdown(running).await;
}
