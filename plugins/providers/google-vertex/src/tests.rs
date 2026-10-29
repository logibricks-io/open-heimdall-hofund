use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{
    BuildContext, CountOutcome, DynUpstreamFactory, ListKind, ListQuery, PluginErrorKind, PluginId,
    ProbeQuery, ProtocolId, RawTable, RawValue, Request, UpstreamFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Value, json};

use super::{VertexFactory, generation, probe_thinking};

const ROOT: &str = "http://127.0.0.1:9/v1/projects/my-proj/locations/us";
const TOKEN: &str = "http://127.0.0.1:9/token";
const IAM: &str = "http://127.0.0.1:9/iam/v1";
const MODEL: &str = "gemini-2.5-flash";

fn plugin() -> PluginId {
    PluginId::from_static("google-vertex")
}

fn raw(pairs: &[(&str, &str)]) -> RawTable {
    let mut raw = RawTable::new();
    for (key, value) in pairs {
        raw.insert(*key, RawValue::String((*value).to_owned()));
    }
    raw
}

/// Options for a test server: project `p` at `us`, every endpoint local.
fn local(credentials: &Path, impersonate: &str) -> RawTable {
    let credentials = credentials.display().to_string();
    raw(&[
        ("project", "my-proj"),
        ("location", "us"),
        ("base_url", "http://127.0.0.1:9"),
        ("credentials", &credentials),
        ("impersonate", impersonate),
        ("token_url", TOKEN),
        ("iam_root", IAM),
    ])
}

fn build(host: &TestHost, raw: &RawTable) -> Arc<dyn UpstreamProvider> {
    DynUpstreamFactory::validate_and_build_with(
        &VertexFactory,
        raw,
        host.context(),
        &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("build")
}

fn build_error(raw: &RawTable) -> String {
    let root = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), root.path());
    match DynUpstreamFactory::validate_and_build_with(
        &VertexFactory,
        raw,
        host.context(),
        &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
    ) {
        Ok(_) => panic!("must not build"),
        Err(error) => error.to_string(),
    }
}

fn write_creds(dir: &Path, body: &Value) -> PathBuf {
    let path = dir.join("adc.json");
    std::fs::write(&path, body.to_string()).expect("credentials");
    path
}

fn person() -> Value {
    json!({
        "type": "authorized_user",
        "client_id": "client",
        "client_secret": "client-secret",
        "refresh_token": "refresh-token",
    })
}

fn token_body(token: &str) -> String {
    json!({"access_token": token, "expires_in": 3600}).to_string()
}

const REPLY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#;

fn has_bearer(sensitive: &[String]) -> bool {
    sensitive.iter().any(|name| name == "authorization")
}

fn form(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).expect("form")
}

#[test]
fn a_refresh_token_is_minted_once_and_reused() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("ya29.minted"));
        host.http().enqueue(REPLY);
        host.http().enqueue(REPLY);
        let provider = build(&host, &local(&creds, ""));
        for _ in 0..2 {
            provider
                .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
                .await
                .expect("completion");
        }
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].uri(), TOKEN);
        let body = form(calls[0].body());
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=refresh-token"), "{body}");
        assert!(!has_bearer(calls[0].sensitive_headers()));
        for call in &calls[1..] {
            assert_eq!(
                call.uri(),
                format!("{ROOT}/publishers/google/models/{MODEL}:generateContent")
            );
            assert!(has_bearer(call.sensitive_headers()));
            assert!(
                !call
                    .sensitive_headers()
                    .iter()
                    .any(|name| name == "x-goog-api-key")
            );
            let sent: Value = hofund_plugin_sdk::json::from_slice(call.body()).expect("json");
            assert!(sent.get("model").is_none(), "{sent}");
            assert!(sent.get("stream").is_none(), "{sent}");
        }
    });
}

