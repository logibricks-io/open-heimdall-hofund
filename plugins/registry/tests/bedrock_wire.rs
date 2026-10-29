//! The bedrock plugin's Messages calls on a real wire: header values the
//! recording client does not keep.

use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{BuildContext, PluginId, ProbeQuery, ProtocolId, RawTable, RawValue};
use hofund_plugin_registry::factory;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Probes a Messages provider built with `headers` and returns the headers
/// the upstream received.
async fn probe_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().expect("root");
    let host = runtime_host(
        PluginId::from_static("bedrock"),
        "ABSK-wire-key",
        root.path(),
        HttpLimits::default(),
    )
    .expect("host");
    let mut raw = RawTable::new();
    raw.insert(
        "base_url",
        RawValue::String(format!("{}/anthropic", server.uri())),
    );
    let mut table = RawTable::new();
    for (name, value) in headers {
        table.insert(*name, RawValue::String((*value).to_owned()));
    }
    raw.insert("headers", RawValue::Table(table));
    let provider = factory("bedrock")
        .expect("bedrock factory")
        .validate_and_build_with(
            &raw,
            &host,
            &BuildContext::new(ProtocolId::ANTHROPIC_MESSAGES),
        )
        .expect("build");
    provider
        .probe(&ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude").expect("query"))
        .await
        .expect("probe");
    let received = server.received_requests().await.expect("recording");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.path(), "/anthropic/v1/messages");
    received[0]
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn values<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_messages_probe_sends_the_version_and_the_key() {
    let headers = probe_headers(&[]).await;
    assert_eq!(values(&headers, "anthropic-version"), ["2023-06-01"]);
    assert_eq!(values(&headers, "x-api-key"), ["ABSK-wire-key"]);
    assert!(values(&headers, "authorization").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_betas_are_fitted_and_a_configured_version_wins() {
    let headers = probe_headers(&[
        (
            "anthropic-beta",
            "advanced-tool-use-2025-11-20, redact-thinking-2026-02-12",
        ),
        ("anthropic-version", "2099-01-01"),
        ("x-team", "a, a"),
    ])
    .await;
    assert_eq!(
        values(&headers, "anthropic-beta"),
        ["tool-search-tool-2025-10-19"]
    );
    assert_eq!(values(&headers, "anthropic-version"), ["2099-01-01"]);
    assert_eq!(values(&headers, "x-team"), ["a, a"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_beta_with_nothing_supported_is_dropped() {
    let headers = probe_headers(&[("anthropic-beta", "prompt-caching-scope-2026-01-05")]).await;
    assert!(values(&headers, "anthropic-beta").is_empty(), "{headers:?}");
}
