use hofund_config::Config;
use hofund_control_proto::QuotaKind;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::runtime::Runtime;
use crate::{RegistryAdapter, provider_keys};

fn config(text: &str) -> Config {
    hofund_config::load(text).expect("config")
}

fn runtime(dir: &tempfile::TempDir, config: &Config) -> std::sync::Arc<Runtime> {
    let runtime = Runtime::new(
        std::sync::Arc::new(RegistryAdapter),
        &dir.path().join("hofund.toml"),
        Some(0),
    );
    runtime.initial_snapshot(config, None).expect("snapshot");
    runtime
}

#[tokio::test]
async fn p3p_custom_balance_url_feeds_quota_and_router_wallet() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/panel/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd":42.5}"#))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let text = format!(
        "[[providers]]\nid = \"relay\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"secret\"\n\
         [providers.balance]\nurl = \"{}/panel/balance\"\npath = \"$usd\"\n\
         [[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n",
        server.uri()
    );
    let runtime = runtime(&dir, &config(&text));
    let (cards, _) = runtime.quota_report(false).await;
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].kind, QuotaKind::Balance);
    assert_eq!(cards[0].balance, "$42.50");
    let allowances = runtime.quota_allowances();
    assert_eq!(allowances.len(), 1);
    assert_eq!(
        allowances[0].left("demo"),
        Some((42.5, "relay\0USD".to_owned()))
    );
}

#[tokio::test]
async fn p3q_two_keys_with_the_same_balance_share_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"amount":10}"#))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let url = format!("{}/shared", server.uri());
    let text = format!(
        "[[providers]]\nid = \"relay\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"alpha\"\nkey_name = \"one\"\n\
         [providers.balance]\nurl = \"{url}\"\npath = \"$amount\"\n\
         [[providers.keys]]\nkey = \"beta\"\nname = \"two\"\n\
         [[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n"
    );
    let runtime = runtime(&dir, &config(&text));
    let (cards, _) = runtime.quota_report(false).await;
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|card| card.balance == "$10.00"));
    let first = provider_keys::key_id("alpha");
    assert_eq!(cards[0].shared_balance, first);
    assert_eq!(cards[1].shared_balance, first);
}

#[tokio::test]
async fn p3r_transient_balance_failure_keeps_the_last_card() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd":1}"#))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let url = format!("{}/balance", server.uri());
    let text = format!(
        "[[providers]]\nid = \"relay\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"secret\"\n\
         [providers.balance]\nurl = \"{url}\"\npath = \"$usd\"\n\
         [[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n"
    );
    let runtime = runtime(&dir, &config(&text));
    let (first, _) = runtime.quota_report(false).await;
    assert_eq!(first[0].balance, "$1.00");
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/balance"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&server)
        .await;
    let (cached, _) = runtime.quota_report(false).await;
    assert_eq!(cached[0].balance, "$1.00");
    assert!(cached[0].as_of.is_some());
    assert!(cached[0].error.is_empty());
}