#[test]
fn a_service_account_signs_a_jwt_and_streams() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(
            dir.path(),
            &json!({
                "type": "service_account",
                "client_email": "worker@p.iam.gserviceaccount.com",
                "private_key": include_str!("fixtures/vertex-sa-pkcs8.pem"),
                "private_key_id": "key-1",
            }),
        );
        host.http().enqueue(token_body("ya29.signed"));
        host.http().enqueue("data: {}\n\n");
        let provider = build(&host, &local(&creds, ""));
        let _ = provider
            .execute(
                Request::new(MODEL).with_stream(true),
                ProtocolId::GENERATIVE_LANGUAGE,
            )
            .await;
        let calls = host.http().calls();
        assert_eq!(calls.len(), 2);
        let body = form(calls[0].body());
        assert!(
            body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
            "{body}"
        );
        assert!(body.contains("assertion=eyJ"), "{body}");
        assert_eq!(
            calls[1].uri(),
            format!("{ROOT}/publishers/google/models/{MODEL}:streamGenerateContent?alt=sse")
        );
        assert!(has_bearer(calls[1].sensitive_headers()));
    });
}

#[test]
fn impersonation_trades_the_token_and_counts() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("source-token"));
        host.http().enqueue(
            json!({"accessToken": "impersonated-token", "expireTime": "2026-10-25T07:00:00Z"})
                .to_string(),
        );
        host.http().enqueue(r#"{"totalTokens":9}"#);
        let provider = build(&host, &local(&creds, " Worker@Example.com "));
        let counted = provider
            .count_input(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .expect("count");
        assert!(matches!(counted, CountOutcome::Counted(9)), "{counted:?}");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[1].uri(),
            format!("{IAM}/projects/-/serviceAccounts/worker@example.com:generateAccessToken")
        );
        assert!(has_bearer(calls[1].sensitive_headers()));
        let body: Value = hofund_plugin_sdk::json::from_slice(calls[1].body()).expect("json");
        assert_eq!(body["lifetime"], "3600s");
        assert_eq!(
            body["scope"],
            json!(["https://www.googleapis.com/auth/cloud-platform"])
        );
        assert!(body.get("delegates").is_none(), "{body}");
        assert_eq!(
            calls[2].uri(),
            format!("{ROOT}/publishers/google/models/{MODEL}:countTokens")
        );
    });
}

#[test]
fn an_impersonated_credentials_file_names_its_delegates() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(
            dir.path(),
            &json!({
                "type": "impersonated_service_account",
                "service_account_impersonation_url": format!("{IAM}/projects/-/serviceAccounts/target%40p.iam.gserviceaccount.com:generateAccessToken"),
                "delegates": ["hop@p.iam.gserviceaccount.com"],
                "source_credentials": person(),
            }),
        );
        host.http().enqueue(token_body("source-token"));
        host.http()
            .enqueue(json!({"accessToken": "impersonated-token"}).to_string());
        host.http().enqueue(REPLY);
        let provider = build(&host, &local(&creds, ""));
        provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .expect("completion");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[1].uri(),
            format!(
                "{IAM}/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken"
            )
        );
        let body: Value = hofund_plugin_sdk::json::from_slice(calls[1].body()).expect("json");
        assert_eq!(
            body["delegates"],
            json!(["projects/-/serviceAccounts/hop@p.iam.gserviceaccount.com"])
        );
    });
}

#[test]
fn a_forbidden_impersonation_names_the_token_creator_role() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("source-token"));
        host.http()
            .enqueue_response(403, &[], json!({"error": "permission_denied"}).to_string());
        let provider = build(&host, &local(&creds, "worker@example.com"));
        let error = provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .err()
            .expect("forbidden");
        let message = error.to_string();
        assert!(
            message.contains("roles/iam.serviceAccountTokenCreator"),
            "{message}"
        );
        assert!(
            message.contains("impersonating worker@example.com"),
            "{message}"
        );
        assert_eq!(host.http().calls().len(), 2);
    });
}

#[test]
fn impersonation_errors_redact_tokens_and_response_bodies() {
    const SOURCE: &str = "SENTINEL-SOURCE-TOKEN-7731";
    const BODY: &str = "SENTINEL-BODY-7731";
    for (reply, expected) in [
        (json!({"detail": BODY}).to_string(), "no accessToken"),
        (format!("not-json {BODY}"), "not JSON"),
    ] {
        block_on(async {
            let dir = tempfile::tempdir().expect("scratch");
            let host = TestHost::open(plugin(), dir.path());
            let creds = write_creds(dir.path(), &person());
            host.http().enqueue(token_body(SOURCE));
            host.http().enqueue(reply);
            let provider = build(&host, &local(&creds, "worker@example.com"));
            let error = provider
                .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
                .await
                .err()
                .expect("bad IAM reply");
            let message = error.to_string();
            assert!(
                message.contains("impersonating worker@example.com"),
                "{message}"
            );
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains(BODY), "{message}");
            assert!(!message.contains(SOURCE), "{message}");
        });
    }
}

