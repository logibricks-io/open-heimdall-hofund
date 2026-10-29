//! Host model aggregation against local mock upstreams.

use std::path::PathBuf;
use std::time::Duration;

use hofund_daemon::ModelsHost;
use hofund_plugin_api::ListKind;
use hofund_provider::ModelPlan;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

fn load_config(source: &str) -> hofund_config::Config {
    hofund_config::load(source).expect("config")
}

struct ConfigPath {
    _guard: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for ConfigPath {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

fn make_config_path(_test: &str) -> ConfigPath {
    let guard = tempfile::tempdir().expect("temp dir");
    let path = guard.path().join("hofund.toml");
    ConfigPath {
        _guard: guard,
        path,
    }
}

fn make_host() -> ModelsHost {
    ModelsHost::new(Duration::from_secs(2), Duration::from_secs(2))
}

fn plan(chat: &str, responses: &str, anthropic: &str, generative: &str) -> ModelPlan {
    ModelPlan {
        chat: chat.to_owned(),
        responses: responses.to_owned(),
        anthropic: anthropic.to_owned(),
        generative: generative.to_owned(),
        ..ModelPlan::default()
    }
}

fn ids(models: &hofund_daemon::ProviderModels) -> Vec<&str> {
    models.models.iter().map(|model| model.id()).collect()
}

#[derive(Clone, Copy)]
enum AuthMode {
    Both,
    XApiOnly,
    BearerOnly,
}

impl Match for AuthMode {
    fn matches(&self, request: &Request) -> bool {
        let x_api = request.headers.contains_key("x-api-key");
        let bearer = request
            .headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer test-key");
        match self {
            Self::Both => x_api && bearer,
            Self::XApiOnly => x_api && !bearer,
            Self::BearerOnly => bearer && !x_api,
        }
    }
}

#[tokio::test]
async fn cross_wire_merge_unions_tags_and_text_only_wins() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chat/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "shared", "display_name": "chat first", "modalities": {"input": ["text"]}},
                {"id": "drawn", "kind": "image"},
                {"id": "method-only"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/generative/models"))
        .and(query_param("pageSize", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "models/shared", "displayName": "gemini second", "inputTokenLimit": 1000},
                {"name": "models/drawn", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/method-only", "supportedGenerationMethods": ["embedContent"]}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = server.uri();
    let host = make_host().with_model_plan(
        "google",
        plan(
            &format!("{base}/chat"),
            "",
            "",
            &format!("{base}/generative"),
        ),
    );
    let config = load_config(
        "[[providers]]\nid = \"google\"\npreset = \"google\"\nprotocol = \"openai-chat\"\napi_key = \"test\"\n",
    );
    let new_config_path = make_config_path("merge");
    let models = host
        .list_models(&config, &new_config_path, "google")
        .await
        .expect("merged list");
    assert_eq!(ids(&models), ["shared", "drawn"]);
    assert_eq!(models.models[0].display_name(), Some("chat first"));
    assert_eq!(models.models[0].image_input(), Some(false));
    assert_eq!(models.models[0].context(), 1000);
    assert!(models.models[1].draws());
}

#[tokio::test]
async fn models_url_env_is_the_only_request_and_query_and_key_are_redacted() {
    const ENV: &str = "HOFUND_TEST_MODELS_URL";
    let new_config_path = make_config_path("models-url");
    if std::env::var_os(ENV).is_some() {
        let config = load_config(
            "[[providers]]\nid = \"custom\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key = \"APIKEY-SENTINEL\"\n[providers.models]\nurl = \"env:HOFUND_TEST_MODELS_URL\"\n",
        );
        let error = make_host()
            .list_models(&config, &new_config_path, "custom")
            .await
            .expect_err("refused exact URL");
        let text = error.to_string();
        assert!(!text.contains("URL-SECRET-SENTINEL"), "{text}");
        assert!(!text.contains("APIKEY-SENTINEL"), "{text}");
        assert!(!text.contains("key="), "{text}");
        return;
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .and(query_param("key", "URL-SECRET-SENTINEL"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("APIKEY-SENTINEL URL-SECRET-SENTINEL"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(0)
        .mount(&server)
        .await;
    let url = format!("{}/exact/models?key=URL-SECRET-SENTINEL", server.uri());
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env(ENV, url)
        .arg("models_url_env_is_the_only_request_and_query_and_key_are_redacted")
        .arg("--exact")
        .arg("--nocapture")
        .status()
        .expect("child test");
    assert!(status.success());
    server.verify().await;
}

#[tokio::test]
async fn public_drawers_append_only_new_image_rows_and_failure_adds_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/keyed/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "chat"}, {"id": "gpt-image-existing"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"model_id": "gpt-image-new", "model_name": "New image", "types": "image_generation", "output_modalities": "image", "input_modalities": "text,image"},
                {"model_id": "gpt-image-existing", "types": "image_generation", "output_modalities": "image"},
                {"model_id": "chat", "types": "text", "output_modalities": "text"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = server.uri();
    let source = format!(
        "[[providers]]\nid = \"mix\"\npreset = \"aihubmix\"\nprotocol = \"openai-chat\"\napi_key = \"test\"\n[providers.models]\nurl = \"{base}/keyed/models\"\n"
    );
    let config = load_config(&source);
    let host = make_host().with_public_drawers_url(format!("{base}/public/models"));
    let new_config_path = make_config_path("drawers");
    let models = host
        .list_models(&config, &new_config_path, "mix")
        .await
        .expect("drawers");
    assert_eq!(
        ids(&models),
        ["chat", "gpt-image-existing", "gpt-image-new"]
    );
    assert!(models.models[2].draws());
    assert_eq!(models.models[2].image_input(), Some(true));

    let host = make_host().with_public_drawers_url(format!("{base}/missing-feed"));
    let failure_path = make_config_path("drawers-failure");
    let models = host
        .list_models(&config, &failure_path, "mix")
        .await
        .expect("feed failure is ignored");
    assert_eq!(ids(&models), ["chat", "gpt-image-existing"]);
}

#[tokio::test]
async fn custom_base_fixes_v1_and_reuses_it_but_versioned_base_is_untouched() {
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
            "data": [{"id": "gpt"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"custom\"\nprotocol = \"openai-chat\"\nbase_url = \"{}\"\napi_key = \"test\"\n",
        server.uri()
    ));
    let host = make_host();
    let new_config_path = make_config_path("fix-v1");
    let models = host
        .list_models(&config, &new_config_path, "custom")
        .await
        .expect("fallback");
    assert_eq!(
        models.corrected_base.as_deref(),
        Some(format!("{}/v1", server.uri()).as_str())
    );
    host.list_models(&config, &new_config_path, "custom")
        .await
        .expect("learned base");
    server.verify().await;

    let versioned = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt"}]
        })))
        .expect(1)
        .mount(&versioned)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"versioned\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test\"\n",
        versioned.uri()
    ));
    let versioned_path = make_config_path("versioned");
    let models = host
        .list_models(&config, &versioned_path, "versioned")
        .await
        .expect("versioned");
    assert_eq!(models.corrected_base, None);
}

