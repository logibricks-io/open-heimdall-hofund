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
async fn p3a_deepseek_orders_currencies_and_splits_granted_credit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/balance"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"balance_infos":[{"currency":"CNY","total_balance":"-0.05","granted_balance":"1.00","topped_up_balance":"-1.05"},{"currency":"USD","total_balance":"11.12","granted_balance":"1.12","topped_up_balance":"10.00"}]}"#,
        ))
        .mount(&server)
        .await;
    let report = read_report(
        reader("https://api.deepseek.com/v1", BalanceOptions::default())
            .with_source_root(server.uri()),
    )
    .await;
    let balance = report.balance.expect("balance");
    assert_eq!(balance.text, "$11.12 · ¥-0.05");
    assert_eq!(balance.wallets[0].unit, "USD");
    assert_eq!(balance.wallets[1].unit, "CNY");
    assert_eq!(balance.split.len(), 4);
    assert_eq!(balance.split[0].label, "Granted");
    assert_eq!(balance.split[0].text, "$1.12");
    assert_eq!(balance.split[1].label, "Topped up");
    assert!(report.account_wide);
}

#[tokio::test]
async fn p3b_moonshot_reads_available_balance_in_each_regional_currency() {
    for (host, sign) in [("api.moonshot.cn", "¥"), ("api.moonshot.ai", "$")] {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            "/v1/users/me/balance",
            200,
            r#"{"code":0,"data":{"available_balance":"49.58"},"status":true}"#,
        )
        .await;
        let report = read_report(
            reader(&format!("https://{host}/v1"), BalanceOptions::default())
                .with_source_root(server.uri()),
        )
        .await;
        assert_eq!(
            report.balance.expect("balance").text,
            format!("{sign}49.58")
        );
    }
}

#[tokio::test]
async fn p3c_openrouter_subtracts_usage_from_credits() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/credits",
        200,
        r#"{"data":{"total_credits":20,"total_usage":3.5}}"#,
    )
    .await;
    let report = read_report(
        reader("https://openrouter.ai/api/v1", BalanceOptions::default())
            .with_source_root(server.uri()),
    )
    .await;
    let balance = report.balance.expect("balance");
    assert_eq!(balance.text, "$16.50");
    assert_eq!(balance.wallets[0].amount, 16.5);
}

#[tokio::test]
async fn p3d_siliconflow_reads_regions_and_explains_retired_query() {
    for (host, sign) in [("api.siliconflow.cn", "¥"), ("api.siliconflow.com", "$")] {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            "/v1/user/info",
            200,
            r#"{"code":20000,"data":{"totalBalance":"88.88"}}"#,
        )
        .await;
        let report = read_report(
            reader(&format!("https://{host}/v1"), BalanceOptions::default())
                .with_source_root(server.uri()),
        )
        .await;
        assert_eq!(
            report.balance.expect("balance").text,
            format!("{sign}88.88")
        );

        let retired = MockServer::start().await;
        mock(
            &retired,
            "GET",
            "/v1/user/info",
            410,
            r#"{"code":20092,"message":"deprecated","data":null}"#,
        )
        .await;
        let error = reader(&format!("https://{host}/v1"), BalanceOptions::default())
            .with_source_root(retired.uri())
            .read()
            .await
            .expect_err("retired");
        assert!(error.message.contains("retired its balance query"));
        assert!(error.message.contains(&format!(
            "https://cloud.siliconflow.{}",
            if host.ends_with(".cn") { "cn" } else { "com" }
        )));
    }
}

#[tokio::test]
async fn p3e_stepfun_reads_spendable_balance_with_vouchers() {
    for (host, sign) in [("api.stepfun.com", "¥"), ("api.stepfun.ai", "$")] {
        let server = MockServer::start().await;
        mock(
            &server,
            "GET",
            "/v1/accounts",
            200,
            r#"{"object":"account","type":"prepaid","balance":26.0,"total_cash_balance":0.0,"total_voucher_balance":26.0}"#,
        )
        .await;
        let report = read_report(
            reader(&format!("https://{host}/v1"), BalanceOptions::default())
                .with_source_root(server.uri()),
        )
        .await;
        assert_eq!(
            report.balance.expect("balance").text,
            format!("{sign}26.00")
        );
    }
}

