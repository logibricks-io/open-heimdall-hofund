//! Session control routes backed by an offline fake reader.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Local;
use hofund_control_proto::{
    API_VERSION, ControlErrorKind, SessionDetailResponse, SessionListResponse,
    SessionManageResponse, SessionOverviewResponse, SessionProgressResponse, SessionReadsResponse,
    SessionSearchResponse, SessionStatsResponse, SessionTranscriptResponse, decode_error_envelope,
    decode_response,
};
use hofund_daemon::{
    CompiledPreset, ControlRuntime, DaemonRegistry, Running, StartOptions, start_linked,
};
use hofund_plugin_api::{
    AgentDescription, DynUpstreamFactory, InboundSurface, PluginError, ProtocolId, SessionFile,
    SessionLocator, SessionParse, SessionParser, SessionPart, SessionRecord, SessionStore,
    SessionTokens, SessionTranscript, SessionTranscriptParser,
};
use hofund_sessions::consent::Consent;
use hofund_sessions::resume::safe_session_id;

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
    agent: String,
    locator: FakeLocator,
    last_ms: i64,
    days: Vec<hofund_plugin_api::SessionDay>,
    count: usize,
    child: bool,
}

struct FakeParser {
    record: SessionRecord,
    last_ms: i64,
    days: Vec<hofund_plugin_api::SessionDay>,
}

impl SessionParser for FakeParser {
    fn push(&mut self, _value: &serde_json::Value) -> Result<(), PluginError> {
        self.record.title = "Offline fixture".to_owned();
        self.record.cwd = "/work/project".to_owned();
        self.record.last_ms = self.last_ms;
        for day in &self.days {
            self.record.add_usage(day.model.clone(), day.tokens);
            self.record.add_day(day.clone());
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        Ok(SessionParse::new(self.record, "{}"))
    }
}

impl SessionStore for FakeStore {
    fn agent(&self) -> &str {
        &self.agent
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        Ok((0..self.count)
            .map(|index| {
                let id = session_file_id(index, self.child);
                SessionFile::new(&id, root.join(format!("{id}.jsonl")))
            })
            .collect())
    }

    fn parser(
        &self,
        file: &SessionFile,
        _state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        let mut record = SessionRecord::new(
            &self.agent,
            format!("{}:{}", self.agent, file.id),
            &file.path,
        );
        if self.child && file.id == "child" {
            record = record.with_parent(format!("{}:one", self.agent));
        }
        Ok(Box::new(FakeParser {
            record,
            last_ms: self.last_ms,
            days: self.days.clone(),
        }))
    }

    fn transcript_available(&self) -> bool {
        true
    }