#[test]
fn a_refused_refresh_token_asks_to_sign_in_again() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue_response(
            400,
            &[],
            json!({"error": "invalid_grant", "error_description": "SECRET-DETAIL"}).to_string(),
        );
        let provider = build(&host, &local(&creds, ""));
        let error = provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .err()
            .expect("refused");
        let message = error.to_string();
        assert!(message.contains("the Google credentials in"), "{message}");
        assert!(
            message.contains("refused (400): invalid_grant"),
            "{message}"
        );
        assert!(message.contains("sign in again"), "{message}");
        assert!(!message.contains("SECRET-"), "{message}");
    });
}

#[test]
fn a_failed_mint_is_replayed_without_another_request() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue_response(503, &[], "{}");
        let provider = build(&host, &local(&creds, ""));
        for _ in 0..2 {
            let error = provider
                .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
                .await
                .err()
                .expect("down");
            assert!(error.to_string().contains("(503)"), "{error}");
            assert!(!error.to_string().contains("sign in again"), "{error}");
        }
        assert_eq!(host.http().calls().len(), 1);
    });
}

#[test]
fn a_credentials_file_created_after_construction_is_used() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = dir.path().join("adc.json");
        let provider = build(&host, &local(&creds, ""));
        let error = provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .err()
            .expect("missing");
        let message = error.to_string();
        assert!(message.contains("no Google credentials at"), "{message}");
        assert!(
            message.contains("clear it to use Application Default Credentials"),
            "{message}"
        );
        assert!(host.http().calls().is_empty());
        write_creds(dir.path(), &person());
        host.http().enqueue(token_body("ya29.late-file"));
        host.http().enqueue(REPLY);
        provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .expect("completion");
        assert_eq!(host.http().calls().len(), 2);
    });
}

#[test]
fn a_rewritten_credentials_file_is_read_again() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("first"));
        host.http().enqueue(REPLY);
        let provider = build(&host, &local(&creds, ""));
        provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .expect("first");
        let mut longer = person();
        longer["refresh_token"] = json!("a-longer-refresh-token");
        write_creds(dir.path(), &longer);
        host.http().enqueue(token_body("second"));
        host.http().enqueue(REPLY);
        provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .expect("second");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 4);
        assert!(form(calls[2].body()).contains("refresh_token=a-longer-refresh-token"));
    });
}

#[test]
fn workload_identity_an_api_key_and_odd_files_are_refused() {
    block_on(async {
        for (body, expected) in [
            (
                json!({"type": "external_account"}),
                "workload identity federation (external_account)",
            ),
            (
                json!({"type": "authorized_user"}),
                "they have no refresh token",
            ),
            (
                json!({"type": "service_account"}),
                "their client_email is missing",
            ),
            (json!({"type": "mystery"}), "they are of type \"mystery\""),
            (
                json!({"type": "authorized_user", "universe_domain": "example.com"}),
                "they are for example.com",
            ),
            (
                json!({"type": "impersonated_service_account"}),
                "names no service account",
            ),
            (
                json!({
                    "type": "impersonated_service_account",
                    "service_account_impersonation_url": "https://iam/v1/projects/-/serviceAccounts/a@p:generateAccessToken",
                }),
                "their source_credentials can't be read",
            ),
            (
                json!({
                    "type": "impersonated_service_account",
                    "service_account_impersonation_url": "https://iam/v1/projects/-/serviceAccounts/a@p:generateAccessToken",
                    "source_credentials": {
                        "type": "impersonated_service_account",
                        "service_account_impersonation_url": "https://iam/v1/projects/-/serviceAccounts/b@p:generateAccessToken",
                    },
                }),
                "source is another impersonation",
            ),
            (
                json!([1]),
                "holds no Google credentials: expected a JSON object",
            ),
        ] {
            let dir = tempfile::tempdir().expect("scratch");
            let host = TestHost::open(plugin(), dir.path());
            let creds = write_creds(dir.path(), &body);
            let provider = build(&host, &local(&creds, ""));
            let error = provider
                .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
                .await
                .err()
                .expect("refused");
            assert!(error.to_string().contains(expected), "{expected}: {error}");
            assert!(host.http().calls().is_empty(), "{expected}");
        }
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        host.secrets().insert(&plugin(), "api_key", "AIza-key");
        let creds = write_creds(dir.path(), &person());
        let provider = build(&host, &local(&creds, ""));
        let error = provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .err()
            .expect("api key");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(error.to_string().contains("not an API key"), "{error}");
        assert!(host.http().calls().is_empty());
    });
}

