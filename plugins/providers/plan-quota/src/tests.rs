use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{HostContext, PlanQuery, PluginId};
use hofund_plugin_sdk::json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::control_plans::{tc3_signature, volc_signature};
use crate::{Reader, Source, read_source};

fn host() -> (tempfile::TempDir, HostContext) {
    let root = tempfile::tempdir().expect("tempdir");
    let host = hofund_kit_upstream::runtime_host(
        PluginId::from_static("plan-quota"),
        "test-key",
        root.path(),
        hofund_kit_upstream::HttpLimits::new(Duration::from_secs(1), Duration::from_secs(2)),
    )
    .expect("host");
    (root, host)
}

fn query() -> PlanQuery {
    PlanQuery::new(Vec::new(), "test-key")
        .with_access_key("test-access-id", "test-access-secret")
        .with_now(UNIX_EPOCH + Duration::from_secs(1_793_548_815))
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| value | (u32::from(*byte) << (16 - index * 8)));
        for index in 0..4 {
            if index > chunk.len() {
                break;
            }
            let shift = 18 - index * 6;
            out.push(char::from(ALPHABET[((value >> shift) & 63) as usize]));
        }
    }
    out
}

fn jwt(claims: Value) -> String {
    format!("e30.{}.signature", b64url(claims.to_string().as_bytes()))
}

#[tokio::test]
async fn p1_kimi_reads_counted_and_every_month_share_form() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usages"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "usage": {"limit": "100", "used": "30", "resetTime": "2026-10-31T05:24:18.44Z"},
            "limits": [
                {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"}, "detail": {"limit": "100", "remaining": "88", "resetTime": "2026-10-29T09:00:00Z"}},
                {"window": {"duration": 1, "timeUnit": "TIME_UNIT_WEEK"}, "detail": {"limit": 100, "used": 20}}
            ],
            "usages": {
                "limitMonthTotal": {"usedRatio": "0.4", "resetTime": 1_794_873_600_000_i64},
                "limit_month_code": {"used_ratio": 0.1},
                "limit_7d": {"used_ratio": 0.3012, "reset_time": "2026-10-31T05:24:18Z"},
                "limit_5h": {"used_ratio": 0.1234}
            }
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::key_plans::read_kimi(&query(), &host, &format!("{}/usages", server.uri()))
        .await
        .expect("Kimi plan");
    let windows = cards[0].windows();
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].name(), "5 hours");
    assert!((windows[0].used() - 12.34).abs() < 1e-9);
    assert_eq!(windows[1].name(), "7 days");
    assert!((windows[1].used() - 30.12).abs() < 1e-9);
    assert_eq!(windows[2].name(), "Month");
    assert_eq!(windows[2].span(), Some(Duration::from_secs(30 * 86_400)));
    assert_eq!(windows[2].resets_at(), Some(UNIX_EPOCH + Duration::from_secs(1_794_873_600)));

    let month_code_only = json!({"usages": {
        "limit_5h": {"used_ratio": 0.25},
        "limit_month_code": {"used_ratio": 0.5, "reset_time": "2026-11-17T00:00:00Z"}
    }});
    let parsed = crate::key_plans::parse_kimi(month_code_only.to_string().as_bytes()).expect("code share");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].name(), "Month");
    assert_eq!(parsed[1].used(), 50.0);
    let counted = crate::key_plans::parse_kimi(br#"{"limits":[{"window":{"duration":1,"timeUnit":"TIME_UNIT_MONTH"},"detail":{"limit":100,"remaining":50,"resetTime":1794873600}}]}"#)
        .expect("counted month");
    assert_eq!(counted[0].name(), "Month");
    assert_eq!(counted[0].used(), 50.0);
}

