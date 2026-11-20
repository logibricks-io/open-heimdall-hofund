//! Preferred APIs and catalog entries reach every key and survive a restart.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{ProtocolId, Request};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn registry() -> Arc<dyn super::DaemonRegistry> {
    Arc::new(super::RegistryAdapter)
}

fn reply(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

const RESPONSES_REPLY: &str = r#"{"id":"r1","object":"response","status":"completed","model":"m","output":[{"type":"message","id":"o1","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#;

fn ask(model: &str) -> Request {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    }))
    .expect("request")
}

#[tokio::test]
async fn d241_every_key_keeps_the_preferred_api_across_a_restart() {
    let server = MockServer::start().await;
    let models = serde_json::json!({"data": [{
        "id": "north/model-a",
        "preferred_api": "/v1/responses",
        "native_endpoints": ["/v1/chat/completions"]
    }]});
    let catalog = serde_json::json!({"models": [{
        "slug": "north/model-a",
        "supported_reasoning_levels": [{"effort": "high"}],
        "shell_type": "unified_exec",
        "base_instructions": "REMOTE",
        "experimental_supported_tools": [{"name": "kept-tool"}]
    }]});
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(reply(&models.to_string()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/codex/models"))
        .respond_with(reply(&catalog.to_string()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(reply(RESPONSES_REPLY))
        .expect(2)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("hofund.toml");
    let config = hofund_config::load(&format!(
        r#"
        [[providers]]
        id = "office"
        preset = "remote-gateway"
        base_url = "{base}"
        api_key = "key-one"
        [providers.models]
        refresh = "1h"
        [[providers.keys]]
        key = "key-two"
        "#,
        base = server.uri()
    ))
    .expect("config");
    let registry = registry();
    let host = super::ModelsHost::new(Duration::from_secs(2), Duration::from_secs(5))
        .with_state_root(dir.path());
    let built = super::providers(
        &config,
        &path,
        None,
        super::Mode::Serve,
        Some(&host),
        None,
        None,
        &registry,
    )
    .expect("providers");
    assert_eq!(built.instances.len(), 2);
    host.install_upstreams(&built.instances);
    host.list_models(&config, &path, "office")
        .await
        .expect("list");
    for instance in built.instances.values() {
        assert_eq!(
            instance
                .upstream
                .learned_preferred("north/model-a")
                .as_deref(),
            Some("responses")
        );
        let entry = instance
            .upstream
            .learned_catalog_entry("north/model-a")
            .expect("catalog entry");
        assert!(entry.contains("kept-tool"), "{entry}");
    }
    let second = built
        .instances
        .values()
        .find(|instance| !instance.model_list)
        .expect("second key");
    second
        .upstream
        .execute(ask("north/model-a"), ProtocolId::ANTHROPIC_MESSAGES)
        .await
        .expect("second key asks the preferred API");

    let host = super::ModelsHost::new(Duration::from_secs(2), Duration::from_secs(5))
        .with_state_root(dir.path());
    let built = super::providers(
        &config,
        &path,
        None,
        super::Mode::Serve,
        Some(&host),
        None,
        None,
        &registry,
    )
    .expect("restarted providers");
    host.install_upstreams(&built.instances);
    let asked = host.refresh_due(&config, &path, SystemTime::now()).await;
    assert!(
        asked.is_empty(),
        "a fresh saved list must not be fetched again"
    );
    for instance in built.instances.values() {
        assert_eq!(
            instance
                .upstream
                .learned_preferred("north/model-a")
                .as_deref(),
            Some("responses")
        );
        let entry = instance
            .upstream
            .learned_catalog_entry("north/model-a")
            .expect("seeded catalog entry");
        assert!(entry.contains("kept-tool"), "{entry}");
    }
    let second = built
        .instances
        .values()
        .find(|instance| !instance.model_list)
        .expect("restarted second key");
    second
        .upstream
        .execute(ask("north/model-a"), ProtocolId::ANTHROPIC_MESSAGES)
        .await
        .expect("restarted key still asks the preferred API");
    server.verify().await;
}