#[tokio::test]
async fn openrouter_skips_anthropic_with_chat_and_rejects_only_base_namespace() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chat/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "openai/gpt"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/anthropic/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "anthropic/claude"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = server.uri();
    let chat = format!("{base}/chat");
    let anthropic = format!("{base}/anthropic");
    let host = make_host()
        .with_model_plan("router", plan(&chat, "", &anthropic, ""))
        .with_base_host(&anthropic, "openrouter.ai");
    let config = load_config(
        "[[providers]]\nid = \"router\"\npreset = \"openrouter\"\nprotocol = \"openai-chat\"\napi_key = \"test\"\n",
    );
    let new_config_path = make_config_path("openrouter-skip");
    let models = host
        .list_models(&config, &new_config_path, "router")
        .await
        .expect("chat list");
    assert_eq!(ids(&models), ["openai/gpt"]);

    let config = load_config(
        "[[providers]]\nid = \"router\"\npreset = \"openrouter\"\nprotocol = \"anthropic-messages\"\napi_key = \"test\"\n",
    );
    let host = make_host()
        .with_model_plan("router", plan("", "", &anthropic, ""))
        .with_base_host(&anthropic, "openrouter.ai");
    let only_path = make_config_path("openrouter-only");
    let error = host
        .list_models(&config, &only_path, "router")
        .await
        .expect_err("namespaced list");
    assert!(
        error.to_string().contains("https://openrouter.ai/api/v1"),
        "{error}"
    );
}