#[test]
fn a_large_credentials_file_is_refused_before_reading() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = dir.path().join("adc.json");
        std::fs::write(&creds, vec![b' '; 1024 * 1024 + 1]).expect("big");
        let provider = build(&host, &local(&creds, ""));
        let error = provider
            .execute(Request::new(MODEL), ProtocolId::GENERATIVE_LANGUAGE)
            .await
            .err()
            .expect("too large");
        assert!(error.to_string().contains("larger than 1 MiB"), "{error}");
    });
}

#[test]
fn the_probe_applies_the_thinking_rules() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("token"));
        let provider = build(&host, &local(&creds, ""));
        let models = [
            ("gemini-3-flash", Some(json!({"thinkingLevel": "low"}))),
            ("gemini-2.5-pro", Some(json!({"thinkingBudget": 128}))),
            ("gemini-2.5-flash", Some(json!({"thinkingBudget": 0}))),
            ("gemini-flash-latest", None),
        ];
        for (model, _) in &models {
            host.http().enqueue("{}");
            let query = ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, *model).expect("query");
            provider.probe(&query).await.expect("probe");
        }
        let calls = host.http().calls();
        assert_eq!(calls.len(), 5, "one mint, then a probe per model");
        for (call, (model, thinking)) in calls[1..].iter().zip(models) {
            assert_eq!(
                call.uri(),
                format!("{ROOT}/publishers/google/models/{model}:generateContent")
            );
            assert!(has_bearer(call.sensitive_headers()));
            let body: Value = hofund_plugin_sdk::json::from_slice(call.body()).expect("json");
            assert_eq!(body["generationConfig"]["maxOutputTokens"], 16, "{model}");
            assert_eq!(
                body["generationConfig"].get("thinkingConfig").cloned(),
                thinking,
                "{model}"
            );
        }
    });
}

#[test]
fn the_probe_sends_user_headers_as_sensitive() {
    block_on(async {
        let dir = tempfile::tempdir().expect("scratch");
        let host = TestHost::open(plugin(), dir.path());
        let creds = write_creds(dir.path(), &person());
        host.http().enqueue(token_body("token"));
        host.http().enqueue("{}");
        let mut options = local(&creds, "");
        let mut headers = RawTable::new();
        headers.insert(
            "X-Vertex-AI-LLM-Request-Type",
            RawValue::String("shared".to_owned()),
        );
        options.insert("headers", RawValue::Table(headers));
        let provider = build(&host, &options);
        let query = ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, MODEL).expect("query");
        provider.probe(&query).await.expect("probe");
        let calls = host.http().calls();
        assert_eq!(
            calls[1].sensitive_headers(),
            ["authorization", "x-vertex-ai-llm-request-type"]
        );
    });
}

#[test]
fn the_thinking_rule_reads_the_generation() {
    assert_eq!(generation("gemini-3.8-flash"), 3);
    assert_eq!(generation("gemini-4-argon"), 4);
    assert_eq!(generation("gemini-flash-latest"), 0);
    assert_eq!(generation("claude-3"), 0);
    assert_eq!(
        probe_thinking("Gemini-2.5-Pro-Preview"),
        Some(json!({"thinkingBudget": 128}))
    );
    assert_eq!(probe_thinking("gemini-1.5-pro"), None);
}

#[test]
fn the_static_list_follows_the_location_and_sends_nothing() {
    block_on(async {
        for (location, first) in [
            ("", "gemini-3.8-flash"),
            ("eu", "gemini-3.8-flash"),
            ("europe-west4", "gemini-2.5-pro"),
        ] {
            let dir = tempfile::tempdir().expect("scratch");
            let host = TestHost::open(plugin(), dir.path());
            let provider = build(
                &host,
                &raw(&[("project", "my-proj"), ("location", location)]),
            );
            let list = provider
                .list_models(&ListQuery::default())
                .await
                .expect("list");
            assert_eq!(list.kind(), ListKind::Catalog);
            assert_eq!(list.models()[0].id(), first, "{location}");
            assert!(host.http().calls().is_empty());
        }
    });
}