#[tokio::test]
async fn p3f_commandcode_reads_bought_credits_and_plan_month_meter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alpha/billing/credits"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"credits":{"monthlyCredits":5.2207933742,"purchasedCredits":0,"freeCredits":0},"windowLimits":{"limited":true,"fiveHour":{"used":0.0866816244,"cap":14,"resetAt":4102444800000},"weekly":{"used":0.8971969678,"cap":35,"resetAt":4102444800000}}}"#,
        ))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/alpha/billing/subscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"success":true,"data":{"planId":"individual-go-monthly","status":"active","currentPeriodEnd":"2026-11-15T00:00:00Z"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let reader = reader(
        "https://api.commandcode.ai/provider/v1",
        BalanceOptions::default(),
    )
    .with_source_root(server.uri());
    for _ in 0..2 {
        let report = report_for(&reader).await;
        assert_eq!(report.balance.as_ref().expect("balance").text, "$5.22");
        let plan = report.plan.expect("plan");
        assert_eq!(plan.name, "Go");
        assert_eq!(plan.windows.len(), 3);
        assert_eq!(plan.windows[2].name, "Credits");
        assert_eq!(plan.windows[2].display, "$4.78 / $10.00");
        assert!((plan.windows[2].used - 47.792).abs() < 0.01);
    }
}

async fn report_for(reader: &BalanceReader) -> BalanceReport {
    reader.read().await.expect("balance read").expect("card")
}

#[tokio::test]
async fn p3g_ollama_separates_included_allowance_from_bought_credits() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/balance",
        200,
        r#"{"included":{"balance_usd":72.5,"allowance_usd":100,"period":{"from":"2026-10-15T09:30:00Z","until":"2026-11-15T09:30:00Z"}},"purchased":{"balance_usd":25}}"#,
    )
    .await;
    let report = read_report(
        reader("https://ollama.com/v1", BalanceOptions::default()).with_source_root(server.uri()),
    )
    .await;
    assert_eq!(report.balance.expect("credits").text, "$25.00");
    let plan = report.plan.expect("plan");
    assert_eq!(plan.windows[0].name, "Month");
    assert_eq!(plan.windows[0].amount, 27.5);
    assert!(!plan.windows[0].blocking);

    let missing = MockServer::start().await;
    mock(
        &missing,
        "GET",
        "/api/balance",
        200,
        r#"{"included":{"session":{"remaining_percent":75,"resets_at":"2026-11-01T07:00:00Z"},"weekly":{"remaining_percent":40,"resets_at":"2026-11-05T00:00:00Z"}}}"#,
    )
    .await;
    let report = read_report(
        reader("https://ollama.com/v1", BalanceOptions::default()).with_source_root(missing.uri()),
    )
    .await;
    assert!(report.balance.is_none());
    let plan = report.plan.expect("legacy plan");
    assert_eq!(plan.windows.len(), 2);
    assert_eq!(plan.windows[0].used, 25.0);
    assert_eq!(plan.windows[1].used, 60.0);
    assert!(plan.windows.iter().all(|window| window.blocking));
}

#[tokio::test]
async fn p3h_aihubmix_reads_key_or_account_and_rejects_unlimited_key() {
    let key_server = MockServer::start().await;
    mock(
        &key_server,
        "GET",
        "/dashboard/billing/remain",
        200,
        r#"{"object":"list","total_usage":12.5}"#,
    )
    .await;
    let report = read_report(
        reader("https://aihubmix.com/v1", BalanceOptions::default())
            .with_source_root(key_server.uri()),
    )
    .await;
    assert_eq!(report.balance.expect("key balance").text, "$12.50");
    assert!(!report.account_wide);

    let account_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/self"))
        .and(header("authorization", "account-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"success":true,"data":{"quota":2500000}}"#),
        )
        .mount(&account_server)
        .await;
    let report = read_report(
        reader(
            "https://aihubmix.com/v1",
            BalanceOptions {
                token: "account-token".to_owned(),
                ..BalanceOptions::default()
            },
        )
        .with_source_root(account_server.uri()),
    )
    .await;
    assert_eq!(report.balance.expect("account balance").text, "$5.00");
    assert!(report.account_wide);

    let unlimited = MockServer::start().await;
    mock(
        &unlimited,
        "GET",
        "/dashboard/billing/remain",
        200,
        r#"{"total_usage":-1}"#,
    )
    .await;
    let error = reader("https://aihubmix.com/v1", BalanceOptions::default())
        .with_source_root(unlimited.uri())
        .read()
        .await
        .expect_err("unlimited key");
    assert!(error.message.contains("has no limit"));
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
    assert_eq!(balance.text, "share 50% · total $8.00 · broken — · count 3.00");
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

#[tokio::test]
async fn p3m_opencode_zen_says_no_key_balance_without_network() {
    let error = reader("https://opencode.ai/zen/v1", BalanceOptions::default())
        .read()
        .await
        .expect_err("no key balance");
    assert!(error.message.contains("tells no API key its balance"));
    assert!(error.message.contains("https://opencode.ai/console"));

    let unsupported = reader("https://example.invalid/v1", BalanceOptions::default())
        .read()
        .await
        .expect("unsupported is not an error");
    assert!(unsupported.is_none());
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
