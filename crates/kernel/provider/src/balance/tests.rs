use super::*;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(base: &str) -> Endpoint {
    let mut endpoint = Endpoint::new("provider", base);
    endpoint.api_key = "test-key".to_owned();
    endpoint
}

fn reader(base: &str, options: BalanceOptions) -> BalanceReader {
    BalanceReader::new(endpoint(base), options).expect("reader")
}

async fn read_report(reader: BalanceReader) -> BalanceReport {
    reader.read().await.expect("balance read").expect("card")
}

async fn mock(server: &MockServer, verb: &str, route: &str, status: u16, body: &str) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn p3i_custom_expression_aggregates_lists_and_keeps_partial_amounts() {
    let server = MockServer::start().await;
    let body = r#"{"data":{"total":8,"subscriptions":[{"status":"active","kind":"paid","used":20,"cap":40},{"status":"active","kind":"paid","used":10,"cap":20},{"status":"free","kind":"trial","used":100,"cap":0}]}}"#;
    mock(&server, "GET", "/usage", 200, body).await;
    let base = format!("{}/usage", server.uri());
    let report = read_report(
        reader(
            "https://relay.example/v1",
            BalanceOptions {
                url: base.clone(),
                path: "share: sum(data.subscriptions[status=active].used) / sum(data.subscriptions[status=active].cap) %; total: $data.total; broken: data.missing; count: count(data.subscriptions.*.kind)".to_owned(),
                ..BalanceOptions::default()
            },
        ),
    )
    .await;
    let balance = report.balance.expect("balance");
    assert_eq!(
        balance.text,
        "share 50% · total $8.00 · broken — · count 3.00"
    );
    assert_eq!(balance.parts.len(), 4);
    assert!(!balance.parts[2].error.is_empty());
    assert_eq!(balance.parts[0].percent, Some(50.0));

    let report = read_report(
        reader(
            "https://relay.example/v1",
            BalanceOptions {
                url: base,
                path: "max(data.subscriptions.*.used / data.subscriptions.*.cap) + min(data.subscriptions[status=active].cap)".to_owned(),
                ..BalanceOptions::default()
            },
        ),
    )
    .await;
    assert_eq!(report.balance.expect("balance").text, "20.50");
}

#[tokio::test]
async fn p3j_custom_auth_post_and_key_placeholders_are_applied() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/balance"))
        .and(query_param("key", "a b/+"))
        .and(header("authorization", "Bearer a b/+"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd_balance":"0.94306040"}"#))
        .mount(&server)
        .await;
    let mut endpoint = endpoint("https://relay.example/v1");
    endpoint.api_key = "a b/+".to_owned();
    endpoint.headers = vec![("Authorization".to_owned(), "Bearer {token}".to_owned())];
    let report = read_report(
        BalanceReader::new(
            endpoint,
            BalanceOptions {
                url: format!("{}/balance?key={{TOKEN}}", server.uri()),
                path: "$usd_balance".to_owned(),
                token: "Bearer a b/+".to_owned(),
                method: "post".to_owned(),
            },
        )
        .expect("reader"),
    )
    .await;
    assert_eq!(report.balance.expect("balance").text, "$0.94");

    let jwt_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/profile"))
        .and(header("authorization", "Bearer eyJ.one.two"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"balance":7.25}}"#))
        .mount(&jwt_server)
        .await;
    let report = read_report(reader(
        "https://relay.example/v1",
        BalanceOptions {
            url: format!("{}/api/v1/user/profile", jwt_server.uri()),
            token: "eyJ.one.two".to_owned(),
            ..BalanceOptions::default()
        },
    ))
    .await;
    assert_eq!(report.balance.expect("balance").text, "$7.25");
    assert!(report.account_wide);
}

#[tokio::test]
async fn p3k_custom_defaults_fallback_and_refusals_match_query_kind() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/usage/token",
        200,
        r#"{"data":{"total_available":2500000}}"#,
    )
    .await;
    let report = read_report(reader(
        "https://relay.example/v1",
        BalanceOptions {
            url: format!("{}/api/user/self?removed=1", server.uri()),
            path: "$data.quota / 500000".to_owned(),
            ..BalanceOptions::default()
        },
    ))
    .await;
    assert_eq!(report.balance.expect("balance").text, "$5.00");

    let refused = MockServer::start().await;
    mock(
        &refused,
        "GET",
        "/api/user/self",
        200,
        r#"{"success":false,"message":"New-Api-User header not provided"}"#,
    )
    .await;
    let error = reader(
        "https://relay.example/v1",
        BalanceOptions {
            url: format!("{}/api/user/self", refused.uri()),
            token: "account-token".to_owned(),
            ..BalanceOptions::default()
        },
    )
    .read()
    .await
    .expect_err("refusal");
    assert!(error.message.starts_with("add the header New-Api-User"));

    let error = reader(
        "https://relay.example/v1",
        BalanceOptions {
            url: format!("{}/api/usage/token", server.uri()),
            token: "account-token".to_owned(),
            ..BalanceOptions::default()
        },
    )
    .read()
    .await
    .expect_err("token cannot be sent to key query");
    assert!(error.message.contains("takes the API key"));
}

#[tokio::test]
async fn p3l_panel_usage_returns_windows_and_account_wide_metadata() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/v1/usage",
        200,
        r#"{"mode":"unrestricted","remaining":100,"rate_limits":[{"window":"5h","limit":10,"used":2.5,"reset_at":"2026-11-01T12:00:00Z"},{"window":"1d","limit":100,"used":25},{"window":"7d","limit":0,"used":0}]}"#,
    )
    .await;
    let report = read_report(reader(
        "https://relay.example/v1",
        BalanceOptions {
            url: format!("{}/v1/usage", server.uri()),
            ..BalanceOptions::default()
        },
    ))
    .await;
    assert!(report.account_wide);
    assert_eq!(report.balance.expect("balance").text, "$100.00");
    assert_eq!(report.windows.len(), 2);
    assert_eq!(report.windows[0].name, "5 hours");
    assert_eq!(report.windows[0].used, 25.0);
    assert_eq!(report.windows[0].unit, "USD");
}

#[test]
fn p3n_error_helpers_do_not_expose_key_or_accept_other_methods() {
    let redacted = redact_error("GET https://relay.test/?key=a+b%2F%2B failed", "a b/+");
    assert!(!redacted.contains("a b/+"));
    assert!(!redacted.contains("a+b%2F%2B"));
    assert!(redacted.contains("…"));
    assert!(balance_method("GET").is_ok());
    assert!(balance_method("post").is_ok());
    assert!(balance_method("PUT").is_err());
    assert!(balance_method("POST\r\nX-Test: bad").is_err());
}