    fn transcript_parser(
        &self,
        file: &SessionFile,
    ) -> Result<Box<dyn SessionTranscriptParser>, PluginError> {
        Ok(Box::new(FakeTranscriptParser {
            child: file.id == "child",
            transcript: SessionTranscript::default(),
        }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let bare = session_id.split_once(':')?.1;
        if !safe_session_id(bare) {
            return None;
        }
        Some(vec![
            "fake".to_owned(),
            "--resume".to_owned(),
            bare.to_owned(),
        ])
    }
}

struct FakeTranscriptParser {
    child: bool,
    transcript: SessionTranscript,
}

impl SessionTranscriptParser for FakeTranscriptParser {
    fn push(&mut self, _value: &serde_json::Value) -> Result<(), PluginError> {
        let (question, answer) = if self.child {
            ("Child question", "Child answer")
        } else {
            ("Offline question", "Offline answer")
        };
        self.transcript.parts = vec![
            SessionPart::new("user", "text", "", question),
            SessionPart::new("assistant", "text", "", answer),
        ];
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionTranscript, PluginError> {
        Ok(self.transcript)
    }
}

fn session_file_id(index: usize, child: bool) -> String {
    match (index, child) {
        (0, _) => "one".to_owned(),
        (1, true) => "child".to_owned(),
        _ => format!("session-{index}"),
    }
}

fn fake_store(dir: &Path, agent: &str, count: usize) -> FakeStore {
    let now = Local::now();
    let root = dir.join(agent);
    std::fs::create_dir(&root).expect("agent root");
    for index in 0..count {
        let id = session_file_id(index, false);
        std::fs::write(root.join(format!("{id}.jsonl")), b"{\"fixture\":true}\n")
            .expect("session fixture");
    }
    FakeStore {
        agent: agent.to_owned(),
        locator: FakeLocator { root },
        last_ms: now.timestamp_millis(),
        days: vec![hofund_plugin_api::SessionDay::new(
            now.date_naive().to_string(),
            "model",
            SessionTokens::new(4, 3, 2, 1),
        )],
        count,
        child: false,
    }
}

async fn start(dir: &Path) -> Running {
    start_with_reads(dir, true).await
}

async fn start_with_reads(dir: &Path, reads: bool) -> Running {
    start_with_count(dir, reads, 1).await
}

async fn start_with_count(dir: &Path, reads: bool, count: usize) -> Running {
    let store = fake_store(dir, "fake", count);
    start_with_stores(dir, reads, vec![store]).await
}

async fn start_with_stores(dir: &Path, reads: bool, stores: Vec<FakeStore>) -> Running {
    let registry = Arc::new(FakeRegistry {
        stores: stores
            .into_iter()
            .map(|store| Arc::new(store) as Arc<dyn SessionStore>)
            .collect(),
    });
    let config = dir.join("hofund.toml");
    std::fs::write(
        &config,
        "listen = \"127.0.0.1:0\"\n[usage.prices.\"*/model\"]\ninput = 1.0\noutput = 1.0\ncache_read = 1.0\ncache_write = 1.0\n[usage.prices.\"*/other\"]\ninput = 1.0\noutput = 1.0\ncache_read = 1.0\ncache_write = 1.0\n",
    )
    .expect("config");
    let mut consent = Consent::new(dir);
    consent.settle();
    consent.set_reads(Some(reads), []).expect("consent");
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

async fn patch_reads(running: &Running, dir: &Path, body: serde_json::Value) -> reqwest::Response {
    let token = std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned();
    support::client()
        .patch(format!(
            "http://{}/v1/hofund/sessions/reads",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("PATCH response")
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
    let resume = list.sessions[0].resume.as_ref().expect("resume argv");
    assert_eq!(
        resume.argv,
        vec!["fake".to_owned(), "--resume".to_owned(), "one".to_owned()]
    );
    assert_eq!(resume.cwd, "/work/project");
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
async fn library_without_a_store_returns_an_empty_view_and_real_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;
    let response = get(&running, dir.path(), "/v1/hofund/library").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let view: hofund_control_proto::LibraryResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("library body"))
            .expect("library view");
    assert_eq!(view.version, API_VERSION);
    assert_eq!(
        view.dir,
        dir.path().join("library").to_string_lossy().as_ref()
    );
    assert!(view.rows.is_empty());
    assert!(view.agents.is_empty());
    assert_eq!(view.error, None);
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

#[tokio::test]
async fn detail_transcript_and_markdown_read_the_listed_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;

    let response = get(&running, dir.path(), "/v1/hofund/sessions/fake:one").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let detail: SessionDetailResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("detail body"))
            .expect("detail");
    assert_eq!(detail.session.id, "fake:one");
    assert!(detail.transcript_available);
    let resume = detail.session.resume.as_ref().expect("resume argv");
    assert_eq!(resume.argv[0], "fake");
    assert_eq!(resume.argv[2], "one");
    assert_eq!(resume.cwd, "/work/project");

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/fake:one/transcript",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let transcript: SessionTranscriptResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("transcript body"),
    )
    .expect("transcript");
    assert_eq!(transcript.parts.len(), 2);
    assert_eq!(transcript.parts[0].text, "Offline question");
    assert_eq!(transcript.parts[1].role, "assistant");

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/fake:one/transcript?sub=missing",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/fake:one/markdown",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .expect("content type"),
        "text/markdown; charset=utf-8"
    );
    let markdown = response.text().await.expect("Markdown body");
    assert!(markdown.contains("# Offline fixture"));
    assert!(markdown.contains("Offline question"));
    assert!(markdown.contains("Offline answer"));
    assert!(markdown_files(dir.path()).is_empty());

    for path in [
        "/v1/hofund/sessions/fake:missing",
        "/v1/hofund/sessions/fake:missing/transcript",
        "/v1/hofund/sessions/fake:missing/markdown",
    ] {
        let response = get(&running, dir.path(), path).await;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
    }
    support::shutdown(running).await;
}

#[tokio::test]
async fn transcript_is_forbidden_when_agent_reads_are_off() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start_with_reads(dir.path(), false).await;
    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/fake:one/transcript",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let error = decode_error_envelope(API_VERSION, &response.bytes().await.expect("error body"))
        .expect("error");
    assert_eq!(error.error.kind, ControlErrorKind::Forbidden);
    support::shutdown(running).await;
}

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).expect("fixture directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                files.push(path);
            }
        }
    }
    files
}

