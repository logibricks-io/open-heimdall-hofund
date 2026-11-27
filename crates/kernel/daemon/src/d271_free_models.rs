use std::sync::Arc;

use hofund_plugin_api::{ContentPart, Request};
use hofund_router::{CooldownPolicy, Cooldowns};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

const CHAT_OK: &str = r#"{"id":"c1","model":"m","choices":[{"message":{"role":"assistant","content":"chat ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

struct ByModel {
    limited: &'static str,
    body: &'static str,
    retry: bool,
}

impl Respond for ByModel {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("request json");
        if body["model"].as_str() == Some(self.limited) {
            let response = ResponseTemplate::new(429).set_body_string(self.body);
            if self.retry {
                response.insert_header("retry-after", "600")
            } else {
                response
            }
        } else {
            ResponseTemplate::new(200).set_body_string(CHAT_OK)
        }
    }
}

async fn snapshot(base_url: &str) -> (tempfile::TempDir, hofund_router::Snapshot, Arc<Cooldowns>) {
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\nprotocol = \"openai-chat\"\nbase_url = \"{base_url}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"paid-model\"\nupstreams = [\"relay\"]\n[[routes]]\nmodel = \"free-model\"\nupstreams = [\"relay\"]\n",
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
    let (snapshot, _table) =
        super::membership::open_with_table(&built, routes, policy, Arc::clone(&cooldowns))
            .expect("router");
    (dir, snapshot, cooldowns)
}

async fn assert_chat_ok(completion: hofund_plugin_api::Completion) {
    match completion {
        hofund_plugin_api::Completion::Unary(response) => assert!(
            response
                .message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::Text(text) if text.text == "chat ok"))
        ),
        hofund_plugin_api::Completion::Raw(mut body) => {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                bytes.extend_from_slice(&chunk.expect("body chunk"));
            }
            assert!(
                String::from_utf8_lossy(&bytes).contains("chat ok"),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }
        _ => panic!("stream response"),
    }
}

#[tokio::test]
async fn d271_model_refusals_leave_the_other_model_served_through_daemon_dispatch() {
    for (limited, served, body, retry, posts) in [
        (
            "paid-model",
            "free-model",
            r#"{"error":{"message":"usage limit reached (weekly)","type":"rate_limit_error"}}"#,
            true,
            2,
        ),
        (
            "free-model",
            "paid-model",
            r#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error"}}"#,
            true,
            3,
        ),
        ("free-model", "paid-model", "", false, 3),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                    {"id": "paid-model", "go": {"monthly_usage": {"go": 200}}},
                    {"id": "free-model", "go": {"monthly_usage": null}}
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ByModel {
                limited,
                body,
                retry,
            })
            .expect(posts)
            .mount(&server)
            .await;
        let (_dir, snapshot, cooldowns) = snapshot(&server.uri()).await;
        let member = super::membership::member_id("relay");
        let refused = snapshot
            .load()
            .dispatch(&Request::new(limited), "openai-chat")
            .await;
        assert!(refused.is_err(), "{limited} unexpectedly answered");
        assert!(
            cooldowns.cooling(&member).is_none(),
            "{limited}: member rest {:?}; model rest {:?}",
            cooldowns.cooling(&member),
            cooldowns.cooling_for_model(&member, limited),
        );
        assert!(cooldowns.cooling_for_model(&member, limited).is_some());
        assert!(cooldowns.cooling_for_model(&member, served).is_none());

        let served_requests = if limited == "free-model" { 2 } else { 1 };
        for _ in 0..served_requests {
            let routed = snapshot
                .load()
                .dispatch(&Request::new(served), "openai-chat")
                .await
                .expect("other model answers");
            assert_chat_ok(routed.completion).await;
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn d271_paid_ordinary_rate_limit_rests_the_whole_key_through_daemon_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                {"id": "paid-model", "go": {"monthly_usage": {"go": 200}}},
                {"id": "free-model", "go": {"monthly_usage": null}}
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "600")
                .set_body_string(
                    r#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error"}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, snapshot, cooldowns) = snapshot(&server.uri()).await;
    let member = super::membership::member_id("relay");
    let refused = snapshot
        .load()
        .dispatch(&Request::new("paid-model"), "openai-chat")
        .await;
    assert!(refused.is_err(), "paid ordinary rate limit answered");
    assert!(
        cooldowns.cooling(&member).is_some(),
        "paid ordinary rate limit did not rest the key"
    );
    server.verify().await;
}
