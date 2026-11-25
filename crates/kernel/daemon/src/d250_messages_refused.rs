//! A custom relay whose group refuses Messages answers through Chat (D-250).

use std::sync::Arc;

use hofund_plugin_api::{Completion, ContentPart, Request};
use hofund_router::{CooldownPolicy, Cooldowns};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REFUSAL: &str = r#"{"type":"error","error":{"type":"permission_error","message":"This group does not allow /v1/messages dispatch"}}"#;
const CHAT_OK: &str = r#"{"id":"c1","model":"m","choices":[{"message":{"role":"assistant","content":"chat ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

fn assert_chat_ok(completion: Completion) {
    let Completion::Unary(response) = completion else {
        panic!("a unary Chat answer is translated to a unary IR response");
    };
    assert!(
        response
            .message
            .content
            .iter()
            .any(|part| { matches!(part, ContentPart::Text(text) if text.text == "chat ok") })
    );
}

#[tokio::test]
async fn d250_custom_messages_relay_uses_chat_once_and_remembers_for_all_models() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string(REFUSAL))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_OK))
        .expect(4)
        .mount(&server)
        .await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-a\"\nupstreams = [\"relay\"]\n[[routes]]\nmodel = \"model-b\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let registry: Arc<dyn super::DaemonRegistry> = Arc::new(super::RegistryAdapter);
    let dir = tempfile::tempdir().expect("temp dir");
    let super::Built { built, limits, .. } = super::providers(
        &config,
        &dir.path().join("hofund.toml"),
        None,
        super::Mode::Serve,
        None,
        None,
        None,
        &registry,
    )
    .expect("providers");
    let (routes, policy) = super::route_plan(&config, None, Some(&built), &limits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let (snapshot, table) = super::membership::open_with_table(
        &built,
        routes.clone(),
        policy.clone(),
        Arc::clone(&cooldowns),
    )
    .expect("router");

    for model in ["model-a", "model-b", "model-a"] {
        let routed = snapshot
            .load()
            .dispatch(&Request::new(model), "anthropic-messages")
            .await
            .expect("Chat answers");
        assert_eq!(routed.upstream_id, "relay");
        assert_eq!(routed.protocol, "openai-chat");
        assert_chat_ok(routed.completion);
    }

    table
        .replace(&built, routes, policy)
        .expect("unchanged config republishes");
    let routed = snapshot
        .load()
        .dispatch(&Request::new("model-b"), "anthropic-messages")
        .await
        .expect("republish keeps the unfit mark");
    assert_eq!(routed.protocol, "openai-chat");
    assert_chat_ok(routed.completion);
    assert!(
        cooldowns
            .cooling(&super::membership::member_id("relay"))
            .is_none()
    );
    server.verify().await;
}

#[test]
fn d250_provider_test_marks_only_the_api_refusal_as_unserved() {
    for (message, unserved) in [(REFUSAL, true), ("key rejected", false)] {
        let error = super::DaemonError::new(message).with_kind(super::ErrorKind::Upstream {
            kind: hofund_provider::ProviderErrorKind::Status,
            status: Some(403),
        });
        let result = super::control::test_result(super::models::Timed {
            model: "model-a".to_owned(),
            latency: std::time::Duration::from_millis(1),
            result: Err(error),
        });
        assert!(!result.ok);
        assert_eq!(result.unserved, unserved);
        assert_eq!(result.status, Some(403));
        assert_eq!(result.error.as_ref().expect("error").message, message);
    }
}

#[tokio::test]
async fn d250_real_messages_probe_reports_not_served() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"data":[{"id":"model-a","object":"model"}]}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string(REFUSAL))
        .expect(1)
        .mount(&server)
        .await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n",
        server.uri()
    ))
    .expect("config");
    let dir = tempfile::tempdir().expect("temp dir");
    let host = super::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    let error = host
        .probe(
            &config,
            &dir.path().join("hofund.toml"),
            "relay",
            Some("model-a"),
        )
        .await
        .expect_err("the API refuses the probe");
    let result = super::control::test_result(super::models::Timed {
        model: "model-a".to_owned(),
        latency: std::time::Duration::from_millis(1),
        result: Err(error),
    });
    assert!(result.unserved);
    assert_eq!(result.status, Some(403));
    assert!(
        result
            .error
            .as_ref()
            .expect("error")
            .message
            .contains("does not allow /v1/messages dispatch")
    );
    server.verify().await;
}

#[tokio::test]
async fn d250_chat_success_clears_a_model_rest_on_the_primary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string(REFUSAL))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAT_OK))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, snapshot, cooldowns) = relay(&server.uri()).await;
    let primary = super::membership::member_id("relay");
    cooldowns.record_failure_for_model(
        &primary,
        "model-a",
        &hofund_plugin_api::PluginError::status_error(429, "slow").with_model_scope(),
    );
    snapshot
        .load()
        .dispatch(&Request::new("model-a"), "anthropic-messages")
        .await
        .expect("Chat answers");
    assert!(cooldowns.cooling_for_model(&primary, "model-a").is_none());
    let alternate = super::membership::alternate_member_id(&primary, "openai-chat");
    assert!(cooldowns.cooling_for_model(&alternate, "model-a").is_none());
    server.verify().await;
}

#[tokio::test]
async fn d250_chat_rate_limit_rests_the_primary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string(REFUSAL))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, snapshot, cooldowns) = relay(&server.uri()).await;
    let primary = super::membership::member_id("relay");
    match snapshot
        .load()
        .dispatch(&Request::new("model-a"), "anthropic-messages")
        .await
    {
        Ok(_) => panic!("Chat is rate limited"),
        Err(error) => {
            assert!(matches!(error, hofund_router::RouterError::Upstream { .. }));
        }
    }
    assert!(cooldowns.cooling(&primary).is_some());
    let alternate = super::membership::alternate_member_id(&primary, "openai-chat");
    assert!(cooldowns.cooling(&alternate).is_none());
    server.verify().await;
}

async fn relay(base_url: &str) -> (tempfile::TempDir, hofund_router::Snapshot, Arc<Cooldowns>) {
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{base_url}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"model-a\"\nupstreams = [\"relay\"]\n",
    ))
    .expect("config");
    let registry: Arc<dyn super::DaemonRegistry> = Arc::new(super::RegistryAdapter);
    let dir = tempfile::tempdir().expect("temp dir");
    let super::Built { built, limits, .. } = super::providers(
        &config,
        &dir.path().join("hofund.toml"),
        None,
        super::Mode::Serve,
        None,
        None,
        None,
        &registry,
    )
    .expect("providers");
    let (routes, policy) = super::route_plan(&config, None, Some(&built), &limits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let (snapshot, _) =
        super::membership::open_with_table(&built, routes, policy, Arc::clone(&cooldowns))
            .expect("router");
    (dir, snapshot, cooldowns)
}