#[tokio::test]
async fn static_region_live_and_only_fallback_follow_preset_policy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/static/models"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/live/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "live-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/mantle/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "mantle-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = server.uri();
    let config = load_config(
        "[[providers]]\nid = \"static\"\npreset = \"baidu-qianfan\"\nprotocol = \"openai-chat\"\n[[providers]]\nid = \"live\"\npreset = \"baidu-qianfan\"\nprotocol = \"openai-chat\"\n[[providers]]\nid = \"bedrock\"\npreset = \"bedrock\"\nprotocol = \"openai-chat\"\n[[providers]]\nid = \"mantle\"\npreset = \"bedrock-mantle\"\nprotocol = \"openai-chat\"\n[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\n[providers.options]\nproject = \"my-project\"\n",
    );
    let static_plan = ModelPlan {
        no_list: true,
        models: vec!["static-model".to_owned()],
        ..plan(&format!("{base}/static"), "", "", "")
    };
    let live_plan = ModelPlan {
        no_list: true,
        lists: true,
        ..plan(&format!("{base}/live"), "", "", "")
    };
    let host = make_host()
        .with_model_plan("static", static_plan.clone())
        .with_model_plan("bedrock", static_plan)
        .with_model_plan("live", live_plan)
        .with_model_plan("mantle", plan(&format!("{base}/mantle"), "", "", ""));
    let new_config_path = make_config_path("kinds");
    let static_models = host
        .list_models(&config, &new_config_path, "static")
        .await
        .expect("static");
    assert_eq!(static_models.kind, ListKind::Catalog);
    assert_eq!(ids(&static_models), ["static-model"]);
    let bedrock = host
        .list_models(&config, &new_config_path, "bedrock")
        .await
        .expect("bedrock static");
    assert_eq!(ids(&bedrock), ["static-model"]);
    let vertex = host
        .list_models(&config, &new_config_path, "vertex")
        .await
        .expect("vertex static");
    assert_eq!(vertex.kind, ListKind::Catalog);
    assert!(!vertex.models.is_empty());
    let live = host
        .list_models(&config, &new_config_path, "live")
        .await
        .expect("region live");
    assert_eq!(live.kind, ListKind::Live);
    assert_eq!(ids(&live), ["live-model"]);
    let mantle = host
        .list_models(&config, &new_config_path, "mantle")
        .await
        .expect("mantle live");
    assert_eq!(ids(&mantle), ["mantle-model"]);
    server.verify().await;
}

#[tokio::test]
async fn only_filter_uses_static_fallback_and_catalog_limits() {
    struct Limits;
    impl hofund_daemon::ModelLimits for Limits {
        fn limits(&self, model: &str) -> (u64, u64) {
            assert_eq!(model, "glm");
            (32_000, 4096)
        }
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "other/model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let host = make_host()
        .with_catalog_limits(std::sync::Arc::new(Limits))
        .with_model_plan(
            "cline",
            ModelPlan {
                only: "cline-pass/".to_owned(),
                models: vec!["cline-pass/glm".to_owned()],
                ..plan(&format!("{}/v1", server.uri()), "", "", "")
            },
        );
    let config = load_config(
        "[[providers]]\nid = \"cline\"\npreset = \"clinepass\"\nprotocol = \"openai-chat\"\napi_key = \"test\"\n",
    );
    let new_config_path = make_config_path("only");
    let models = host
        .list_models(&config, &new_config_path, "cline")
        .await
        .expect("static fallback");
    assert_eq!(ids(&models), ["cline-pass/glm"]);
    assert_eq!(models.models[0].context(), 32_000);
    assert_eq!(models.models[0].max_output(), 4096);
}

#[tokio::test]
async fn azure_deployments_and_fallback_are_host_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/deployments"))
        .and(query_param("api-version", "2022-12-01"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "base-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"azure\"\npreset = \"azure\"\nprotocol = \"openai-chat\"\nbase_url = \"{}\"\napi_key = \"test\"\n",
        server.uri()
    ));
    let new_config_path = make_config_path("azure");
    let models = make_host()
        .list_models(&config, &new_config_path, "azure")
        .await
        .expect("base-model fallback");
    assert_eq!(models.kind, ListKind::Deployments);
    assert!(models.base_model_fallback);
    assert_eq!(ids(&models), ["base-model"]);
}

