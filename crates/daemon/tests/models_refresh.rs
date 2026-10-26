//! Refresh scheduling, picks, and per-base state against local mocks.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_daemon::{ModelsHost, ProviderModels, RefreshTiming};
use hofund_provider::ModelPlan;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const NOW: u64 = 1_000_000;

fn at(millis: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
}

fn scratch(test: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "hofund-model-refresh-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("directory");
    directory
}

fn host(directory: &Path) -> ModelsHost {
    ModelsHost::new(Duration::from_secs(2), Duration::from_secs(2))
        .with_state_root(directory.join(".hofund"))
}

fn config(server: &MockServer, refresh: &str) -> hofund_config::Config {
    hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"{refresh}\"\n",
        server.uri()
    ))
    .expect("config")
}

fn multi_host(directory: &Path, server: &MockServer) -> ModelsHost {
    host(directory).with_model_plan(
        "multi",
        ModelPlan {
            chat: format!("{}/chat/v1", server.uri()),
            responses: format!("{}/responses/v1", server.uri()),
            ..ModelPlan::default()
        },
    )
}

fn multi_config() -> hofund_config::Config {
    hofund_config::load(
        "[[providers]]\nid = \"multi\"\npreset = \"deepseek\"\nprotocol = \"openai-chat\"\napi_key = \"test-key\"\n",
    )
    .expect("config")
}

fn ids(listed: &ProviderModels) -> Vec<&str> {
    listed.models.iter().map(|model| model.id()).collect()
}

fn rows(ids: &[&str]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()
    }))
}

struct Answers {
    answers: Mutex<VecDeque<ResponseTemplate>>,
}

impl Answers {
    fn new(answers: Vec<ResponseTemplate>) -> Self {
        assert!(!answers.is_empty());
        Self {
            answers: Mutex::new(answers.into()),
        }
    }
}

impl Respond for Answers {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let mut answers = self.answers.lock().expect("answers");
        if answers.len() > 1 {
            answers.pop_front().expect("answer")
        } else {
            answers.front().expect("answer").clone()
        }
    }
}

async fn mount(server: &MockServer, route: &'static str, answers: Vec<ResponseTemplate>) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(Answers::new(answers))
        .mount(server)
        .await;
}

fn state_path(directory: &Path, provider: &str) -> PathBuf {
    std::fs::read_dir(directory.join(".hofund/host/models"))
        .expect("state directory")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|state| state["id"] == provider)
        })
        .expect("provider state")
}

fn state(directory: &Path, provider: &str) -> Value {
    let path = state_path(directory, provider);
    serde_json::from_slice(&std::fs::read(path).expect("state")).expect("JSON")
}

fn set_picks(host: &ModelsHost, provider: &str, picks: &[&str]) {
    let picks = picks.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    host.set_picks(provider, &picks);
}

