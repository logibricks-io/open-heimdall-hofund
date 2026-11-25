//! Address-added relay effort ids, from the list through the posted body.

use std::sync::Arc;

use hofund_codec::{AnthropicMessages, Codec};
use hofund_plugin_api::{Effort, Request};
use hofund_router::{CooldownPolicy, Cooldowns, Snapshot};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request as WireRequest, ResponseTemplate};

const CHAT_OK: &str = r#"{"id":"c1","model":"m","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
const MESSAGES_OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;

struct Model(&'static str);

impl Match for Model {
    fn matches(&self, request: &WireRequest) -> bool {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|body| body.get("model")?.as_str().map(str::to_owned))
            .is_some_and(|model| model == self.0)
    }
}

async fn chat_ok(server: &MockServer, model: &'static str, count: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(Model(model))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_OK))
        .expect(count)
        .mount(server)
        .await;
}

async fn messages_ok(server: &MockServer, model: &'static str, count: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(Model(model))
        .respond_with(ResponseTemplate::new(200).set_body_string(MESSAGES_OK))
        .expect(count)
        .mount(server)
        .await;
}

fn messages_body(model: &str, extra: &str) -> Request {
    let body = format!(
        r#"{{"model":"{model}","max_tokens":32000,"messages":[{{"role":"user","content":"hi"}}]{extra}}}"#
    );
    AnthropicMessages
        .decode_request(body.as_bytes())
        .expect("messages decode")
}

fn thinking_budget(kind: &str, budget: u64) -> String {
    format!(r#","thinking":{{"type":"{kind}","budget_tokens":{budget}}}"#)
}

async fn count_ok(server: &MockServer, model: &'static str, count: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(Model(model))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(count)
        .mount(server)
        .await;
}

async fn relay_snapshot(
    config: &hofund_config::Config,
    host: &super::ModelsHost,
) -> (tempfile::TempDir, Snapshot, super::ProviderModels) {
    let registry: Arc<dyn super::DaemonRegistry> = Arc::new(super::RegistryAdapter);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("hofund.toml");
    let super::Built {
        built,
        limits,
        instances,
    } = super::providers(
        config,
        &path,
        None,
        super::Mode::Serve,
        Some(host),
        None,
        None,
        &registry,
    )
    .expect("providers");
    host.install_upstreams(&instances);
    let listed = host
        .list_models(config, &path, &config.providers[0].id)
        .await
        .expect("model list");
    let (routes, policy) = super::route_plan(config, Some(host), Some(&built), &limits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let (snapshot, _) =
        super::membership::open_with_table(&built, routes, policy, cooldowns).expect("router");
    (dir, snapshot, listed)
}

fn source_family() -> Vec<hofund_plugin_api::ModelSpec> {
    [
        "model-4-alpha-low",
        "model-4-alpha-medium",
        "model-4-alpha-high",
    ]
    .into_iter()
    .map(|id| hofund_plugin_api::ModelSpec::new(id, None).expect("model"))
    .collect()
}

fn open_routes(
    config: &hofund_config::Config,
    host: &super::ModelsHost,
) -> (tempfile::TempDir, Snapshot) {
    let registry: Arc<dyn super::DaemonRegistry> = Arc::new(super::RegistryAdapter);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("hofund.toml");
    let super::Built {
        built,
        limits,
        instances,
    } = super::providers(
        config,
        &path,
        None,
        super::Mode::Serve,
        Some(host),
        None,
        None,
        &registry,
    )
    .expect("providers");
    host.install_upstreams(&instances);
    let (routes, policy) = super::route_plan(config, Some(host), Some(&built), &limits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let (snapshot, _) =
        super::membership::open_with_table(&built, routes, policy, cooldowns).expect("router");
    (dir, snapshot)
}

fn messages_config(base: &str, provider: &str) -> hofund_config::Config {
    hofund_config::load(&format!(
        "[[providers]]\nid = \"{provider}\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{base}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-4-alpha\"\nupstreams = [\"{provider}\"]\n",
    ))
    .expect("config")
}

fn ask(model: &str, effort: Option<Effort>) -> Request {
    let mut request = Request::new(model);
    request.params.effort = effort;
    request
}

#[tokio::test]
async fn d246_chat_relay_folds_and_sends_effort_ids_on_every_translation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "model-4-alpha-high"},
                {"id": "model-4-alpha-low"},
                {"id": "model-4-alpha-medium"},
                {"id": "model-4-beta-low"},
                {"id": "model-4-highspeed"}
            ]
        })))
        .expect(2)
        .mount(&server)
        .await;
    for (model, count) in [
        ("model-4-alpha-medium", 2),
        ("model-4-alpha-low", 2),
        ("model-4-alpha-high", 2),
    ] {
        chat_ok(&server, model, count).await;
    }
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-4-alpha\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let (_dir, snapshot, listed) = relay_snapshot(&config, &host).await;
    assert_eq!(
        listed
            .models
            .iter()
            .map(|model| model.id())
            .collect::<Vec<_>>(),
        ["model-4-alpha", "model-4-beta-low", "model-4-highspeed"]
    );
    assert_eq!(listed.models[0].efforts(), ["low", "medium", "high"]);
    assert_eq!(host.source_cached("relay").models.len(), 5);
    assert_eq!(host.cached("relay").models.len(), 3);

    for (model, effort, inbound, expected) in [
        ("model-4-alpha", None, "openai-chat", "model-4-alpha-medium"),
        (
            "model-4-alpha",
            Some(Effort::Low),
            "openai-responses",
            "model-4-alpha-low",
        ),
        (
            "model-4-alpha",
            Some(Effort::Xhigh),
            "anthropic-messages",
            "model-4-alpha-high",
        ),
        (
            "model-4-alpha-high",
            Some(Effort::Low),
            "openai-chat",
            "model-4-alpha-high",
        ),
        (
            "relay/model-4-alpha-low",
            Some(Effort::High),
            "openai-chat",
            "model-4-alpha-low",
        ),
    ] {
        let routed = snapshot
            .load()
            .dispatch(&ask(model, effort), inbound)
            .await
            .expect("dispatch");
        assert_eq!(routed.upstream_model, expected);
    }

    let probe = host
        .probe(
            &config,
            &_dir.path().join("hofund.toml"),
            "relay",
            Some("model-4-alpha"),
        )
        .await
        .expect("family probe");
    assert_eq!(probe.model, "model-4-alpha");
    server.verify().await;
}