#[tokio::test]
async fn every_failed_base_gets_the_manual_entry_suffix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"broken\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test\"\n",
        server.uri()
    ));
    let new_config_path = make_config_path("all-fail");
    let error = make_host()
        .list_models(&config, &new_config_path, "broken")
        .await
        .expect_err("all failed");
    assert!(
        error
            .to_string()
            .ends_with("type its model ids in by hand, or give the URL its list is at"),
        "{error}"
    );
}

#[tokio::test]
async fn identical_chat_and_responses_lists_are_fetched_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "shared"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri());
    let host = make_host().with_model_plan("openai", plan(&base, &base, "", ""));
    let config = load_config(
        "[[providers]]\nid = \"openai\"\npreset = \"openai\"\nprotocol = \"openai-chat\"\napi_key = \"same-key\"\n",
    );
    let new_config_path = make_config_path("dedupe");
    let models = host
        .list_models(&config, &new_config_path, "openai")
        .await
        .expect("deduplicated list");
    assert_eq!(ids(&models), ["shared"]);
    server.verify().await;
}

// reference: internal/provider/models.go:687-696 and 777-781
#[tokio::test]
async fn different_bases_answering_at_one_list_url_are_merged_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/relay/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/relay/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "shared"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let base = server.uri();
    let host = make_host().with_model_plan(
        "shared",
        plan(
            &format!("{base}/relay"),
            "",
            &format!("{base}/relay/v1"),
            "",
        ),
    );
    let config = load_config(
        "[[providers]]\nid = \"shared\"\npreset = \"deepseek\"\nprotocol = \"openai-chat\"\napi_key = \"same-key\"\n",
    );
    let new_config_path = make_config_path("answered-dedupe");
    let models = host
        .list_models(&config, &new_config_path, "shared")
        .await
        .expect("deduplicated response");
    assert_eq!(ids(&models), ["shared"]);
    server.verify().await;
}

// reference: internal/provider/models.go:651-654
#[tokio::test]
async fn models_url_learns_and_reuses_bearer_on_messages_only_providers() {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"messages\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{0}/v1\"\napi_key = \"test-key\"\n[providers.models]\nurl = \"{0}/exact/models\"\n",
        server.uri()
    ));
    let host = make_host();
    let new_config_path = make_config_path("models-url-auth");
    for _ in 0..2 {
        let models = host
            .list_models(&config, &new_config_path, "messages")
            .await
            .expect("custom list");
        assert_eq!(ids(&models), ["claude"]);
    }
    server.verify().await;
}

// reference: internal/catalog/live.go:542
#[tokio::test]
async fn models_url_rejects_a_non_200_success_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact/models"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"custom\"\nprotocol = \"openai-chat\"\nbase_url = \"{0}/v1\"\napi_key = \"test-key\"\n[providers.models]\nurl = \"{0}/exact/models\"\n",
        server.uri()
    ));
    let new_config_path = make_config_path("models-url-status");
    let error = make_host()
        .list_models(&config, &new_config_path, "custom")
        .await
        .expect_err("204 is not a list");
    assert!(error.to_string().contains("HTTP 204"), "{error}");
    server.verify().await;
}

#[tokio::test]
async fn custom_azure_resource_host_resolves_to_deployments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "deployment"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let base = server.uri();
    let host = make_host().with_base_host(&base, "resource.openai.azure.com");
    let config = load_config(&format!(
        "[[providers]]\nid = \"azure-custom\"\nprotocol = \"openai-chat\"\nbase_url = \"{base}\"\napi_key = \"test\"\n"
    ));
    let new_config_path = make_config_path("azure-host");
    let models = host
        .list_models(&config, &new_config_path, "azure-custom")
        .await
        .expect("custom Azure host");
    assert_eq!(models.kind, ListKind::Deployments);
    assert_eq!(ids(&models), ["deployment"]);
}

#[tokio::test]
async fn azure_messages_lists_live_models_not_deployments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-sonnet"}]
        })))
        .mount(&server)
        .await;
    let base = server.uri();
    let host = make_host().with_base_host(&base, "resource.services.ai.azure.com");
    let config = load_config(&format!(
        "[[providers]]\nid = \"azure-messages\"\npreset = \"azure\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{base}\"\napi_key = \"test\"\n"
    ));
    let new_config_path = make_config_path("azure-messages");
    let models = host
        .list_models(&config, &new_config_path, "azure-messages")
        .await
        .expect("azure messages");
    assert_eq!(models.kind, ListKind::Live);
    assert_eq!(ids(&models), ["claude-sonnet"]);
}