#[tokio::test]
async fn p2_zhipu_reads_windows_team_fallback_and_plan_term() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/monitor/usage/quota/limit"))
        .and(header("authorization", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 200, "success": true,
            "data": {"level": "pro", "limits": [
                {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 12, "nextResetTime": 1_790_000_000_000_i64},
                {"type": "TOKENS_LIMIT", "unit": 6, "number": 7, "percentage": 40, "nextResetTime": 1_790_500_000_000_i64},
                {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 3}
            ]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/biz/subscription/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 200, "success": true,
            "data": [{"productName": "GLM Coding Pro", "status": "VALID", "autoRenew": 1, "nextRenewTime": "2026-10-18 10:00:00"}]
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::key_plans::read_zhipu(&query(), &host, &format!("{}/api/monitor/usage/quota/limit", server.uri()), &server.uri())
        .await
        .expect("Zhipu plan");
    let card = &cards[0];
    assert_eq!(card.plan(), "pro");
    assert_eq!(card.windows().len(), 3);
    assert_eq!(card.windows()[0].name(), "5 hours");
    assert_eq!(card.windows()[1].name(), "7 days");
    assert!(!card.windows()[2].blocking());
    assert_eq!(card.renew(), "auto");
    assert_eq!(card.until(), hofund_plugin_sdk::time::parse_rfc3339("2026-10-18T10:00:00+08:00"));
    let off = crate::key_plans::parse_zhipu_term(br#"{"data":[{"status":"VALID","valid":"2026-09-18 10:00:00-2026-12-18 10:00:00","autoRenew":false}]}"#)
        .expect("off term");
    assert_eq!(off.1, "off");
    assert_eq!(off.0, hofund_plugin_sdk::time::parse_rfc3339("2026-12-18T10:00:00+08:00").expect("date"));

    let team = crate::key_plans::parse_zhipu(br#"{"success":true,"data":{"level":"team","limits":[{"type":"CREDIT_LIMIT","unit":3,"number":0,"percentage":42}]}}"#)
        .expect("team window");
    assert_eq!(team.1[0].name(), "5 hours");
    let rate_limited = crate::key_plans::parse_zhipu(br#"{"code":1302,"msg":"too busy","success":false}"#)
        .expect_err("envelope rate limit");
    assert_eq!(rate_limited.status(), Some(429));
    let refused = crate::key_plans::parse_zhipu(br#"{"code":1000,"msg":"bad key","success":false}"#)
        .expect_err("refused key");
    assert_eq!(refused.kind(), hofund_plugin_api::PluginErrorKind::Auth);
}

#[tokio::test]
async fn p3_open_code_reads_all_windows_and_says_why_a_read_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usage": {
            "rolling": {"status": "ok", "percent": 0, "resetsAt": "2026-09-25T20:00:00Z"},
            "weekly": {"status": "ok", "percent": 62, "resetsAt": "2026-09-28T00:00:00Z"},
            "monthly": {"status": "rate-limited", "percent": 100, "resetsAt": "2026-10-11T00:00:00Z"}
        }})))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::key_plans::read_open_code(&query(), &host, &format!("{}/usage", server.uri()))
        .await
        .expect("OpenCode plan");
    let windows = cards[0].windows();
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].resets_at(), None);
    assert_eq!(windows[1].used(), 62.0);
    assert_eq!(windows[2].name(), "Month");

    let denied = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {"message": "subscription required"}})))
        .mount(&denied)
        .await;
    let error = crate::key_plans::read_open_code(&query(), &host, &format!("{}/usage", denied.uri()))
        .await
        .expect_err("no subscription");
    assert!(error.to_string().contains("member who subscribed"), "{error}");
    let malformed = crate::key_plans::read_open_code(&query(), &host, &format!("{}/usage", server.uri()))
        .await;
    drop(malformed);
    let shape = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usage": {"fiveHour": {"used": 3}}, "plan": "go"})))
        .mount(&shape)
        .await;
    let error = crate::key_plans::read_open_code(&query(), &host, &format!("{}/usage", shape.uri()))
        .await
        .expect_err("unknown shape");
    assert!(error.to_string().contains("usage.fiveHour"), "{error}");
    assert!(!error.to_string().contains('3'), "{error}");
}