#[tokio::test]
async fn d246_messages_count_uses_nearest_and_preserves_old_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "model-4-alpha-low"}, {"id": "model-4-alpha-High"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    count_ok(&server, "model-4-alpha-low", 1).await;
    count_ok(&server, "model-4-alpha-High", 2).await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-4-alpha\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let (_dir, snapshot, listed) = relay_snapshot(&config, &host).await;
    assert_eq!(listed.models.len(), 1);
    assert_eq!(listed.models[0].efforts(), ["low", "high"]);

    for (model, effort, expected) in [
        ("model-4-alpha", Some(Effort::Low), "model-4-alpha-low"),
        ("model-4-alpha", None, "model-4-alpha-High"),
        (
            "model-4-alpha-High",
            Some(Effort::Low),
            "model-4-alpha-High",
        ),
    ] {
        let counted = snapshot
            .load()
            .count(&ask(model, effort), "anthropic-messages")
            .await
            .expect("count");
        assert_eq!(counted.tokens, 7);
        assert!(!counted.estimated);
        let calls = server.received_requests().await.expect("requests");
        let last = calls.last().expect("count request");
        let body: serde_json::Value = serde_json::from_slice(&last.body).expect("body");
        assert_eq!(body["model"], expected);
    }
    server.verify().await;
}

