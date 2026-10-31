//! Persistent model state and configured-instance reuse against local mocks.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use hofund_daemon::ModelsHost;
use hofund_plugin_api::{
    Completion, ListQuery, ModelList, ModelSpec, PluginError, ProbeQuery, ProbeReport, ProtocolId,
    Request, UpstreamProvider,
};
use hofund_provider::ModelPlan;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_host() -> ModelsHost {
    ModelsHost::new(Duration::from_secs(2), Duration::from_secs(2))
}

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<std::path::Path> for Scratch {
    fn as_ref(&self) -> &std::path::Path {
        self.0.path()
    }
}

fn scratch(_test: &str) -> Scratch {
    Scratch(tempfile::tempdir().expect("directory"))
}

struct CountingUpstream {
    lists: AtomicUsize,
    probes: AtomicUsize,
}

#[async_trait]
impl UpstreamProvider for CountingUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Err(PluginError::unsupported("not used by this test"))
    }

    async fn list_models(&self, _query: &ListQuery) -> Result<ModelList, PluginError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(ModelList::new(
            vec![ModelSpec::new("registered-model", None).expect("model")],
            None,
        ))
    }

    async fn probe(&self, _query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Ok(ProbeReport::new("registered"))
    }
}

#[tokio::test]
async fn registered_instance_is_used_for_list_and_probe() {
    let directory = scratch("registered");
    let config_path = directory.join("hofund.toml");
    let config = hofund_config::load(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .expect("config");
    let upstream = Arc::new(CountingUpstream {
        lists: AtomicUsize::new(0),
        probes: AtomicUsize::new(0),
    });
    let mut host = make_host();
    host.register_instance("chat", upstream.clone(), "http://127.0.0.1:9/v1");

    let listed = host
        .list_models(&config, &config_path, "chat")
        .await
        .expect("registered list");
    assert_eq!(listed.models[0].id(), "registered-model");
    let probed = host
        .probe(&config, &config_path, "chat", Some("registered-model"))
        .await
        .expect("registered probe");
    assert_eq!(probed.report.snippet(), "registered");
    assert_eq!(upstream.lists.load(Ordering::SeqCst), 2);
    assert_eq!(upstream.probes.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn absent_or_stale_registry_entry_uses_a_fresh_build() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "fresh-model"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let directory = scratch("fallback");
    let config_path = directory.join("hofund.toml");
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
        server.uri()
    ))
    .expect("config");
    let absent = make_host()
        .list_models(&config, &config_path, "chat")
        .await
        .expect("absent registry entry");
    assert_eq!(absent.models[0].id(), "fresh-model");

    let upstream = Arc::new(CountingUpstream {
        lists: AtomicUsize::new(0),
        probes: AtomicUsize::new(0),
    });
    let mut stale = make_host();
    stale.register_instance("chat", upstream.clone(), "http://127.0.0.1:9/v1");
    let fallback = stale
        .list_models(&config, &config_path, "chat")
        .await
        .expect("stale registry entry");
    assert_eq!(fallback.models[0].id(), "fresh-model");
    assert_eq!(upstream.lists.load(Ordering::SeqCst), 0);
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn fix_v1_on_another_wire_does_not_change_the_configured_base() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chat/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/chat/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "chat-model"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/anthropic/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-model"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let base = server.uri();
    let host = make_host().with_model_plan(
        "messages",
        ModelPlan {
            chat: format!("{base}/chat"),
            anthropic: format!("{base}/anthropic/v1"),
            ..ModelPlan::default()
        },
    );
    let config = hofund_config::load(
        "[[providers]]\nid = \"messages\"\npreset = \"deepseek\"\nprotocol = \"anthropic-messages\"\napi_key = \"test-key\"\n",
    )
    .expect("config");
    let config_path = scratch("per-wire").join("hofund.toml");
    for _ in 0..2 {
        let listed = host
            .list_models(&config, &config_path, "messages")
            .await
            .expect("configured Messages base");
        assert_eq!(listed.corrected_base, None);
    }
    server.verify().await;
}

#[tokio::test]
async fn persisted_chat_fix_v1_survives_restart_and_reaches_the_router() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "chat-model"}]
        })))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let directory = scratch("router-restart");
    let config_path = directory.join("hofund.toml");
    let source = format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"chat\"]\n",
        server.uri()
    );
    std::fs::write(&config_path, &source).expect("config");
    let config = hofund_config::load(&source).expect("config");
    let state_root = directory.join(".hofund");
    let first = make_host()
        .with_state_root(&state_root)
        .list_models(&config, &config_path, "chat")
        .await
        .expect("learned base");
    assert_eq!(
        first.corrected_base.as_deref(),
        Some(format!("{}/v1", server.uri()).as_str())
    );
    let restarted = make_host()
        .with_state_root(&state_root)
        .list_models(&config, &config_path, "chat")
        .await
        .expect("reloaded base");
    assert_eq!(restarted.models[0].id(), "chat-model");
    let pinned = hofund_config::load(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
        server.uri()
    ))
    .expect("versioned config");
    let versioned = make_host()
        .with_state_root(&state_root)
        .list_models(&pinned, &config_path, "chat")
        .await
        .expect("user-written versioned base");
    assert_eq!(versioned.corrected_base, None);
    assert_eq!(std::fs::read_to_string(&config_path).expect("TOML"), source);
    let state_files = std::fs::read_dir(state_root.join("host/models"))
        .expect("state directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("state files");
    assert_eq!(state_files.len(), 1);
    let state = std::fs::read_to_string(state_files[0].path()).expect("state JSON");
    assert!(state.contains(&format!("\"chat\": \"{}/v1\"", server.uri())));

    let running = hofund_daemon::start_for_test(&config_path)
        .await
        .expect("daemon restart");
    let token = std::fs::read_to_string(directory.join("hofund.token")).expect("token");
    let response = support::client()
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .bearer_auth(token.trim())
        .json(&json!({"model": "demo", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("route request");
    assert_eq!(response.status(), 200);
    support::shutdown(running).await;
    server.verify().await;
    let _ = std::fs::remove_dir_all(directory);
}
