//! Remote quota cards merged from a loopback peer gateway.

mod support;

use hofund_control_proto::{ListEnvelope, Quota, QuotaKind, QuotaResponse};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn start_with_remote(peer: &str, key: &str) -> (Running, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("hofund-remote-quotas-")
        .tempdir()
        .expect("scratch");
    let config = dir.path().join("hofund.toml");
    let text = format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"office\"\nname = \"Office\"\npreset = \"remote-gateway\"\nprotocol = \"openai-chat\"\nbase_url = \"{peer}\"\napi_key = \"{key}\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"office\"]\n"
    );
    std::fs::write(&config, text).expect("config");
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_runtime: Some(ControlRuntime::new(None, dir.path().to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    (running, dir)
}

#[tokio::test]
async fn d38r_1_remote_cards_prefix_and_cache() {
    let peer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/hofund/quotas/cards"))
        .and(header("authorization", "Bearer sk-office"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ListEnvelope::new(vec![Quota {
                provider: "codex".to_owned(),
                name: "Codex".to_owned(),
                kind: QuotaKind::Subscription,
                user: "a@x.com".to_owned(),
                ..Quota::default()
            }])),
        )
        .mount(&peer)
        .await;
    let (running, dir) = start_with_remote(&peer.uri(), "sk-office").await;
    let token = std::fs::read_to_string(dir.path().join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned();
    let control = format!("http://{}", running.control_local_addr().expect("control"));
    let first = support::client()
        .get(format!("{control}/v1/hofund/quotas?kept=1"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("quotas");
    let first: QuotaResponse = first.json().await.expect("body");
    let codex = first
        .data
        .iter()
        .find(|card| card.provider == "office/codex")
        .expect("office/codex");
    assert_eq!(codex.name, "Codex · Office");
    assert_eq!(codex.user, "a@x.com");
    let _ = support::client()
        .get(format!("{control}/v1/hofund/quotas?kept=1"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("quotas again");
    assert_eq!(peer.received_requests().await.unwrap_or_default().len(), 1);
}