#[tokio::test]
async fn d246_messages_thinking_budget_selects_the_member_on_execute_and_count() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "model-4-alpha-low"},
                {"id": "model-4-alpha-medium"},
                {"id": "model-4-alpha-high"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    messages_ok(&server, "model-4-alpha-high", 2).await;
    messages_ok(&server, "model-4-alpha-low", 1).await;
    count_ok(&server, "model-4-alpha-high", 1).await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-4-alpha\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let (_dir, snapshot, _) = relay_snapshot(&config, &host).await;
    let budget = thinking_budget("enabled", 30_000);
    let decoded = messages_body("model-4-alpha", &budget);
    assert_eq!(decoded.params.effort, Some(Effort::Xhigh));
    let routed = snapshot
        .load()
        .dispatch(&decoded, "anthropic-messages")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_model, "model-4-alpha-high");
    let adaptive = messages_body("model-4-alpha", &thinking_budget("adaptive", 30_000));
    let routed = snapshot
        .load()
        .dispatch(&adaptive, "anthropic-messages")
        .await
        .expect("adaptive dispatch");
    assert_eq!(routed.upstream_model, "model-4-alpha-high");
    let preferred = messages_body(
        "model-4-alpha",
        &format!(
            r#"{},"output_config":{{"effort":"low"}}"#,
            thinking_budget("enabled", 30_000)
        ),
    );
    assert_eq!(preferred.params.effort, Some(Effort::Low));
    let routed = snapshot
        .load()
        .dispatch(&preferred, "anthropic-messages")
        .await
        .expect("output_config wins");
    assert_eq!(routed.upstream_model, "model-4-alpha-low");
    let counted = snapshot
        .load()
        .count(&decoded, "anthropic-messages")
        .await
        .expect("count");
    assert_eq!(counted.tokens, 7);
    assert!(!counted.estimated);
    let calls = server.received_requests().await.expect("requests");
    let posted = calls
        .iter()
        .filter(|call| {
            call.url.path() == "/v1/messages" || call.url.path() == "/v1/messages/count_tokens"
        })
        .map(|call| serde_json::from_slice::<serde_json::Value>(&call.body).expect("body"))
        .collect::<Vec<_>>();
    assert_eq!(posted.len(), 4);
    assert_eq!(posted[0]["model"], "model-4-alpha-high");
    assert_eq!(posted[0]["thinking"]["type"], "enabled");
    assert_eq!(posted[0]["thinking"]["budget_tokens"], 30_000);
    assert!(posted[0].get("output_config").is_none());
    assert_eq!(posted[1]["model"], "model-4-alpha-high");
    assert_eq!(posted[1]["thinking"]["type"], "adaptive");
    assert_eq!(posted[1]["thinking"]["budget_tokens"], 30_000);
    assert_eq!(posted[2]["model"], "model-4-alpha-low");
    assert_eq!(posted[2]["thinking"]["budget_tokens"], 30_000);
    assert_eq!(posted[2]["output_config"]["effort"], "low");
    assert_eq!(posted[3]["model"], "model-4-alpha-high");
    assert_eq!(posted[3]["thinking"]["type"], "enabled");
    assert_eq!(posted[3]["thinking"]["budget_tokens"], 30_000);
    server.verify().await;
}