#[test]
fn build_checks_every_setting() {
    for (options, expected) in [
        (
            raw(&[]),
            "Google Vertex AI needs the id of your Google Cloud project",
        ),
        (raw(&[("project", "-x")]), "is no Google Cloud project id"),
        (
            raw(&[("project", "my-proj"), ("location", "us_east")]),
            "is no Vertex AI location",
        ),
        (
            raw(&[("project", "my-proj"), ("credentials", "adc.json")]),
            "credentials: must be a full path",
        ),
        (
            raw(&[("project", "my-proj"), ("impersonate", "worker")]),
            "impersonate: \"worker\" is no service account's email",
        ),
        (
            raw(&[
                ("project", "my-proj"),
                ("token_url", "http://example.com/t"),
            ]),
            "token_url: must be an https URL",
        ),
        (
            raw(&[("project", "my-proj"), ("iam_root", "file:///x")]),
            "iam_root: must be an https URL",
        ),
        (
            raw(&[("project", "my-proj"), ("region", "us")]),
            "region: unknown key",
        ),
    ] {
        let error = build_error(&options);
        assert!(error.contains(expected), "{expected}: {error}");
    }
    let mut odd = raw(&[]);
    odd.insert("project", RawValue::Integer(1));
    assert!(build_error(&odd).contains("project: must be a string"));
    let mut odd = raw(&[("project", "my-proj")]);
    odd.insert("headers", RawValue::String("x".to_owned()));
    assert!(build_error(&odd).contains("headers must be a table"));
    let mut odd = raw(&[("project", "my-proj")]);
    let mut headers = RawTable::new();
    headers.insert("x-a", RawValue::Integer(1));
    odd.insert("headers", RawValue::Table(headers));
    assert!(build_error(&odd).contains("headers.x-a: header values must be strings"));
}

#[test]
fn only_generative_language_is_served() {
    let dir = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), dir.path());
    let options = raw(&[("project", "my-proj")]);
    let Err(error) = DynUpstreamFactory::validate_and_build_with(
        &VertexFactory,
        &options,
        host.context(),
        &BuildContext::new(ProtocolId::OPENAI_CHAT),
    ) else {
        panic!("chat must not build");
    };
    assert!(
        error.to_string().contains("does not declare protocol"),
        "{error}"
    );
    let config = UpstreamFactory::validate(&VertexFactory, &options).expect("valid");
    let Err(error) = UpstreamFactory::build_with(
        &VertexFactory,
        &config,
        host.context(),
        &BuildContext::new(ProtocolId::OPENAI_CHAT),
    ) else {
        panic!("chat must not build");
    };
    assert_eq!(
        error.to_string(),
        "google-vertex does not serve protocol `openai-chat`"
    );
    assert!(UpstreamFactory::build(&VertexFactory, &config, host.context()).is_ok());
}

#[test]
fn config_check_reports_a_missing_credentials_file() {
    let dir = tempfile::tempdir().expect("scratch");
    let missing = dir.path().join("none.json").display().to_string();
    let config = UpstreamFactory::validate(
        &VertexFactory,
        &raw(&[("project", "my-proj"), ("credentials", &missing)]),
    )
    .expect("valid");
    let error = UpstreamFactory::preflight(&VertexFactory, &config)
        .expect_err("missing")
        .to_string();
    assert!(error.contains("credentials: names no file"), "{error}");
    let config =
        UpstreamFactory::validate(&VertexFactory, &raw(&[("project", "my-proj")])).expect("valid");
    assert!(UpstreamFactory::preflight(&VertexFactory, &config).is_ok());
}

#[test]
fn the_schema_has_every_option_and_requires_none() {
    let schema = UpstreamFactory::config_schema(&VertexFactory);
    let text = format!("{schema:?}");
    for key in [
        "base_url",
        "project",
        "location",
        "credentials",
        "impersonate",
        "token_url",
        "iam_root",
    ] {
        assert!(text.contains(key), "{key}: {text}");
    }
}
