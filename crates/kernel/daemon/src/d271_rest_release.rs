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

fn started(dir: &Path, text: &str) -> Arc<Runtime> {
    let config = hofund_config::load(text).expect("config");
    let runtime = Runtime::new(
        Arc::new(crate::RegistryAdapter),
        &dir.join("hofund.toml"),
        Some(0),
    );
    runtime.initial_snapshot(&config, None).expect("snapshot");
    runtime
}

#[tokio::test]
async fn d271_preupgrade_quota_releases_the_whole_member_rest_on_the_first_plan_read() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"usage": {
            "rolling": {"status": "rate-limited", "percent": 100, "resetsAt": "2026-09-25T20:00:00Z"},
            "weekly": {"status": "rate-limited", "percent": 100, "resetsAt": "2026-09-28T00:00:00Z"}
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(catalog()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    write_preupgrade_files(dir.path());
    let usage = format!("{}/usage", server.uri());
    let runtime = started(
        dir.path(),
        &format!(
            "[[providers]]\nid = \"relay\"\npreset = \"opencode-go\"\napi_key = \"test-key\"\n\
             [providers.options]\nplan_quota_url = \"{usage}\"\n\
             [[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"other-key\"\n\
             [[routes]]\nmodel = \"paid-model\"\nupstreams = [\"relay\", \"other\"]\n"
        ),
    );
    assert_loaded_whole_rest(&runtime);
    let _ = runtime.quota_report(false).await;
    let found = exclusions(&runtime);
    assert!(
        found.iter().any(|id| id == "free-model"),
        "plan read did not learn the free model: {found:?}"
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
        "[[providers]]\nid = \"relay\"\npreset = \"opencode-go\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n\
         [[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"other-key\"\n\
         [[routes]]\nmodel = \"paid-model\"\nupstreams = [\"relay\"]\n",
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