#[tokio::test]
async fn d246_refreshed_source_list_drives_the_next_dispatch_and_count() {
    let server = MockServer::start().await;
    let lists = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(move |_request: &WireRequest| {
            let hit = lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let data = if hit == 0 {
                serde_json::json!({
                    "data": [{"id": "model-4-alpha-low"}, {"id": "model-4-alpha-medium"}]
                })
            } else {
                serde_json::json!({
                    "data": [{"id": "model-4-alpha-low"}, {"id": "model-4-alpha-xhigh"}]
                })
            };
            ResponseTemplate::new(200).set_body_json(data)
        })
        .mount(&server)
        .await;
    messages_ok(&server, "model-4-alpha-medium", 1).await;
    messages_ok(&server, "model-4-alpha-low", 1).await;
    count_ok(&server, "model-4-alpha-low", 1).await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-4-alpha\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let (dir, snapshot, _) = relay_snapshot(&config, &host).await;
    let plain = messages_body("model-4-alpha", "");
    let first = snapshot
        .load()
        .dispatch(&plain, "anthropic-messages")
        .await
        .expect("first dispatch");
    assert_eq!(first.upstream_model, "model-4-alpha-medium");
    host.list_models(&config, &dir.path().join("hofund.toml"), "relay")
        .await
        .expect("second list");
    assert_eq!(host.cached("relay").models[0].efforts(), ["low", "xhigh"]);
    let second = snapshot
        .load()
        .dispatch(&plain, "anthropic-messages")
        .await
        .expect("dispatch after refresh");
    assert_eq!(second.upstream_model, "model-4-alpha-low");
    let counted = snapshot
        .load()
        .count(&plain, "anthropic-messages")
        .await
        .expect("count after refresh");
    assert_eq!(counted.tokens, 7);
    assert!(!counted.estimated);
    let calls = server.received_requests().await.expect("requests");
    let posted = calls
        .iter()
        .filter(|call| call.method.as_str() == "POST")
        .map(|call| {
            serde_json::from_slice::<serde_json::Value>(&call.body)
                .expect("body")
                .get("model")
                .and_then(|model| model.as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        posted,
        [
            "model-4-alpha-medium",
            "model-4-alpha-low",
            "model-4-alpha-low"
        ]
    );
    server.verify().await;
}

#[tokio::test]
async fn d246_preset_lists_stay_unfolded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "model-4-alpha-low"}, {"id": "model-4-alpha-high"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"models":[]}"#))
        .expect(1)
        .mount(&server)
        .await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\npreset = \"remote-gateway\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let dir = tempfile::tempdir().expect("temp dir");
    let listed = host
        .list_models(&config, &dir.path().join("hofund.toml"), "relay")
        .await
        .expect("list");
    assert_eq!(
        listed
            .models
            .iter()
            .map(|model| model.id())
            .collect::<Vec<_>>(),
        ["model-4-alpha-low", "model-4-alpha-high"]
    );
    server.verify().await;
}