#[tokio::test]
async fn p4_command_code_adds_month_credits_and_keeps_the_cached_plan() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alpha/billing/credits"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "credits": {"monthlyCredits": 5.2207933742, "purchasedCredits": 0, "freeCredits": 0},
            "windowLimits": {"limited": true,
                "fiveHour": {"used": 0.0866816244, "cap": 14, "resetAt": 4_102_444_800_000_i64},
                "weekly": {"used": 0.8971969678, "cap": 35, "resetAt": 4_102_444_800_000_i64}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/alpha/billing/subscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "data": {"planId": "individual-go-monthly", "status": "active", "currentPeriodEnd": 4_102_444_800_000_i64, "cancelAtPeriodEnd": false}
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let reader = Reader::default();
    let source = Source::CommandCode {
        credits: format!("{}/alpha/billing/credits", server.uri()),
        subscriptions: format!("{}/alpha/billing/subscriptions", server.uri()),
    };
    let cards = read_source(&reader, &query(), &host, source.clone()).await.expect("Command Code");
    let card = &cards[0];
    assert_eq!(card.plan(), "Go");
    assert_eq!(card.windows().len(), 3);
    assert_eq!(card.windows()[0].name(), "5 hours");
    assert_eq!(card.windows()[1].name(), "Weekly");
    assert_eq!(card.windows()[2].display(), "$4.78 / $10.00");
    assert_eq!(card.renew(), "auto");

    Mock::given(method("GET"))
        .and(path("/alpha/billing/subscriptions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let later = query().with_now(query().now() + Duration::from_secs(601));
    let cards = read_source(&reader, &later, &host, source).await.expect("cached plan");
    assert_eq!(cards[0].plan(), "Go");
    assert_eq!(cards[0].windows().len(), 3);
}

#[tokio::test]
async fn p5_mini_max_reads_general_aside_counts_and_used_up_windows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/token_plan/remains"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_remains": [
                {"model_name": "general", "start_time": 1_791_040_000_000_i64, "end_time": 1_791_126_400_000_i64,
                    "current_interval_total_count": 0, "current_interval_usage_count": 0, "current_interval_remaining_percent": 94, "current_interval_status": 1,
                    "weekly_start_time": 1_791_043_200_000_i64, "weekly_end_time": 1_791_648_000_000_i64,
                    "current_weekly_total_count": 0, "current_weekly_remaining_percent": 93, "current_weekly_status": 1},
                {"model_name": "video", "start_time": 1_791_043_200_000_i64, "end_time": 1_791_129_600_000_i64,
                    "current_interval_total_count": 5, "current_interval_usage_count": 1, "current_interval_remaining_percent": 80, "current_interval_status": 1,
                    "weekly_start_time": 1_791_043_200_000_i64, "weekly_end_time": 1_791_648_000_000_i64,
                    "current_weekly_total_count": 35, "current_weekly_usage_count": 0, "current_weekly_remaining_percent": 0, "current_weekly_status": 2},
                {"model_name": "music", "current_interval_status": 3, "current_interval_total_count": 0, "current_weekly_status": 3, "current_weekly_total_count": 0}
            ],
            "base_resp": {"status_code": 0, "status_msg": "success"}
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::key_plans::read_mini_max(&query(), &host, &format!("{}/v1/token_plan/remains", server.uri()))
        .await
        .expect("MiniMax plan");
    let windows = cards[0].windows();
    assert_eq!(windows.len(), 4);
    assert_eq!(windows[0].name(), "24 hours");
    assert_eq!(windows[0].used(), 6.0);
    assert_eq!(windows[1].name(), "7 days");
    assert_eq!(windows[2].name(), "Video · 24 hours");
    assert_eq!(windows[2].amount(), 1.0);
    assert_eq!(windows[2].limit(), 5.0);
    assert_eq!(windows[2].unit(), "videos");
    assert!(!windows[2].blocking());
    assert_eq!(windows[3].used(), 100.0);
    assert_eq!(windows[3].amount(), 35.0);

    let mismatch = crate::key_plans::parse_mini_max(br#"{"model_remains":[{"model_name":"video","current_interval_total_count":5,"current_interval_usage_count":2,"current_interval_remaining_percent":50,"current_interval_status":1}],"base_resp":{"status_code":0}}"#)
        .expect("count mismatch");
    assert_eq!(mismatch[0].limit(), 0.0);
}

#[tokio::test]
async fn p6_ollama_reads_included_allowance_and_credit_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/balance"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "included": {"balance_usd": 72.5, "allowance_usd": 100, "period": {"from": "2026-09-15T09:30:00Z", "until": "2026-10-15T09:30:00Z"}},
            "purchased": {"balance_usd": 25.0}
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::key_plans::read_ollama(&query(), &host, &format!("{}/api/balance", server.uri()))
        .await
        .expect("Ollama plan");
    let window = &cards[0].windows()[0];
    assert_eq!(window.name(), "Month");
    assert_eq!(window.amount(), 27.5);
    assert_eq!(window.limit(), 100.0);
    assert_eq!(window.unit(), "USD");
    assert!(!window.blocking());
    assert_eq!(window.span(), Some(Duration::from_secs(30 * 86_400)));
}

#[tokio::test]
async fn p7_step_fun_reads_windows_term_and_renews_an_expiring_session() {
    let server = MockServer::start().await;
    let account = "4161";
    let access = jwt(json!({"activated": true, "oasis_id": account, "exp": 1_793_548_815 + 120}));
    let refresh = jwt(json!({"device_id": "web-1", "oasis_id": account, "exp": 1_793_548_815 + 2_592_000}));
    let renewed = jwt(json!({"activated": true, "oasis_id": account, "exp": 1_793_548_815 + 3600}));
    let session = format!("{access}...{refresh}");
    Mock::given(method("POST"))
        .and(path("/passport/proto.api.passport.v1.PassportService/RefreshToken"))
        .and(header("oasis-appid", "20700"))
        .and(header("oasis-webid", "web-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accessToken": {"raw": renewed}, "refreshToken": {"raw": refresh}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour_usage_left_rate": 0.6, "five_hour_usage_reset_time": "1790680000",
            "weekly_usage_left_rate": "0.9", "weekly_usage_reset_time": "1791200000",
            "plan_credit_rate_limit": {"subscription_credit_left_rate": 0.87, "subscription_credit_reset_time": "1792000000"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/step.openapi.devcenter.Dashboard/GetStepPlanStatus"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"subscription": {"name": "Plus", "expired_at": "1793000000", "auto_renew": true}})))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let reader = Reader::default();
    let source = Source::StepFun { platform: server.uri(), app: "20700".to_owned() };
    let query = query().with_session_token(session);
    let cards = read_source(&reader, &query, &host, source).await.expect("Step Plan");
    let card = &cards[0];
    assert_eq!(card.plan(), "Plus");
    assert_eq!(card.windows().len(), 3);
    assert_eq!(card.windows()[0].used(), 40.0);
    assert!((card.windows()[1].used() - 10.0).abs() < 1e-9);
    assert_eq!(card.windows()[2].used(), 13.0);
    assert_eq!(card.renew(), "auto");
    assert!(card.until().is_some());
}

#[tokio::test]
async fn p8_volcengine_signs_and_reads_coding_and_agent_windows() {
    let expected = "HMAC-SHA256 Credential=AKLTexample/20261009/cn-beijing/ark/request, SignedHeaders=content-type;host;x-content-sha256;x-date, Signature=deb80c0cd8bd77768ca40f6d47c7204621c0994428f6ab77e23b76e15dc4c047";
    let at = hofund_plugin_sdk::time::parse_rfc3339("2026-10-09T08:30:15Z").expect("time");
    assert_eq!(volc_signature("AKLTexample", "c2VjcmV0LWV4YW1wbGU=", "cn-beijing", "GetCodingPlanUsage", at), expected);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(query_param("Action", "GetCodingPlanUsage"))
        .and(query_param("Version", "2024-01-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ResponseMetadata": {"RequestId": "r1"},
            "Result": {"Status": "ok", "QuotaUsage": [
                {"Level": "session", "Percent": 10.5, "ResetTimestamp": 1_758_100_000},
                {"Level": "weekly", "Percent": 20.25, "ResetTimestamp": 1_758_200_000},
                {"Level": "monthly", "Percent": "30.75", "ResetTimestamp": 1_758_300_000}
            ]}
        })))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::control_plans::read_volcengine(&query(), &host, &format!("{}/", server.uri()), "GetCodingPlanUsage", "Coding Plan", "cn-beijing")
        .await
        .expect("Coding Plan");
    assert_eq!(cards[0].windows().len(), 3);
    assert_eq!(cards[0].windows()[0].used(), 10.5);
    assert_eq!(cards[0].windows()[2].name(), "Month");

    Mock::given(method("POST"))
        .and(path("/"))
        .and(query_param("Action", "GetAFPUsage"))
        .and(query_param("Version", "2024-01-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ResponseMetadata": {"RequestId": "r2"},
            "Result": {"Tier": "Lite", "AFPFiveHour": {"Quota": 100, "Used": 42, "ResetTime": 1_790_000_000_000_i64}, "AFPWeekly": {"Used": "30", "Total": "120", "ResetTimestamp": 1_790_000_000_i64}}
        })))
        .mount(&server)
        .await;
    let agent = crate::control_plans::read_volcengine(&query(), &host, &format!("{}/", server.uri()), "GetAFPUsage", "Agent Plan", "cn-shanghai")
        .await
        .expect("Agent Plan");
    assert_eq!(agent[0].plan(), "Agent Plan Lite");
    assert_eq!(agent[0].windows()[0].amount(), 42.0);
    assert_eq!(agent[0].windows()[1].used(), 25.0);
}

