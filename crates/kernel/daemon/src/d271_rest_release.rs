use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_router::PersistedRest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::membership::member_id;
use crate::runtime::Runtime;

const QUOTA_WITHOUT_EXCLUSIONS: &str = r#"[{"quota":{"provider":"relay","name":"relay","kind":"plan","windows":[{"name":"5 hours","used":100.0,"remaining":0.0}]},"windows":[{"span":{"name":"5 hours","used":100.0,"remaining":0.0}}],"persistent":true}]"#;

fn catalog() -> serde_json::Value {
    serde_json::json!({"data": [
        {"id": "paid-model", "go": {"monthly_usage": {"go": 200}}},
        {"id": "free-model", "go": {"monthly_usage": null}},
        {"id": "legacy-zero", "cost": {"text": {
            "input": {"price": 0, "cache": {"read": 0, "write": 0}},
            "output": {"price": 0}
        }}}
    ]})
}

fn write_preupgrade_files(dir: &Path) {
    assert!(
        !QUOTA_WITHOUT_EXCLUSIONS.contains("excluded_models"),
        "the saved plan card must omit exclusions"
    );
    std::fs::write(dir.join("quotas.json"), QUOTA_WITHOUT_EXCLUSIONS).expect("quota file");
    let until = SystemTime::now() + Duration::from_secs(6 * 60 * 60);
    let relay = member_id("relay").to_string();
    let other = member_id("other").to_string();
    let rests = vec![
        PersistedRest::new(relay.clone(), until, 3),
        PersistedRest::for_model(relay, "paid-model".to_owned(), until, 2),
        PersistedRest::new(other, until, 4),
    ];
    std::fs::write(
        dir.join("rests.json"),
        serde_json::to_vec(&rests).expect("rests"),
    )
    .expect("rest file");
}

fn exclusions(runtime: &Runtime) -> Vec<String> {
    runtime
        .quotas()
        .alert_cards()
        .into_iter()
        .flat_map(|(_, windows)| windows)
        .flat_map(|window| window.excluded_models)
        .collect()
}

fn saved_rests(dir: &Path) -> Vec<PersistedRest> {
    serde_json::from_slice(&std::fs::read(dir.join("rests.json")).expect("rests")).expect("rests")
}

fn assert_loaded_whole_rest(runtime: &Runtime) {
    let relay = member_id("relay");
    let other = member_id("other");
    assert!(
        exclusions(runtime).is_empty(),
        "load saw exclusions {:?}",
        exclusions(runtime)
    );
    assert!(runtime.cooldowns().cooling(&relay).is_some());
    assert!(
        runtime
            .cooldowns()
            .cooling_for_model(&relay, "paid-model")
            .is_some()
    );
    assert!(runtime.cooldowns().cooling(&other).is_some());
}

fn assert_released_whole_rest(runtime: &Runtime, dir: &Path) {
    let relay = member_id("relay");
    let other = member_id("other");
    assert!(
        runtime.cooldowns().cooling(&relay).is_none(),
        "whole-member credit rest still blocks after the catalog was read"
    );
    assert!(
        runtime
            .cooldowns()
            .cooling_for_model(&relay, "paid-model")
            .is_some()
    );
    assert!(runtime.cooldowns().cooling(&other).is_some());
    let saved = saved_rests(dir);
    assert!(
        saved
            .iter()
            .all(|rest| rest.member != relay.to_string() || rest.model.is_some()),
        "rests.json kept the whole-member rest: {saved:?}"
    );
    assert_eq!(saved.len(), 2, "{saved:?}");
}

/// Resolves the synthetic `gated` preset as zero-price-free and delegates the rest.
struct FlagRegistry;

fn gated_preset() -> &'static hofund_plugin_registry::CompiledPreset {
    static PRESET: std::sync::OnceLock<hofund_plugin_registry::CompiledPreset> =
        std::sync::OnceLock::new();
    PRESET.get_or_init(|| hofund_plugin_registry::CompiledPreset {
        id: "gated",
        plugin: "openai-compat",
        name: "Gated",
        kind: "relay",
        short: "",
        note: "",
        catalog: "",
        website: "",
        keys_url: "",
        site_host: "",
        key_hint: "",
        no_key: false,
        no_list: false,
        options_base: false,
        hosts: false,
        only: "",
        models: &[],
        header_hints: &[],
        endpoint: "",
        endpoint_hint: "",
        endpoint_needed: "",
        region_label: "",
        decide: "",
        chat: "http://127.0.0.1:9/v1",
        responses: "",
        anthropic: "",
        generative: "",
        realtime: "",
        config: &[],
        regions: &[],
        zero_price_free: true,
    })
}

impl crate::DaemonRegistry for FlagRegistry {
    fn factory(&self, id: &str) -> Option<Arc<dyn hofund_plugin_api::DynUpstreamFactory>> {
        crate::RegistryAdapter.factory(id)
    }

    fn is_kernel_factory(&self, id: &str) -> bool {
        crate::RegistryAdapter.is_kernel_factory(id)
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        crate::RegistryAdapter.builtin_factory_ids()
    }

    fn builtin_protocols(&self) -> Vec<hofund_plugin_api::ProtocolId> {
        crate::RegistryAdapter.builtin_protocols()
    }

    fn find_preset(
        &self,
        id: &str,
    ) -> Option<(
        &'static hofund_plugin_registry::CompiledPreset,
        Option<&'static str>,
    )> {
        if id == "gated" {
            return Some((gated_preset(), None));
        }
        crate::RegistryAdapter.find_preset(id)
    }

    fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
        crate::RegistryAdapter.agents()
    }

    fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
        crate::RegistryAdapter.surfaces()
    }
}

#[tokio::test]
async fn d271_preupgrade_quota_releases_the_whole_member_rest_on_the_first_plan_read() {
    let dir = tempfile::tempdir().expect("dir");
    write_preupgrade_files(dir.path());
    let config = hofund_config::load(
        "[[providers]]\nid = \"relay\"\npreset = \"gated\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"test-key\"\n\
         [[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"other-key\"\n\
         [[routes]]\nmodel = \"paid-model\"\nupstreams = [\"relay\", \"other\"]\n",
    )
    .expect("config");
    let path = dir.path().join("hofund.toml");
    let runtime = Runtime::new(Arc::new(FlagRegistry), &path, Some(0));
    let mut host = crate::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .with_registry(Arc::new(FlagRegistry));
    runtime
        .initial_snapshot(&config, Some(&mut host))
        .expect("snapshot");
    host.seed_models(
        "relay",
        &[
            hofund_plugin_api::ModelSpec::new("free-model", None)
                .expect("model")
                .with_free(true),
            hofund_plugin_api::ModelSpec::new("paid-model", None)
                .expect("model")
                .with_prices(1, 2, 0),
        ],
    );
    let host = Arc::new(host);
    runtime.attach_models(Arc::clone(&host));
    assert_loaded_whole_rest(&runtime);
    let _ = runtime.quota_report(false).await;
    assert!(
        exclusions(&runtime).is_empty(),
        "a plan card without exclusions must stay empty"
    );
    assert_released_whole_rest(&runtime, dir.path());
}

#[tokio::test]
async fn d271_model_read_releases_the_whole_member_rest_without_saved_exclusions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(catalog()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    write_preupgrade_files(dir.path());
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"relay\"\npreset = \"gated\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n\
         [[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"other-key\"\n\
         [[routes]]\nmodel = \"paid-model\"\nupstreams = [\"relay\"]\n",
        server.uri()
    ))
    .expect("config");
    let path = dir.path().join("hofund.toml");
    let runtime = Runtime::new(Arc::new(FlagRegistry), &path, Some(0));
    let mut host = crate::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .with_registry(Arc::new(FlagRegistry));
    runtime
        .initial_snapshot(&config, Some(&mut host))
        .expect("snapshot");
    let host = Arc::new(host);
    runtime.attach_models(Arc::clone(&host));
    assert_loaded_whole_rest(&runtime);
    let listed = host
        .list_models(&config, &path, "relay")
        .await
        .expect("model list");
    let ids = listed
        .models
        .iter()
        .map(hofund_plugin_api::ModelSpec::id)
        .collect::<Vec<_>>();
    assert!(ids.contains(&"free-model"), "{ids:?}");
    assert!(ids.contains(&"paid-model"), "{ids:?}");
    assert!(
        exclusions(&runtime).is_empty(),
        "model read wrote plan exclusions {:?}",
        exclusions(&runtime)
    );
    assert_released_whole_rest(&runtime, dir.path());
}

#[tokio::test]
async fn d271_ungated_mixed_catalog_keeps_the_whole_key_rest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                {"id": "free-model", "go": {"monthly_usage": null}},
                {"id": "paid-model", "cost": {"text": {
                    "input": {"price": 1, "cache": {"read": 0, "write": 0}},
                    "output": {"price": 2}
                }}}
            ]})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("quotas.json"), "[]").expect("quota file");
    let until = SystemTime::now() + Duration::from_secs(6 * 60 * 60);
    let plain = member_id("plain").to_string();
    std::fs::write(
        dir.path().join("rests.json"),
        serde_json::to_vec(&vec![PersistedRest::new(plain.clone(), until, 3)]).expect("rests"),
    )
    .expect("rest file");
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"plain\"\nprotocol = \"openai-chat\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n\
         [[routes]]\nmodel = \"paid-model\"\nupstreams = [\"plain\"]\n",
        server.uri()
    ))
    .expect("config");
    let path = dir.path().join("hofund.toml");
    let runtime = Runtime::new(Arc::new(crate::RegistryAdapter), &path, Some(0));
    let mut host = crate::ModelsHost::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    );
    runtime
        .initial_snapshot(&config, Some(&mut host))
        .expect("snapshot");
    let host = Arc::new(host);
    runtime.attach_models(Arc::clone(&host));
    assert!(
        runtime.cooldowns().cooling(&member_id("plain")).is_some(),
        "the saved whole-key rest must load"
    );
    let listed = host
        .list_models(&config, &path, "plain")
        .await
        .expect("model list");
    let ids = listed
        .models
        .iter()
        .map(hofund_plugin_api::ModelSpec::id)
        .collect::<Vec<_>>();
    assert!(
        ids.contains(&"free-model") && ids.contains(&"paid-model"),
        "{ids:?}"
    );
    assert!(
        runtime.cooldowns().cooling(&member_id("plain")).is_some(),
        "a model read released an ungated whole-key rest"
    );
    let _ = runtime.quota_report(false).await;
    assert!(
        runtime.cooldowns().cooling(&member_id("plain")).is_some(),
        "a plan read released an ungated whole-key rest"
    );
    let saved = saved_rests(dir.path());
    assert!(
        saved
            .iter()
            .any(|rest| rest.member == plain && rest.model.is_none()),
        "rests.json dropped the ungated whole-key rest: {saved:?}"
    );
}
