use hofund_config::Config;
use hofund_control_proto::{Quota, QuotaKind};
use hofund_plugin_api::{BalanceCard, BalanceQuery, BalanceSupport, BalanceWallet, PluginError};
use hofund_router::quota::QuotaStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Job, JobKind, record, transient_error};
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
         [providers.balance]\nurl = \"{url}\"\npath = \"$amount\"\ntoken = \"panel\"\n\
         [[providers.keys]]\nkey = \"beta\"\nname = \"two\"\n\
         [[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n"
    );
    let runtime = runtime(&dir, &config(&text));
    let (first, _) = runtime.quota_report(false).await;
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|card| card.balance == "$10.00"));
    assert!(first.iter().all(|card| card.shared_balance.is_empty()));
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"amount":9}"#))
        .mount(&server)
        .await;
    let (cards, _) = runtime.quota_report(false).await;
    assert_eq!(cards.len(), 2);
    assert!(cards.iter().all(|card| card.balance == "$9.00"));
    let leader = provider_keys::key_id("alpha");
    assert_eq!(cards[0].shared_balance, leader);
    assert_eq!(cards[1].shared_balance, leader);
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

fn balance_job(support: BalanceSupport) -> Job {
    let mut card = Quota {
        provider: "relay".to_owned(),
        name: "relay".to_owned(),
        kind: QuotaKind::Balance,
        user: provider_keys::key_id("secret"),
        ..Quota::default()
    };
    card.member = format!("relay#{}", card.user);
    Job {
        kind: JobKind::Plugin(BalanceQuery::new(
            vec!["https://aihubmix.com/v1".to_owned()],
            "secret",
        )),
        card,
        support,
    }
}

fn balance_card(amount: f64) -> BalanceCard {
    BalanceCard::pack(
        format!("${amount:.2}"),
        Vec::new(),
        Vec::new(),
        vec![BalanceWallet::new(amount, "USD")],
        Vec::new(),
        String::new(),
        Vec::new(),
        None,
        String::new(),
        true,
    )
}

#[test]
fn p3w_possible_balance_failure_records_nothing_until_a_good_read() {
    let quotas = QuotaStore::in_memory();
    let card = balance_job(BalanceSupport::Possible).card;
    record(
        &quotas,
        balance_job(BalanceSupport::Possible),
        Err(PluginError::status_error(503, "busy")),
    );
    assert!(!quotas.has_reading(&card));
    assert!(quotas.report(&["relay".to_owned()]).is_empty());

    record(
        &quotas,
        balance_job(BalanceSupport::Possible),
        Ok(Some(balance_card(4.0))),
    );
    record(
        &quotas,
        balance_job(BalanceSupport::Possible),
        Err(PluginError::decode("invalid balance reply")),
    );
    let report = quotas.report(&["relay".to_owned()]);
    assert_eq!(report.len(), 1);
    assert!(!report[0].error.is_empty());
}

#[tokio::test]
async fn p3x_non_wide_keys_with_the_same_move_stay_unshared() {
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
    let (first, _) = runtime.quota_report(false).await;
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|card| card.shared_balance.is_empty()));
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"amount":9}"#))
        .mount(&server)
        .await;
    let (second, _) = runtime.quota_report(false).await;
    assert_eq!(second.len(), 2);
    assert!(second.iter().all(|card| card.balance == "$9.00"));
    assert!(second.iter().all(|card| card.shared_balance.is_empty()));
}

#[test]
fn p3y_transport_balance_failure_keeps_the_last_card() {
    let quotas = QuotaStore::in_memory();
    record(
        &quotas,
        balance_job(BalanceSupport::Certain),
        Ok(Some(balance_card(2.0))),
    );
    let error = PluginError::transport("connection reset");
    assert!(transient_error(&error));
    record(&quotas, balance_job(BalanceSupport::Certain), Err(error));
    let report = quotas.report(&["relay".to_owned()]);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].balance, "$2.00");
    assert!(report[0].as_of.is_some());
    assert!(report[0].error.is_empty());
}