#[tokio::test]
async fn d246_family_and_old_member_picks_survive_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "model-4-alpha-low"},
                {"id": "model-4-alpha-medium"},
                {"id": "model-4-alpha-high"}
            ]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ))
    .expect("config");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let (dir, _snapshot, _) = relay_snapshot(&config, &host).await;
    host.set_picks(
        "relay",
        &["model-4-alpha".to_owned(), "model-4-alpha-high".to_owned()],
    );
    host.list_models(&config, &dir.path().join("hofund.toml"), "relay")
        .await
        .expect("second list");
    let picks = host.picks("relay");
    assert_eq!(picks.picks(), ["model-4-alpha", "model-4-alpha-high"]);
    assert!(picks.delisted().is_empty());
    assert_eq!(host.source_cached("relay").models.len(), 3);
    assert_eq!(host.cached("relay").models.len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn d246_persisted_source_rows_drive_dispatch_and_count_without_a_list() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MESSAGES_OK))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().expect("state dir");
    let seeded = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .with_state_root(root.path().join(".hofund"));
    seeded.seed_models("relay", &source_family());
    drop(seeded);

    let config = messages_config(&server.uri(), "relay");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .with_state_root(root.path().join(".hofund"));
    let (_dir, snapshot) = open_routes(&config, &host);
    assert_eq!(
        host.cached("relay").models[0].efforts(),
        ["low", "medium", "high"]
    );
    assert_eq!(
        host.sent_id("relay", "model-4-alpha", None),
        "model-4-alpha-medium"
    );
    let plain = messages_body("model-4-alpha", "");
    let dispatched = snapshot
        .load()
        .dispatch(&plain, "anthropic-messages")
        .await
        .expect("dispatch from disk");
    assert_eq!(dispatched.upstream_model, "model-4-alpha-medium");
    let counted = snapshot
        .load()
        .count(&plain, "anthropic-messages")
        .await
        .expect("count from disk");
    assert_eq!(counted.tokens, 7);
    let old = messages_body("model-4-alpha-high", "");
    let old_dispatch = snapshot
        .load()
        .dispatch(&old, "anthropic-messages")
        .await
        .expect("old id from disk");
    assert_eq!(old_dispatch.upstream_model, "model-4-alpha-high");
    let old_count = snapshot
        .load()
        .count(&old, "anthropic-messages")
        .await
        .expect("old id count from disk");
    assert_eq!(old_count.tokens, 7);
    let calls = server.received_requests().await.expect("requests");
    assert!(
        calls.iter().all(|call| call.method.as_str() != "GET"),
        "a restart must not fetch the list"
    );
    let posted = calls
        .iter()
        .filter(|call| call.method.as_str() == "POST")
        .map(|call| {
            serde_json::from_slice::<serde_json::Value>(&call.body)
                .expect("body")
                .get("model")
                .and_then(|model| model.as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        posted,
        [
            "model-4-alpha-medium",
            "model-4-alpha-medium",
            "model-4-alpha-high",
            "model-4-alpha-high"
        ]
    );
}

#[tokio::test]
async fn d246_renamed_provider_uses_the_moved_source_rows() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MESSAGES_OK))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().expect("state dir");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .with_state_root(root.path().join(".hofund"));
    host.seed_models("relay", &source_family());
    let config = messages_config(&server.uri(), "moved");
    let (_dir, snapshot) = open_routes(&config, &host);
    host.rename_provider("relay", "moved").expect("rename");
    assert_eq!(
        host.cached("moved").models[0].efforts(),
        ["low", "medium", "high"]
    );
    assert_eq!(
        host.sent_id("moved", "model-4-alpha", None),
        "model-4-alpha-medium"
    );
    let plain = messages_body("model-4-alpha", "");
    let dispatched = snapshot
        .load()
        .dispatch(&plain, "anthropic-messages")
        .await
        .expect("dispatch after rename");
    assert_eq!(dispatched.upstream_model, "model-4-alpha-medium");
    let counted = snapshot
        .load()
        .count(&plain, "anthropic-messages")
        .await
        .expect("count after rename");
    assert_eq!(counted.tokens, 7);
    let calls = server.received_requests().await.expect("requests");
    let posted = calls
        .iter()
        .filter(|call| call.method.as_str() == "POST")
        .map(|call| {
            serde_json::from_slice::<serde_json::Value>(&call.body)
                .expect("body")
                .get("model")
                .and_then(|model| model.as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(posted, ["model-4-alpha-medium", "model-4-alpha-medium"]);
}

#[tokio::test]
async fn d246_disabled_thinking_uses_the_no_effort_default() {
    let server = MockServer::start().await;
    messages_ok(&server, "model-4-alpha-medium", 1).await;
    count_ok(&server, "model-4-alpha-medium", 1).await;
    let config = messages_config(&server.uri(), "relay");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    host.seed_models("relay", &source_family());
    let (_dir, snapshot) = open_routes(&config, &host);
    let request = messages_body("model-4-alpha", r#","thinking":{"type":"disabled"}"#);
    assert!(request.params.effort.is_none());
    let dispatched = snapshot
        .load()
        .dispatch(&request, "anthropic-messages")
        .await
        .expect("disabled dispatch");
    assert_eq!(dispatched.upstream_model, "model-4-alpha-medium");
    let counted = snapshot
        .load()
        .count(&request, "anthropic-messages")
        .await
        .expect("disabled count");
    assert_eq!(counted.tokens, 7);
    let calls = server.received_requests().await.expect("requests");
    let posted = calls
        .iter()
        .filter(|call| call.method.as_str() == "POST")
        .map(|call| serde_json::from_slice::<serde_json::Value>(&call.body).expect("body"))
        .collect::<Vec<_>>();
    assert_eq!(posted.len(), 2);
    assert!(posted.iter().all(|body| {
        body["model"] == "model-4-alpha-medium" && body["thinking"]["type"] == "disabled"
    }));
    server.verify().await;
}
