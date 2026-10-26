//! Mock J: persistent model state, safe filenames, and immutable user configuration.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_daemon::ModelsHost;
use hofund_provider::ModelPlan;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

const INTERVAL: u64 = 600_000;

fn at(millis: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
}

fn scratch(test: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "hofund-model-state-mock-j-{test}-{}",
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

fn multi_host(directory: &Path, server: &MockServer) -> ModelsHost {
    host(directory).with_model_plan(
        "multi",
        ModelPlan {
            chat: format!("{}/chat", server.uri()),
            anthropic: format!("{}/anthropic/v1", server.uri()),
            ..ModelPlan::default()
        },
    )
}

fn multi_config() -> hofund_config::Config {
    hofund_config::load(
        "[[providers]]\nid = \"multi\"\npreset = \"deepseek\"\nprotocol = \"openai-chat\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"10m\"\n",
    )
    .expect("config")
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

#[derive(Clone, Copy)]
enum AuthMode {
    Both,
    XApiOnly,
    BearerOnly,
    PinnedXApiOnly,
}

impl Match for AuthMode {
    fn matches(&self, request: &Request) -> bool {
        let x_api = request.headers.get("x-api-key");
        let bearer = request
            .headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer test-key");
        match self {
            Self::Both => x_api.is_some() && bearer,
            Self::XApiOnly => x_api.is_some_and(|value| value == "test-key") && !bearer,
            Self::BearerOnly => x_api.is_none() && bearer,
            Self::PinnedXApiOnly => {
                x_api.is_some_and(|value| value == "pinned-key")
                    && !request.headers.contains_key("authorization")
            }
        }
    }
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
    serde_json::from_slice(&std::fs::read(state_path(directory, provider)).expect("state"))
        .expect("JSON")
}

fn files_below(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).expect("directory entries") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn restart_preserves_list_sides_picks_auth_bases_and_due_times() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chat/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/chat/v1/models"))
        .respond_with(Answers::new(vec![
            rows(&["keep", "gone"]),
            rows(&["keep", "new"]),
            rows(&["keep", "new"]),
            ResponseTemplate::new(500),
        ]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/anthropic/v1/models"))
        .and(AuthMode::Both)
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("x-api-key and authorization cannot both be present"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/anthropic/v1/models"))
        .and(AuthMode::XApiOnly)
        .respond_with(Answers::new(vec![
            rows(&["claude"]),
            rows(&["claude"]),
            rows(&["claude"]),
            ResponseTemplate::new(500),
        ]))
        .mount(&server)
        .await;

    let directory = scratch("restart");
    let config_path = directory.join("hofund.toml");
    let config = multi_config();
    let first = multi_host(&directory, &server);
    first
        .list_models(&config, &config_path, "multi")
        .await
        .expect("initial list");
    first.set_picks(
        &config_path,
        "multi",
        &["keep".to_owned(), "gone".to_owned(), "typed".to_owned()],
    );
    first
        .list_models(&config, &config_path, "multi")
        .await
        .expect("delisting list");
    let before = state(&directory, "multi");
    assert_eq!(before["picks"], json!(["keep", "typed"]));
    assert_eq!(before["delisted"], json!(["gone"]));
    assert_eq!(before["auth"], "x-api-key");
    assert_eq!(before["bases"]["chat"], format!("{}/chat/v1", server.uri()));
    assert_eq!(before["sides"].as_object().expect("sides").len(), 2);
    let fetched_at = before["fetched_at"].as_u64().expect("fetch time");
    drop(first);

    let restarted = multi_host(&directory, &server);
    assert!(
        restarted
            .refresh_due(&config, &config_path, at(fetched_at + INTERVAL - 1_000))
            .await
            .is_empty()
    );
    assert_eq!(state(&directory, "multi"), before);
    assert_eq!(
        restarted
            .refresh_due(&config, &config_path, at(fetched_at + INTERVAL))
            .await,
        ["multi"]
    );
    let after = state(&directory, "multi");
    for field in [
        "list",
        "sides",
        "picks",
        "picked_from",
        "delisted",
        "auth",
        "bases",
    ] {
        assert_eq!(before[field], after[field], "{field}");
    }
    let picks = restarted.picks(&config_path, "multi");
    assert_eq!(picks.picks(), ["keep", "typed"]);
    assert_eq!(picks.delisted(), ["gone"]);

    let failed_at = fetched_at + 2 * INTERVAL;
    assert_eq!(
        restarted
            .refresh_due(&config, &config_path, at(failed_at))
            .await,
        ["multi"]
    );
    let failed = state(&directory, "multi");
    let retry_at = failed["retry_at"].as_u64().expect("retry time");
    assert_eq!(retry_at, failed_at + INTERVAL);
    drop(restarted);

    let after_failure = multi_host(&directory, &server);
    assert!(
        after_failure
            .refresh_due(&config, &config_path, at(retry_at - 1_000))
            .await
            .is_empty()
    );
    assert_eq!(state(&directory, "multi"), failed);
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn ids_differing_only_by_case_use_distinct_files_and_state() {
    let server = MockServer::start().await;
    for (route, id) in [
        ("/upper/v1/models", "upper-model"),
        ("/lower/v1/models", "lower-model"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(rows(&[id]))
            .expect(2)
            .mount(&server)
            .await;
    }
    let directory = scratch("case");
    let config_path = directory.join("hofund.toml");
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"Primary\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/upper/v1\"\napi_key = \"test-key\"\n[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/lower/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ))
    .expect("config");
    let first = host(&directory);
    for (provider, expected) in [("Primary", "upper-model"), ("primary", "lower-model")] {
        let listed = first
            .list_models(&config, &config_path, provider)
            .await
            .expect("initial list");
        assert_eq!(listed.models[0].id(), expected);
    }
    let upper_path = state_path(&directory, "Primary");
    let lower_path = state_path(&directory, "primary");
    assert_ne!(upper_path, lower_path);
    assert_eq!(state(&directory, "Primary")["list"][0]["id"], "upper-model");
    assert_eq!(state(&directory, "primary")["list"][0]["id"], "lower-model");
    drop(first);

    let restarted = host(&directory);
    for (provider, expected) in [("Primary", "upper-model"), ("primary", "lower-model")] {
        let listed = restarted
            .list_models(&config, &config_path, provider)
            .await
            .expect("restarted list");
        assert_eq!(listed.models[0].id(), expected);
    }
    assert_eq!(state(&directory, "Primary")["list"][0]["id"], "upper-model");
    assert_eq!(state(&directory, "primary")["list"][0]["id"], "lower-model");
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn unusual_ids_create_only_files_directly_inside_host_models() {
    let server = MockServer::start().await;
    for index in 0..4 {
        Mock::given(method("GET"))
            .and(path(format!("/{index}/v1/models")))
            .respond_with(rows(&[&format!("model-{index}")]))
            .mount(&server)
            .await;
    }
    let directory = scratch("contained").join("config");
    std::fs::create_dir_all(&directory).expect("config directory");
    let config_path = directory.join("hofund.toml");
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"a/b\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/0/v1\"\n[[providers]]\nid = \"../escape\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/1/v1\"\n[[providers]]\nid = \"..\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/2/v1\"\n[[providers]]\nid = \"x\\\\y:z\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/3/v1\"\n",
        server.uri()
    ))
    .expect("config");
    let host = host(&directory);
    for provider in ["a/b", "../escape", "..", "x\\y:z"] {
        host.list_models(&config, &config_path, provider)
            .await
            .expect("list");
    }

    let model_directory = directory.join(".hofund/host/models");
    // The plugin host's account store predates this change; skip it here.
    let accounts = directory.join(".hofund/accounts.json");
    let files = files_below(&directory)
        .into_iter()
        .filter(|file| *file != accounts)
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 4);
    for file in files {
        assert_eq!(file.parent(), Some(model_directory.as_path()));
        assert_eq!(
            file.extension().and_then(|part| part.to_str()),
            Some("json")
        );
    }
    for ancestor in directory.ancestors().take(3) {
        assert!(!ancestor.join("escape").exists(), "escape in {ancestor:?}");
    }
    let _ = std::fs::remove_dir_all(directory.parent().expect("scratch root"));
}

#[tokio::test]
async fn mismatched_stored_id_is_fresh_in_memory_and_never_written() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(Answers::new(vec![rows(&["old"]), rows(&["fresh"])]))
        .expect(3)
        .mount(&server)
        .await;
    let directory = scratch("mismatch");
    let config_path = directory.join("hofund.toml");
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"wanted\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"10m\"\n",
        server.uri()
    ))
    .expect("config");
    host(&directory)
        .list_models(&config, &config_path, "wanted")
        .await
        .expect("initial state path");
    let path = state_path(&directory, "wanted");
    let mismatched = br#"{"version":1,"id":"other"}"#;
    std::fs::write(&path, mismatched).expect("mismatched state");

    let restarted = host(&directory);
    let listed = restarted
        .list_models(&config, &config_path, "wanted")
        .await
        .expect("fresh list");
    assert_eq!(listed.models[0].id(), "fresh");
    assert_eq!(std::fs::read(&path).expect("state bytes"), mismatched);
    assert_eq!(
        restarted
            .refresh_due(
                &config,
                &config_path,
                SystemTime::now() + Duration::from_millis(INTERVAL),
            )
            .await,
        ["wanted"]
    );
    assert_eq!(std::fs::read(&path).expect("state bytes"), mismatched);
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn learned_auth_and_fix_v1_leave_toml_untouched_and_pinned_auth_wins() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .and(AuthMode::Both)
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("x-api-key and authorization cannot both be present"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .and(AuthMode::XApiOnly)
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .and(AuthMode::BearerOnly)
        .respond_with(rows(&["claude"]))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .and(AuthMode::PinnedXApiOnly)
        .respond_with(rows(&["claude"]))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(rows(&["chat"]))
        .expect(1)
        .mount(&server)
        .await;

    let directory = scratch("immutable-toml");
    let config_path = directory.join("hofund.toml");
    let source = format!(
        "[[providers]]\nid = \"messages\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{0}/v1\"\napi_key = \"test-key\"\n[providers.models]\nurl = \"{0}/exact/models\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}\"\napi_key = \"test-key\"\n",
        server.uri()
    );
    std::fs::write(&config_path, &source).expect("config");
    let original = std::fs::read(&config_path).expect("original TOML");
    let config = hofund_config::load(&source).expect("config");
    let host = host(&directory);
    host.list_models(&config, &config_path, "messages")
        .await
        .expect("learned auth");
    let fixed = host
        .list_models(&config, &config_path, "chat")
        .await
        .expect("learned base");
    assert_eq!(
        fixed.corrected_base.as_deref(),
        Some(format!("{}/v1", server.uri()).as_str())
    );
    assert_eq!(std::fs::read(&config_path).expect("TOML"), original);

    let pinned_source = source.replace(
        "[[providers]]\nid = \"chat\"",
        "[providers.headers]\nx-api-key = \"pinned-key\"\n[[providers]]\nid = \"chat\"",
    );
    let pinned = hofund_config::load(&pinned_source).expect("pinned config");
    host.list_models(&pinned, &config_path, "messages")
        .await
        .expect("pinned auth");
    assert_eq!(std::fs::read(&config_path).expect("TOML"), original);
    for file in files_below(&directory) {
        let relative = file.strip_prefix(&directory).expect("relative path");
        assert!(
            relative == Path::new("hofund.toml")
                // The plugin host's account store predates this change.
                || relative == Path::new(".hofund/accounts.json")
                || (relative.starts_with(".hofund/host/models")
                    && file.extension().and_then(|part| part.to_str()) == Some("json")),
            "unexpected file {relative:?}"
        );
    }
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}