#[tokio::test]
async fn search_is_case_folded_and_manage_is_read_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;
    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/search?agent=fake&q=QUESTION",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let search: SessionSearchResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("search body"))
            .expect("search");
    assert_eq!(search.hits.len(), 1);
    assert_eq!(search.hits[0].id, "fake:one");
    assert!(search.hits[0].excerpt.contains("Offline question"));
    assert!(!search.cut);

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/manage?agent=fake",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let manage: SessionManageResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("manage body"))
            .expect("manage");
    assert_eq!(manage.agent, "fake");
    assert_eq!(manage.sessions.len(), 1);
    assert_eq!(manage.agents.len(), 1);
    assert_eq!(manage.agents[0].count, 1);
    assert!(!manage.agents[0].deletable);
    assert!(manage.trash.is_empty());
    assert!(!manage.recording && !manage.recorded);
    assert!(!std::path::Path::new(&manage.trash_dir).exists());
    assert_eq!(manage.revision.len(), 64);
    let response = get(&running, dir.path(), "/v1/hofund/sessions/manage").await;
    let default_view: SessionManageResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("default manage body"),
    )
    .expect("default manage");
    assert_eq!(default_view.agent, "");
    assert_eq!(default_view.sessions.len(), 1);
    support::shutdown(running).await;
}

#[tokio::test]
async fn manage_lists_every_agent_and_preserves_a_zero_match_filter() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start_with_stores(
        dir.path(),
        true,
        vec![
            fake_store(dir.path(), "many", 2),
            fake_store(dir.path(), "few", 1),
        ],
    )
    .await;

    let response = get(&running, dir.path(), "/v1/hofund/sessions/manage").await;
    let all: SessionManageResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("manage body"))
            .expect("manage");
    assert_eq!(all.agent, "");
    assert_eq!(all.sessions.len(), 3);
    assert_eq!(all.agents.len(), 2);
    assert_eq!(all.agents[0].agent, "many");
    assert_eq!(all.agents[0].count, 2);
    assert_eq!(all.agents[1].agent, "few");
    assert_eq!(all.agents[1].count, 1);

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/manage?agent=missing",
    )
    .await;
    let missing: SessionManageResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("manage body"))
            .expect("manage");
    assert_eq!(missing.agent, "missing");
    assert!(missing.sessions.is_empty());
    assert_eq!(missing.agents, all.agents);
    support::shutdown(running).await;
}