#[tokio::test]
async fn p9_tencent_signs_lists_team_plans_and_reads_cycle_windows() {
    let payload = br#"{"Limit": 1, "Filters": [{"Values": ["\u672a\u547d\u540d"], "Name": "instance-name"}]}"#;
    let id = format!("AKID{}", "*".repeat(32));
    let signature = tc3_signature(&id, &"*".repeat(32), "cvm", "cvm.tencentcloudapi.com", "DescribeInstances", payload, UNIX_EPOCH + Duration::from_secs(1_551_113_065));
    assert!(signature.ends_with("10b1a37a7301a02ca19a647ad722d5e43b4b3cff309d421d85b46093f6ab6c4f"), "{signature}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("x-tc-action", "DescribeTokenPlanList"))
        .and(header("x-tc-version", "2026-03-22"))
        .and(header("x-tc-region", "ap-guangzhou"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Response": {"TokenPlanSet": [{
            "TeamId": "team-a", "Name": "alpha", "Status": "enable", "ProductType": "enterprise-auto"
        }]}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("x-tc-action", "DescribeTokenPlan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Response": {
            "TeamId": "team-a", "Name": "alpha", "Status": "enable", "ProductType": "enterprise-auto",
            "PackageInfo": {"TotalQuota": "1000000", "TotalUsed": "100000", "CycleQuota": "1000000", "CycleUnit": "month", "StartTime": "2026-04-01T00:00:00Z", "ExpireTime": "2027-04-01T00:00:00Z", "CurrentCycle": 1},
            "TokenSummary": {"CycleStartTime": "2026-04-01T00:00:00+08:00", "CycleEndTime": "2026-05-01T00:00:00+08:00"}
        }})))
        .mount(&server)
        .await;
    let (_root, host) = host();
    let cards = crate::control_plans::read_tencent(&query(), &host, &format!("{}/", server.uri()), "ap-guangzhou")
        .await
        .expect("Token Plan");
    let window = &cards[0].windows()[0];
    assert_eq!(window.name(), "Month");
    assert_eq!(window.used(), 10.0);
    assert_eq!(window.amount(), 100_000.0);
    assert_eq!(window.limit(), 1_000_000.0);
    assert_eq!(window.unit(), "tokens");
    assert_eq!(window.resets_at(), hofund_plugin_sdk::time::parse_rfc3339("2026-05-01T00:00:00+08:00"));
}