#[tokio::test]
async fn refresh_due_uses_fetched_at_and_skips_off_and_static_providers() {
    let server = MockServer::start().await;
    mount(&server, "/v1/models", vec![rows(&["model"])]).await;
    let directory = scratch("due");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = config(&server, "10m");

    assert_eq!(
        host.refresh_due(&config, &config_path, at(NOW)).await,
        ["chat"]
    );
    assert_eq!(
        host.refresh_due(&config, &config_path, at(NOW + 599_999))
            .await,
        Vec::<String>::new()
    );
    assert_eq!(
        host.refresh_due(&config, &config_path, at(NOW + 600_000))
            .await,
        ["chat"]
    );
    assert_eq!(state(&directory, "chat")["fetched_at"], NOW + 600_000);

    let skipped = hofund_config::load(&format!(
        "[[providers]]\nid = \"off\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/v1\"\n[providers.models]\nrefresh = \"off\"\n[[providers]]\nid = \"blank\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/v1\"\n[providers.models]\nrefresh = \" \"\n[[providers]]\nid = \"static\"\npreset = \"bedrock\"\nprotocol = \"openai-chat\"\n[providers.models]\nrefresh = \"10m\"\n",
        server.uri()
    ))
    .expect("skipped config");
    assert!(
        host.refresh_due(&skipped, &config_path, at(NOW))
            .await
            .is_empty()
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn catalog_bound_never_dials_models_url_or_schedules_a_fetch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .respond_with(rows(&["network-model"]))
        .expect(0)
        .mount(&server)
        .await;
    let directory = scratch("catalog-models-url");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"static\"\npreset = \"google-vertex\"\n[providers.options]\nproject = \"my-project\"\n[providers.models]\nrefresh = \"10m\"\nurl = \"{}/exact/models\"\n",
        server.uri()
    ))
    .expect("config");

    host.list_models(&config, &config_path, "static")
        .await
        .expect("static list");
    assert!(
        host.refresh_due(&config, &config_path, at(NOW))
            .await
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn empty_filtered_success_uses_the_normal_per_try_wait() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/models",
        vec![rows(&["text-embedding-3-small"])],
    )
    .await;
    let directory = scratch("empty-filtered-refresh");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = config(&server, "10m");

    assert_eq!(
        host.refresh_due(&config, &config_path, at(NOW)).await,
        ["chat"]
    );
    let tried = state(&directory, "chat");
    assert_eq!(tried["fetched_at"], Value::Null);
    assert_eq!(tried["retry_at"], NOW + 600_000);
    assert!(
        host.refresh_due(&config, &config_path, at(NOW + 60_000))
            .await
            .is_empty()
    );
    assert_eq!(
        host.refresh_due(&config, &config_path, at(NOW + 600_000))
            .await,
        ["chat"]
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn failed_refresh_waits_for_interval_cap_and_retry_after() {
    for (name, refresh, status, after, expected) in [
        ("interval", "10m", 500, None, 600_000),
        ("capped", "6h", 500, None, 900_000),
        ("no-header", "10m", 429, None, 600_000),
        ("header", "10m", 429, Some(3600), 3_600_000),
        ("short-header", "10m", 429, Some(60), 600_000),
        ("retry-after-cap", "10m", 429, Some(864_000), 86_400_000),
    ] {
        let server = MockServer::start().await;
        let mut failure = ResponseTemplate::new(status);
        if let Some(after) = after {
            failure = failure.insert_header("retry-after", after.to_string());
        }
        mount(&server, "/v1/models", vec![failure, rows(&["model"])]).await;
        let directory = scratch(name);
        let config_path = directory.join("hofund.toml");
        let host = host(&directory);
        let config = config(&server, refresh);

        assert_eq!(
            host.refresh_due(&config, &config_path, at(NOW)).await,
            ["chat"]
        );
        let failed = state(&directory, "chat");
        assert_eq!(failed["tried_at"], NOW);
        assert_eq!(failed["retry_at"], NOW + expected);
        assert!(
            host.refresh_due(&config, &config_path, at(NOW + expected - 1))
                .await
                .is_empty()
        );
        assert_eq!(
            host.refresh_due(&config, &config_path, at(NOW + expected))
                .await,
            ["chat"]
        );
        let succeeded = state(&directory, "chat");
        assert_eq!(succeeded["retry_at"], Value::Null);
        assert_eq!(succeeded["fetched_at"], NOW + expected);
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            2,
            "{name}"
        );
        let _ = std::fs::remove_dir_all(directory);
    }
}