#[tokio::test]
async fn overview_costs_use_only_the_filtered_days_and_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let now = Local::now();
    let yesterday = now
        .date_naive()
        .checked_sub_days(chrono::Days::new(1))
        .expect("yesterday");
    let mut store = fake_store(dir.path(), "fake", 1);
    store.days = vec![
        hofund_plugin_api::SessionDay::new(
            now.date_naive().to_string(),
            "model",
            SessionTokens::new(4, 3, 2, 1),
        ),
        hofund_plugin_api::SessionDay::new(
            now.date_naive().to_string(),
            "other",
            SessionTokens::new(40, 30, 20, 10),
        ),
        hofund_plugin_api::SessionDay::new(
            yesterday.to_string(),
            "model",
            SessionTokens::new(400, 300, 200, 100),
        ),
    ];
    let running = start_with_stores(dir.path(), true, vec![store]).await;

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/overview?days=1&model=model",
    )
    .await;
    let overview: SessionOverviewResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("overview body"))
            .expect("overview");
    let expected = 10.0 / 1_000_000.0;
    assert_eq!(overview.count, 1);
    assert_eq!(overview.median_cost, expected);
    assert_eq!(overview.by_day.len(), 1);
    assert_eq!(overview.by_day[0].cost, expected);
    assert_eq!(overview.top.len(), 1);
    assert_eq!(overview.top[0].cost, expected);
    support::shutdown(running).await;
}

#[tokio::test]
async fn detail_lists_a_subagent_and_reads_its_transcript() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = fake_store(dir.path(), "fake", 2);
    store.child = true;
    std::fs::write(
        store.locator.root.join("child.jsonl"),
        b"{\"fixture\":true}\n",
    )
    .expect("child fixture");
    let running = start_with_stores(dir.path(), true, vec![store]).await;

    let response = get(&running, dir.path(), "/v1/hofund/sessions/fake:one").await;
    let detail: SessionDetailResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("detail body"))
            .expect("detail");
    assert_eq!(detail.session.subagents, vec!["fake:child"]);

    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/fake:one/transcript?sub=fake:child",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let transcript: SessionTranscriptResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("transcript body"),
    )
    .expect("transcript");
    assert_eq!(transcript.parts[0].text, "Child question");
    assert_eq!(transcript.parts[1].text, "Child answer");
    support::shutdown(running).await;
}

#[tokio::test]
async fn search_stops_at_two_hundred_hits() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start_with_count(dir.path(), true, 201).await;
    let response = get(
        &running,
        dir.path(),
        "/v1/hofund/sessions/search?q=question",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let search: SessionSearchResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("search body"))
            .expect("search");
    assert_eq!(search.hits.len(), 200);
    assert!(search.cut);
    assert_eq!(
        search
            .hits
            .iter()
            .map(|hit| hit.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        200
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn reads_patch_supports_optional_revision_and_rejects_stale_or_bad_ids() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(dir.path()).await;
    let response = get(&running, dir.path(), "/v1/hofund/sessions/reads").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let initial: SessionReadsResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("reads body")).expect("reads");
    assert!(initial.asked && initial.all);
    assert_eq!(initial.revision.len(), 64);
    assert_eq!(initial.found.len(), 1);
    assert_eq!(initial.found[0].agent, "fake");
    assert!(initial.found[0].on);

    let response = patch_reads(
        &running,
        dir.path(),
        serde_json::json!({"version": 1, "agents": {"fake": false}}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let updated: SessionReadsResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("updated body"))
            .expect("updated reads");
    assert!(!updated.found[0].on);
    assert!(updated.enabled.is_empty());
    assert_ne!(updated.revision, initial.revision);

    let response = patch_reads(
        &running,
        dir.path(),
        serde_json::json!({
            "version": 1,
            "expected_revision": initial.revision,
            "all": false
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let error = decode_error_envelope(API_VERSION, &response.bytes().await.expect("conflict body"))
        .expect("conflict");
    assert_eq!(error.error.kind, ControlErrorKind::Conflict);

    let response = patch_reads(
        &running,
        dir.path(),
        serde_json::json!({"version": 1, "agents": {"../bad": true}}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let error = decode_error_envelope(API_VERSION, &response.bytes().await.expect("bad id body"))
        .expect("bad id");
    assert_eq!(error.error.kind, ControlErrorKind::BadRequest);

    let response = get(&running, dir.path(), "/v1/hofund/sessions/reads").await;
    let current: SessionReadsResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("current body"))
            .expect("current reads");
    assert_eq!(current.revision, updated.revision);
    assert!(current.all);
    assert!(!current.found[0].on);
    support::shutdown(running).await;
}
