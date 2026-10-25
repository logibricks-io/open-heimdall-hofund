//! Model lists and provider probes against a local mock. No real network.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_daemon::{provider_models, provider_test, refresh_scheduled, usage_report};
use serde_json::json;
use wiremock::matchers::{method, path as url_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("hofund-provider-ops-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("dir");
    path
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("hofund.toml");
    fs::write(&path, body).expect("write");
    path
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("secs")
}

#[tokio::test]
async fn models_prints_a_diff_only_after_a_successful_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-a"}, {"id": "gpt-b"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = scratch();
    let path = write_config(
        &dir,
        &format!(
            "[[providers]]\nid = \"openai\"\npreset = \"openai\"\nbase_url = \"{}/v1\"\napi_key = \"k\"\n",
            server.uri()
        ),
    );
    let first = provider_models(&path, "openai").await.expect("first");
    assert!(first.contains("2 models on chat"), "{first}");
    assert!(!first.contains("+ gpt"), "{first}");
    assert!(first.contains("gpt-a\n"));
    assert!(first.contains("gpt-b\n"));

    server.reset().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-b"}, {"id": "gpt-c"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let second = provider_models(&path, "openai").await.expect("second");
    assert!(second.contains("+ gpt-c"), "{second}");
    assert!(second.contains("- gpt-a"), "{second}");
    assert!(!second.contains("+ gpt-b"), "{second}");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_catalog_preset_is_printed_and_not_fetched() {
    let dir = scratch();
    let path = write_config(
        &dir,
        "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\nproject = \"my-project\"\nrefresh_every = \"10m\"\n",
    );
    let listed = provider_models(&path, "vertex").await.expect("catalog");
    assert!(listed.contains("catalog"), "{listed}");
    assert!(listed.contains("gemini-2.5-flash"), "{listed}");
    refresh_scheduled(&path, now()).await.expect("refresh");
    let cache = fs::read_to_string(dir.join("hofund-models.json")).expect("cache");
    assert!(
        !cache.contains("attempt_ok\": false") && !cache.contains("generativelanguage"),
        "{cache}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_scheduled_refresh_waits_after_a_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-a"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = scratch();
    let path = write_config(
        &dir,
        &format!(
            "[[providers]]\nid = \"openai\"\npreset = \"openai\"\nbase_url = \"{}/v1\"\napi_key = \"k\"\nrefresh_every = \"10m\"\n",
            server.uri()
        ),
    );
    let moment = now();
    refresh_scheduled(&path, moment).await.expect("first");
    refresh_scheduled(&path, moment).await.expect("second");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn provider_test_probes_each_wire_and_records_a_usage_row() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-test"}]
        })))
        .mount(&server)
        .await;
    let chat = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
    Mock::given(method("POST"))
        .and(url_path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(chat))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(url_path("/v1/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "no responses"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = scratch();
    let path = write_config(
        &dir,
        &format!(
            "[[providers]]\nid = \"openai\"\npreset = \"openai\"\nbase_url = \"{}/v1\"\napi_key = \"k\"\n",
            server.uri()
        ),
    );
    let report = provider_test(&path, "openai").await.expect("test");
    assert!(!report.ok, "{}", report.text);
    assert!(report.text.contains("chat ok 200"), "{}", report.text);
    assert!(report.text.contains("gpt-test"), "{}", report.text);
    assert!(
        report.text.contains("responses fail 401"),
        "{}",
        report.text
    );
    let table = usage_report(&path).expect("usage");
    assert!(table.contains("gpt-test"), "{table}");
    assert!(table.contains("200"), "{table}");
    assert!(table.contains("401"), "{table}");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_preset_without_an_endpoint_sends_nothing() {
    let dir = scratch();
    let path = write_config(
        &dir,
        "[[providers]]\nid = \"typesafe\"\npreset = \"typesafe\"\napi_key = \"k\"\n",
    );
    let error = provider_test(&path, "typesafe").await.expect_err("decide");
    assert!(error.to_string().contains("classifies"), "{error}");
    let table = usage_report(&path).expect("usage");
    assert!(!table.contains("typesafe"), "{table}");
    let _ = fs::remove_dir_all(&dir);
}