#[tokio::test]
async fn dead_side_keeps_its_own_saved_part() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/v1/models",
        vec![rows(&["chat", "shared"]), rows(&["chat-new"])],
    )
    .await;
    mount(
        &server,
        "/responses/v1/models",
        vec![rows(&["response", "shared"]), ResponseTemplate::new(500)],
    )
    .await;
    let directory = scratch("dead-own");
    let config_path = directory.join("hofund.toml");
    let host = multi_host(&directory, &server);
    let config = multi_config();
    host.list_models(&config, &config_path, "multi")
        .await
        .expect("initial sides");

    let listed = host
        .list_models(&config, &config_path, "multi")
        .await
        .expect("dead side");
    assert_eq!(ids(&listed), ["chat-new", "response", "shared"]);
    let state = state(&directory, "multi");
    let side = &state["sides"][format!("{}/responses/v1", server.uri())];
    assert_eq!(side.as_array().expect("side").len(), 2);
    assert!(side.to_string().contains("response"));
    assert!(side.to_string().contains("shared"));
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn sibling_fix_v1_side_is_found_at_its_next_request_base() {
    let server = MockServer::start().await;
    mount(&server, "/chat/models", vec![ResponseTemplate::new(500)]).await;
    mount(
        &server,
        "/chat/v1/models",
        vec![rows(&["chat"]), rows(&["chat-new"])],
    )
    .await;
    mount(
        &server,
        "/responses/models",
        vec![ResponseTemplate::new(500)],
    )
    .await;
    mount(
        &server,
        "/responses/v1/models",
        vec![rows(&["response"]), ResponseTemplate::new(500)],
    )
    .await;
    let directory = scratch("sibling-fix-v1-side");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory).with_model_plan(
        "multi",
        ModelPlan {
            chat: format!("{}/chat", server.uri()),
            responses: format!("{}/responses", server.uri()),
            ..ModelPlan::default()
        },
    );
    let config = multi_config();

    host.list_models(&config, &config_path, "multi")
        .await
        .expect("initial corrected sides");
    let saved = state(&directory, "multi");
    let sibling_base = format!("{}/responses", server.uri());
    assert_eq!(saved["bases"]["responses"], format!("{sibling_base}/v1"));
    assert_eq!(saved["sides"][sibling_base.clone()][0]["id"], "response");

    let listed = host
        .list_models(&config, &config_path, "multi")
        .await
        .expect("dead sibling side");
    assert_eq!(ids(&listed), ["chat-new", "response"]);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn never_answered_base_keeps_nothing_when_sides_exist() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/v1/models",
        vec![rows(&["chat"]), rows(&["chat-new"])],
    )
    .await;
    mount(
        &server,
        "/responses/v1/models",
        vec![rows(&["response"]), ResponseTemplate::new(500)],
    )
    .await;
    let directory = scratch("dead-never");
    let config_path = directory.join("hofund.toml");
    let config = multi_config();
    multi_host(&directory, &server)
        .list_models(&config, &config_path, "multi")
        .await
        .expect("initial sides");
    let path = state_path(&directory, "multi");
    let mut saved = state(&directory, "multi");
    saved["sides"]
        .as_object_mut()
        .expect("sides")
        .remove(&format!("{}/responses/v1", server.uri()));
    std::fs::write(&path, serde_json::to_vec_pretty(&saved).expect("JSON")).expect("state");

    let listed = multi_host(&directory, &server)
        .list_models(&config, &config_path, "multi")
        .await
        .expect("dead side");
    assert_eq!(ids(&listed), ["chat-new"]);
    assert!(
        !state(&directory, "multi")["sides"]
            .as_object()
            .expect("sides")
            .contains_key(&format!("{}/responses/v1", server.uri()))
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn missing_sides_keep_unlisted_rows_once_without_assigning_a_side() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/v1/models",
        vec![
            rows(&["old-chat", "old-dead", "shared"]),
            rows(&["old-chat", "shared"]),
            rows(&["old-chat", "shared"]),
        ],
    )
    .await;
    mount(
        &server,
        "/responses/v1/models",
        vec![ResponseTemplate::new(500)],
    )
    .await;
    let directory = scratch("dead-legacy");
    let config_path = directory.join("hofund.toml");
    let first_config = hofund_config::load(&format!(
        "[[providers]]\nid = \"multi\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/chat/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ))
    .expect("single-base config");
    host(&directory)
        .list_models(&first_config, &config_path, "multi")
        .await
        .expect("legacy list");
    let config = multi_config();
    let host = multi_host(&directory, &server);
    let once = host
        .list_models(&config, &config_path, "multi")
        .await
        .expect("legacy retention");
    assert_eq!(ids(&once), ["old-chat", "shared", "old-dead"]);
    assert_eq!(
        state(&directory, "multi")["sides"]
            .as_object()
            .expect("sides")
            .len(),
        1
    );
    let twice = host
        .list_models(&config, &config_path, "multi")
        .await
        .expect("later dead side");
    assert_eq!(ids(&twice), ["old-chat", "shared"]);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn all_failed_bases_leave_list_sides_picks_and_fetch_time_unchanged() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/v1/models",
        vec![rows(&["chat"]), ResponseTemplate::new(500)],
    )
    .await;
    mount(
        &server,
        "/responses/v1/models",
        vec![rows(&["response"]), ResponseTemplate::new(500)],
    )
    .await;
    let directory = scratch("all-failed-state");
    let config_path = directory.join("hofund.toml");
    let host = multi_host(&directory, &server);
    let config = multi_config();
    host.list_models(&config, &config_path, "multi")
        .await
        .expect("initial sides");
    set_picks(&host, "multi", &["chat"]);
    let before = state(&directory, "multi");
    host.list_models(&config, &config_path, "multi")
        .await
        .expect_err("all failed");
    let after = state(&directory, "multi");
    for field in [
        "list",
        "sides",
        "picks",
        "picked_from",
        "delisted",
        "fetched_at",
    ] {
        assert_eq!(before[field], after[field], "{field}");
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn delist_restore_and_combined_updates_preserve_hand_typed_picks() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/models",
        vec![
            rows(&["drop", "restore", "keep"]),
            rows(&["drop", "keep"]),
            rows(&["keep", "restore"]),
            rows(&["drop", "keep", "restore"]),
        ],
    )
    .await;
    let directory = scratch("delist-restore");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = config(&server, "off");
    host.list_models(&config, &config_path, "chat")
        .await
        .expect("first list");
    set_picks(&host, "chat", &["drop", "restore", "keep", "typed"]);

    host.list_models(&config, &config_path, "chat")
        .await
        .expect("delist restore candidate");
    let picks = host.picks("chat");
    assert_eq!(picks.picks(), ["drop", "keep", "typed"]);
    assert_eq!(picks.delisted(), ["restore"]);

    host.list_models(&config, &config_path, "chat")
        .await
        .expect("combined drop and restore");
    let picks = host.picks("chat");
    assert_eq!(picks.picks(), ["keep", "typed", "restore"]);
    assert_eq!(picks.delisted(), ["drop"]);

    host.list_models(&config, &config_path, "chat")
        .await
        .expect("restore dropped pick");
    let picks = host.picks("chat");
    assert_eq!(picks.picks(), ["keep", "typed", "restore", "drop"]);
    assert!(picks.delisted().is_empty());
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn empty_answer_is_an_error_and_changes_no_saved_state() {
    let server = MockServer::start().await;
    mount(&server, "/exact/models", vec![rows(&["model"]), rows(&[])]).await;
    let directory = scratch("empty-list");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/v1\"\napi_key = \"test-key\"\n[providers.models]\nurl = \"{0}/exact/models\"\n",
        server.uri()
    ))
    .expect("config");
    host.list_models(&config, &config_path, "chat")
        .await
        .expect("first list");
    set_picks(&host, "chat", &["model"]);
    let before = state(&directory, "chat");
    let error = host
        .list_models(&config, &config_path, "chat")
        .await
        .expect_err("empty list");
    assert!(error.to_string().contains("no models listed"));
    let after = state(&directory, "chat");
    for field in [
        "list",
        "sides",
        "picks",
        "picked_from",
        "delisted",
        "fetched_at",
    ] {
        assert_eq!(before[field], after[field], "{field}");
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn auto_include_takes_only_new_matching_chat_rows() {
    let server = MockServer::start().await;
    let second = ResponseTemplate::new(200).set_body_json(json!({
        "data": [
            {"id": "existing"},
            {"id": "gpt-old"},
            {"id": "GPT-new"},
            {"id": "claude-case"},
            {"id": "Claude-no"},
            {"id": "gpt-image-2", "kind": "image"}
        ]
    }));
    mount(
        &server,
        "/v1/models",
        vec![rows(&["existing", "gpt-old"]), second],
    )
    .await;
    let directory = scratch("auto-include");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"10m\"\nauto_include = [\"gpt-*\", \"re:(?-i)claude-.*\"]\n",
        server.uri()
    ))
    .expect("config");

    host.refresh_due(&config, &config_path, at(NOW)).await;
    assert!(host.picks("chat").picks().is_empty());
    set_picks(&host, "chat", &["existing"]);
    host.refresh_due(&config, &config_path, at(NOW + 600_000))
        .await;
    let picks = host.picks("chat");
    assert_eq!(picks.picks(), ["GPT-new", "claude-case", "existing"]);
    assert_eq!(
        picks.picked_from(),
        ["existing", "gpt-old", "GPT-new", "claude-case", "Claude-no"]
    );
    assert!(!picks.picks().contains(&"gpt-old".to_owned()));
    assert!(!picks.picks().contains(&"gpt-image-2".to_owned()));
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn manual_list_auto_includes_new_matching_rows() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/v1/models",
        vec![rows(&["existing"]), rows(&["existing", "new-x"])],
    )
    .await;
    let directory = scratch("manual-auto-include");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"off\"\nauto_include = [\"new-*\"]\n",
        server.uri()
    ))
    .expect("config");

    host.list_models(&config, &config_path, "chat")
        .await
        .expect("initial list");
    set_picks(&host, "chat", &["existing"]);
    host.list_models(&config, &config_path, "chat")
        .await
        .expect("manual list");

    let picks = host.picks("chat");
    assert_eq!(picks.picks(), ["new-x", "existing"]);
    assert_eq!(picks.picked_from(), ["existing", "new-x"]);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn probe_pool_puts_saved_picks_after_routes_and_before_the_list() {
    let server = MockServer::start().await;
    mount(&server, "/v1/models", vec![rows(&["listed", "picked"])]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;
    let directory = scratch("picked-probe");
    let config_path = directory.join("hofund.toml");
    let host = host(&directory);
    let config = config(&server, "off");
    host.list_models(&config, &config_path, "chat")
        .await
        .expect("list");
    set_picks(&host, "chat", &["picked"]);
    let probed = host
        .probe(&config, &config_path, "chat", None)
        .await
        .expect("picked probe");
    assert_eq!(probed.model, "picked");
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn running_refresh_starts_and_stops_on_shutdown() {
    let server = MockServer::start().await;
    mount(&server, "/exact/models", vec![rows(&["model"])]).await;
    let directory = scratch("running-refresh");
    let config_path = directory.join("hofund.toml");
    std::fs::write(
        &config_path,
        format!(
            "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"10m\"\nurl = \"{0}/exact/models\"\n",
            server.uri()
        ),
    )
    .expect("config");
    let running = hofund_daemon::start_for_test_with_refresh(
        &config_path,
        RefreshTiming::new(
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("daemon");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("refresh tick");
    running.shutdown().await;
    let stopped = server.received_requests().await.expect("requests").len();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        stopped
    );
    let _ = std::fs::remove_dir_all(directory);
}
