//! Address-added relay effort ids, from the list through the posted body.

use std::sync::Arc;

use hofund_plugin_api::{Effort, Request};
use hofund_router::{CooldownPolicy, Cooldowns, Snapshot};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request as WireRequest, ResponseTemplate};

const CHAT_OK: &str = r#"{"id":"c1","model":"m","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

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