#[test]
fn p10_source_selection_covers_every_served_vendor_and_key_plan_urls() {
    let cases = [
        ("https://open.bigmodel.cn/api/coding/paas/v4", "Zhipu"),
        ("https://api.z.ai/api/coding/paas/v4", "Zhipu"),
        ("https://api.kimi.com/coding/v1", "Kimi"),
        ("https://api.kimi.ai/coding", "Kimi"),
        ("https://opencode.ai/zen/go/v1", "OpenCode"),
        ("https://opencode.ai/inference/go/openai/v1", "OpenCode"),
        ("https://api.commandcode.ai/provider/v1", "CommandCode"),
        ("https://api.minimax.cn/v1", "MiniMax"),
        ("https://api.minimaxi.com/v1", "MiniMax"),
        ("https://ollama.com/v1", "Ollama"),
        ("https://api.stepfun.ai/step_plan/v1", "StepFun"),
        ("https://ark.cn-beijing.volces.com/api/coding/v3", "Volcengine"),
        ("https://ark.cn-shanghai.volces.com/api/plan/v3", "Volcengine"),
        ("https://tokenhub.tencentmaas.com/plan/v3", "Tencent"),
        ("https://tokenhub-intl.tencentmaas.com/plan/v3", "Tencent"),
    ];
    for (base, expected) in cases {
        let query = PlanQuery::new(vec![base.to_owned()], "key");
        let source = crate::source(&query).unwrap_or_else(|| panic!("no source for {base}"));
        assert!(format!("{source:?}").contains(expected), "{base}: {source:?}");
    }
    for base in [
        "https://api.deepseek.com/v1",
        "https://open.bigmodel.cn/api/paas/v4",
        "https://opencode.ai/zen/v1",
        "https://ollama.com.evil.test/v1",
        "http://localhost:11434/v1",
        "https://ark.cn-beijing.volces.com/api/v3",
        "https://tokenhub.tencentmaas.com/v1",
        "https://api.lkeap.cloud.tencent.com/plan/v3",
    ] {
        let query = PlanQuery::new(vec![base.to_owned()], "key");
        if base.contains("open.bigmodel") {
            assert!(crate::source(&query).is_some());
        } else {
            assert!(crate::source(&query).is_none(), "{base}");
        }
    }
}
