//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, BodyStream, Completion, ContentPart, PluginError, PluginErrorKind, PluginId,
    PluginKind, ProbeQuery, ProtocolId, RawTable, Request, SecretString, StreamEvent, Subscription,
    SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};
use hofund_plugin_sdk::protobuf::{FLAG_END, ProtoBuf, deframe, fields, frame};

use crate::creds::{self, Credential};
use crate::{DevinFactory, discover};

const MODELS_FIXTURE: &str = include_str!("fixtures/models.json");

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = DevinFactory.validate(&RawTable::new()).expect("config");
    DevinFactory.build(&cfg, host.context()).expect("build")
}

fn seed_account(host: &TestHost, key: &str, label: &str) -> AccountId {
    let credential = Credential {
        key: SecretString::from(key.to_owned()),
        api_server: "https://server.test".to_owned(),
        devin_api: "https://api.devin.test".to_owned(),
        label: label.to_owned(),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), label.to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn upstream_for(host: &TestHost, id: &AccountId) -> Arc<dyn UpstreamProvider> {
    subscription(host)
        .upstream(id, host.context())
        .expect("upstream")
}

fn enqueue_configs(host: &TestHost) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        crate::connect::testdata::MODEL_CONFIGS,
    );
}

fn chat_body(field: u32, text: &str) -> Vec<u8> {
    let message = ProtoBuf::new().string(field, text).varint(5, 3).finish();
    let mut body = frame(0, &message).expect("content frame");
    body.extend(frame(FLAG_END, b"{}").expect("end frame"));
    body
}

fn request(model: &str) -> Request {
    let mut request = ProbeQuery::new(ProtocolId::OPENAI_CHAT, model)
        .expect("query")
        .minimal_request();
    request.stream = true;
    request
}

fn execute(
    host: &TestHost,
    id: &AccountId,
    request: Request,
    protocol: ProtocolId,
) -> Result<Completion, PluginError> {
    block_on(upstream_for(host, id).execute(request, protocol))
}

fn collect_events(completion: Completion) -> Result<Vec<StreamEvent>, PluginError> {
    let Completion::Stream(mut stream) = completion else {
        panic!("expected an event stream");
    };
    block_on(async move {
        let mut events = Vec::new();
        while let Some(event) =
            std::future::poll_fn(|context| stream.as_mut().poll_next(context)).await
        {
            events.push(event?);
        }
        Ok(events)
    })
}

fn execute_events(host: &TestHost, id: &AccountId, request: Request) -> Vec<StreamEvent> {
    let completion = execute(host, id, request, ProtocolId::OPENAI_CHAT).expect("execute");
    collect_events(completion).expect("events")
}

#[test]
fn a2b_3_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join("devin");
    std::fs::create_dir_all(&home).expect("dir");
    std::fs::write(
        home.join("credentials.toml"),
        "windsurf_api_key = \"devin-import-key-1\"\n",
    )
    .expect("write");
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("devin-import-key-1")
    );
}

#[test]
fn a2b_4_refresh_is_no_op_for_api_key() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-static-key", "dev@example.test");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert!(calls.is_empty(), "refresh must not call the network");
}

#[test]
fn a2b_5_model_list_parsing() {
    let models = crate::models::parse_models(MODELS_FIXTURE.as_bytes()).expect("parse");
    assert!(models.iter().any(|model| model.id() == "swe-2"));
}

#[test]
fn a2b_6_upstream_executes_text_chat() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-upstream-key", "text-chat@example.test");
    enqueue_configs(&host);
    host.http().enqueue(chat_body(3, "hello"));
    let events = execute_events(&host, &id, request("swe-2"));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageStart(start) if start.model.as_deref() == Some("swe-2") && start.id.is_some()
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta(delta) if delta.text == "hello"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageDelta(delta)
            if delta.stop.as_ref().is_some_and(|stop| stop.reason == hofund_plugin_api::StopReason::MaxTokens)
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageStart(start) if start.usage.is_some()
    )));

    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    let chat = &calls[1];
    assert!(
        chat.uri()
            .ends_with("/exa.api_server_pb.ApiServerService/GetChatMessage")
    );
    assert_eq!(chat.header("connect-protocol-version"), Some("1"));
    assert_eq!(
        chat.header("content-type"),
        Some("application/connect+proto")
    );
    assert_eq!(chat.sensitive_headers(), ["authorization".to_owned()]);
    assert!(chat.header("authorization").is_none());
    let request_frame = deframe(chat.body()).expect("Connect request frame");
    let outer = fields(&request_frame.payload);
    assert_eq!(
        outer
            .iter()
            .find(|field| field.number == 21)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("swe-2-high")
    );
    let lower = String::from_utf8_lossy(chat.body()).to_ascii_lowercase();
    assert!(!lower.contains("hofund"));
    assert!(!lower.contains(&(String::from("mag") + "pie")));
}

#[test]
fn stream_finish_reports_usage_and_estimates_only_missing_output() {
    for (label, text, output, expected_output) in [
        ("reported-usage@example.test", "Let me look.", 12, 12),
        ("estimated-usage@example.test", "hello", 0, 2),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_account(&host, "devin-stream-usage-key", label);
        enqueue_configs(&host);
        let usage = ProtoBuf::new()
            .varint(2, 27)
            .varint(3, output)
            .varint(4, 560)
            .varint(5, 7318)
            .finish();
        let usage_message = ProtoBuf::new().bytes(7, &usage).finish();
        let text_message = ProtoBuf::new().string(3, text).finish();
        let body = [
            frame(0, &usage_message).expect("usage"),
            frame(0, &text_message).expect("text"),
            frame(FLAG_END, b"{}").expect("end"),
        ]
        .concat();
        host.http().enqueue(body);
        let events = execute_events(&host, &id, request("swe-2"));
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::MessageDelta(delta) if delta.stop.is_some() => delta.usage.as_ref(),
                _ => None,
            })
            .expect("finish usage");
        assert_eq!(usage.input_tokens, Some(7905));
        assert_eq!(usage.output_tokens, Some(expected_output));
        assert_eq!(usage.total_tokens, Some(7905 + expected_output));
        assert_eq!(usage.cache_read_tokens, Some(7318));
        assert_eq!(usage.cache_write_tokens, Some(560));
    }
}

#[test]
fn a2b_7_secrets_not_in_debug_or_errors() {
    let key = "devin-secret-key-xyzzy";
    let credential = Credential {
        key: SecretString::from(key.to_owned()),
        api_server: "https://server.test".to_owned(),
        devin_api: "https://api.devin.test".to_owned(),
        label: "dev@example.test".to_owned(),
    };
    let debug = format!("{credential:?}");
    assert!(!debug.contains(key));
    let error =
        creds::read_credentials(std::path::Path::new("/nonexistent/devin/credentials.toml"));
    let text = error.expect_err("missing").to_string();
    assert!(!text.contains(key));
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("credentials.toml");
    std::fs::write(
        &path,
        format!("windsurf_api_key = \"{key}\"\n# comment tail\n"),
    )
    .expect("write");
    let parsed = creds::read_credentials(&path).expect("parse");
    assert_eq!(
        secret_text(&parsed.key),
        key,
        "malformed comment must not truncate the key"
    );
    std::fs::write(&path, format!("windsurf_api_key = \"{key}\n")).expect("rewrite");
    let malformed = creds::read_credentials(&path).expect_err("unterminated");
    let malformed_text = malformed.to_string();
    assert!(!malformed_text.contains(key));
}

#[test]
fn a2b_8_quota_reads_windows_and_free_model_exclusions() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-quota-key", "dev@example.test");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"userStatus":{"planStatus":{"planInfo":{"billingStrategy":"BILLING_STRATEGY_QUOTA"},"dailyQuotaRemainingPercent":100,"dailyQuotaResetAtUnix":"1790841600","weeklyQuotaResetAtUnix":"1791100800"}}}"#,
    );
    enqueue_configs(&host);
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "1 day");
    assert_eq!(windows[0].used(), 0);
    assert_eq!(windows[0].limit(), Some(100));
    assert_eq!(windows[1].name(), "7 days");
    assert_eq!(windows[1].used(), 100);
    for window in &windows {
        assert_eq!(
            window.excluded_models(),
            ["swe-2", "swe-2-high", "swe-2-max", "swe-2-medium"]
        );
    }

    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].uri().ends_with("/GetUserStatus"));
    assert_eq!(calls[0].header("content-type"), Some("application/json"));
    assert_eq!(calls[0].header("connect-protocol-version"), Some("1"));
    assert!(calls[0].sensitive_headers().is_empty());
    assert!(calls[0].header("authorization").is_none());
    let request: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(calls[0].body()).expect("request JSON");
    assert_eq!(request["metadata"]["ideName"], "devin-cli");
    assert_eq!(request["metadata"]["extensionName"], "devin-cli");
    assert_eq!(request["metadata"]["ideVersion"], "3000.11.3");
    assert_eq!(request["metadata"]["extensionVersion"], "3000.11.3");
    assert_eq!(request["metadata"]["apiKey"], "devin-quota-key");
    assert_eq!(request["metadata"]["locale"], "en");
    assert!(request["metadata"]["os"].is_string());
}

#[test]
fn quota_failures_do_not_use_chat_or_raw_transport_classification() {
    enum Failure {
        Status(u16),
        Connect,
        Timeout,
    }
    for (label, failure, expected) in [
        (
            "quota-auth@example.test",
            Failure::Status(401),
            PluginErrorKind::Auth,
        ),
        (
            "quota-http@example.test",
            Failure::Status(500),
            PluginErrorKind::Status,
        ),
        (
            "quota-connect@example.test",
            Failure::Connect,
            PluginErrorKind::Io,
        ),
        (
            "quota-timeout@example.test",
            Failure::Timeout,
            PluginErrorKind::Io,
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let id = seed_account(&host, "devin-quota-failure-key", label);
        match failure {
            Failure::Status(status) => host.http().enqueue_response(
                status,
                &[],
                br#"{"code":"resource_exhausted","message":"quota exhausted"}"#,
            ),
            Failure::Connect => host.http().enqueue_connect("offline"),
            Failure::Timeout => host.http().enqueue_timeout("slow"),
        }
        let error = block_on(sub.quota(&id, host.context())).expect_err("quota failure");
        assert_eq!(error.kind(), expected, "{label}: {error}");
        assert!(!error.to_string().contains("devin-quota-failure-key"));
        assert_eq!(host.http().calls().len(), 1);
    }
}

#[test]
fn quota_unauthenticated_code_rejects_even_on_http_500() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-rejected-key", "rejected@example.test");
    host.http().enqueue_response(
        500,
        &[],
        br#"{"error":{"code":"unauthenticated","message":"expired"}}"#,
    );
    let error = block_on(sub.quota(&id, host.context())).expect_err("rejected");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(error.status(), None);
}

#[test]
fn unknown_free_models_leave_exhausted_windows_account_wide() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-unknown-free-key", "unknown-free@example.test");
    host.http().enqueue_response(
        200,
        &[],
        br#"{"userStatus":{"planStatus":{"planInfo":{"billingStrategy":"BILLING_STRATEGY_QUOTA"},"dailyQuotaResetAtUnix":"1790841600","weeklyQuotaResetAtUnix":"1791100800"}}}"#,
    );
    host.http().enqueue_response(503, &[], b"unavailable");
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 2);
    assert!(windows.iter().all(|window| window.used() == 100));
    assert!(
        windows
            .iter()
            .all(|window| window.excluded_models() == &[] as &[String])
    );
    assert_eq!(host.http().calls().len(), 2);
}

#[test]
fn no_quota_windows_or_empty_status_skip_the_model_rpc() {
    for (label, body) in [
        (
            "quota-credits@example.test",
            br#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Free","billingStrategy":"BILLING_STRATEGY_CREDITS"}}}}"#
                .as_slice(),
        ),
        ("quota-empty@example.test", b"".as_slice()),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let id = seed_account(&host, "devin-no-quota-key", label);
        host.http().enqueue_response(200, &[], body);
        let windows = block_on(sub.quota(&id, host.context())).expect("quota");
        assert!(windows.is_empty());
        assert_eq!(host.http().calls().len(), 1);
    }
}

#[test]
fn a2b_9_list_models_falls_back_to_snapshot() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-models-key", "snapshot-models@example.test");
    host.http().enqueue_connect("offline");
    let sub = subscription(&host);
    let models = block_on(sub.list_models(&id, host.context())).expect("models");
    assert!(models.models().iter().any(|model| model.id() == "swe-2"));
}

#[test]
fn a2b_35_reimport_replaces_stored_key() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join("devin");
    std::fs::create_dir_all(&home).expect("dir");
    let path = home.join("credentials.toml");
    std::fs::write(&path, "windsurf_api_key = \"devin-key-v1\"\n").expect("write");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    std::fs::write(&path, "windsurf_api_key = \"devin-key-v2\"\n").expect("rewrite");
    block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("reimport");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(
        SecretObject::parse(&secret)
            .expect("blob")
            .secret("api_key")
            .as_ref()
            .map(secret_text),
        Some("devin-key-v2")
    );
}

fn scripted_chat_error(label: &str, status: u16, body: &[u8]) -> PluginError {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-error-key", label);
    enqueue_configs(&host);
    host.http().enqueue_response(status, &[], body);
    match execute(&host, &id, request("swe-2"), ProtocolId::OPENAI_CHAT) {
        Err(error) => error,
        Ok(_) => panic!("scripted chat should fail"),
    }
}

#[test]
fn reasoning_delta_opens_stream_and_becomes_thinking() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-reasoning-key", "reasoning@example.test");
    enqueue_configs(&host);
    host.http().enqueue(chat_body(9, "considering"));
    let events = execute_events(&host, &id, request("swe-2"));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ThinkingDelta(delta) if delta.text == "considering"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(_)))
    );
}

#[test]
fn empty_and_metadata_only_replies_fail_before_stream() {
    let golden = scripted_chat_error(
        "empty-golden@example.test",
        200,
        crate::connect::testdata::EMPTY_REPLY,
    );
    let metadata = scripted_chat_error(
        "empty-metadata@example.test",
        200,
        &frame(FLAG_END, b"{}").expect("end"),
    );
    for error in [golden, metadata] {
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(502));
        assert_eq!(error.message(), crate::stream::EMPTY_REPLY_MESSAGE);
    }
}

#[test]
fn connect_and_http_errors_follow_section_six_order() {
    let empty = frame(0, b"").expect("empty message");
    let unauthenticated_body = [
        empty.clone(),
        frame(
            FLAG_END,
            br#"{"code":"unauthenticated","message":"expired"}"#,
        )
        .expect("end"),
    ]
    .concat();
    let unauthenticated =
        scripted_chat_error("unauthenticated@example.test", 200, &unauthenticated_body);
    assert_eq!(unauthenticated.kind(), PluginErrorKind::Status);
    assert_eq!(unauthenticated.status(), Some(401));
    assert!(unauthenticated.message().contains("sign in to Devin again"));

    let exhausted_body = [
        empty,
        frame(
            FLAG_END,
            br#"{"error":{"code":"resource_exhausted","message":"rate limit reached"}}"#,
        )
        .expect("end"),
    ]
    .concat();
    let exhausted = scripted_chat_error("exhausted@example.test", 200, &exhausted_body);
    assert_eq!(exhausted.kind(), PluginErrorKind::Quota);
    assert_eq!(exhausted.status(), Some(429));
    assert!(exhausted.message().starts_with("usage limit reached:"));

    let http_exhausted = scripted_chat_error(
        "http-exhausted@example.test",
        401,
        br#"{"code":"resource_exhausted","message":"quota exhausted"}"#,
    );
    assert_eq!(http_exhausted.kind(), PluginErrorKind::Quota);
    assert_eq!(http_exhausted.status(), Some(429));

    let rate_limit_body = frame(
        FLAG_END,
        br#"{"code":"rate_limit","message":"plain rate limit"}"#,
    )
    .expect("end");
    let rate_limit = scripted_chat_error("rate-limit@example.test", 200, &rate_limit_body);
    assert_eq!(rate_limit.kind(), PluginErrorKind::Status);
    assert_eq!(rate_limit.status(), Some(429));
    assert!(!rate_limit.message().starts_with("usage limit reached:"));
}

#[test]
fn tool_only_reply_is_a_router_safe_refusal_before_stream() {
    let message = ProtoBuf::new().bytes(6, b"call").finish();
    let body = [
        frame(0, &message).expect("tool"),
        frame(FLAG_END, b"{}").expect("end"),
    ]
    .concat();
    let error = scripted_chat_error("tool-only@example.test", 200, &body);
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(404));
    assert!(!error.cools());
    assert_eq!(error.message(), crate::stream::TOOLS_MESSAGE);
    assert_ne!(error.message(), crate::stream::EMPTY_REPLY_MESSAGE);
}

#[test]
fn tools_images_and_structured_output_are_refused_without_http() {
    let tool: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
    }))
    .expect("tool request");
    let image: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}
        ]}]
    }))
    .expect("image request");
    let mut structured = request("swe-2");
    structured.extensions.insert(
        "response_format",
        hofund_plugin_sdk::json::json!({"type": "json_object"}),
    );
    let mut responses_structured = request("swe-2");
    responses_structured.extensions.insert(
        "text",
        hofund_plugin_sdk::json::json!({"format": {"type": "json_schema"}}),
    );
    let mut messages_structured = request("swe-2");
    messages_structured.extensions.insert(
        "output_config",
        hofund_plugin_sdk::json::json!({"format": {"type": "json_object"}}),
    );
    for (label, request, phrase) in [
        (
            "tools-refusal@example.test",
            tool,
            crate::stream::TOOLS_MESSAGE,
        ),
        ("images-refusal@example.test", image, "image inputs"),
        (
            "structured-refusal@example.test",
            structured,
            "structured output",
        ),
        (
            "responses-structured-refusal@example.test",
            responses_structured,
            "structured output",
        ),
        (
            "messages-structured-refusal@example.test",
            messages_structured,
            "structured output",
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_account(&host, "devin-refusal-key", label);
        let error = match execute(&host, &id, request, ProtocolId::OPENAI_CHAT) {
            Err(error) => error,
            Ok(_) => panic!("request should be refused"),
        };
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(404), "{label}: {}", error.message());
        assert!(!error.cools());
        assert!(error.message().contains(phrase), "{label}");
        assert!(host.http().calls().is_empty(), "{label}");
    }
}

#[test]
fn live_model_list_uses_raw_protobuf_without_authorization() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-live-models-key", "live-models@example.test");
    enqueue_configs(&host);
    let models = block_on(subscription(&host).list_models(&id, host.context())).expect("models");
    assert!(models.models().iter().any(|model| model.id() == "swe-2"));
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert!(call.uri().ends_with("/GetCliModelConfigs"));
    assert_eq!(call.header("content-type"), Some("application/proto"));
    assert_eq!(call.header("connect-protocol-version"), Some("1"));
    assert!(call.sensitive_headers().is_empty());
    assert!(call.header("authorization").is_none());
    assert!(deframe(call.body()).is_err());
    assert_eq!(fields(call.body())[0].number, 1);
}

#[test]
fn live_model_list_marks_free_tier_and_prices() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-price-key", "prices@example.test");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        crate::connect::testdata::MODEL_CONFIGS_1015,
    );
    let models = block_on(subscription(&host).list_models(&id, host.context())).expect("models");
    let swe = models
        .models()
        .iter()
        .find(|model| model.id() == "swe-2")
        .expect("free family");
    assert_eq!(swe.free(), Some(true));
    assert!(swe.costs_nothing());
    let lightning = models
        .models()
        .iter()
        .find(|model| model.id() == "swe-2-high-lightning")
        .expect("priced variant");
    assert_eq!(lightning.free(), None);
    assert_eq!(
        lightning.prices(),
        (Some(1_000_000), Some(5_000_000), Some(100_000))
    );
    let argon = models
        .models()
        .iter()
        .find(|model| model.id() == "gemini-4-argon")
        .expect("priced family");
    assert_eq!(
        argon.prices(),
        (Some(1_000_000), Some(5_000_000), Some(50_000))
    );
}

#[test]
fn probe_succeeds_from_stored_credentials_without_http() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-probe-key", "probe@example.test");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "swe-2").expect("query");
    block_on(upstream_for(&host, &id).probe(&query)).expect("probe");
    assert!(host.http().calls().is_empty());
}

#[test]
fn large_chat_request_is_gzipped_and_marked() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-gzip-key", "gzip@example.test");
    enqueue_configs(&host);
    host.http().enqueue(chat_body(3, "ok"));
    let mut request = request("swe-2");
    request.messages[0].content[0] = ContentPart::text("x".repeat(9000));
    execute_events(&host, &id, request);
    let calls = host.http().calls();
    let chat = &calls[1];
    assert_eq!(chat.header("connect-content-encoding"), Some("gzip"));
    assert_eq!(deframe(chat.body()).expect("frame").flags & 1, 1);
}

#[test]
fn connect_and_timeout_before_chat_status_keep_transport_kinds() {
    for (label, timeout) in [
        ("chat-connect@example.test", false),
        ("chat-timeout@example.test", true),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_account(&host, "devin-transport-key", label);
        enqueue_configs(&host);
        if timeout {
            host.http().enqueue_timeout("slow");
        } else {
            host.http().enqueue_connect("offline");
        }
        let error = match execute(&host, &id, request("swe-2"), ProtocolId::OPENAI_CHAT) {
            Err(error) => error,
            Ok(_) => panic!("transport should fail"),
        };
        assert_eq!(
            error.kind(),
            if timeout {
                PluginErrorKind::Timeout
            } else {
                PluginErrorKind::Connect
            }
        );
    }
}

#[test]
fn body_failures_after_success_are_broken_reply_502s() {
    for opened in [false, true] {
        let body = BodyStream::unfold(0_u8, move |step| async move {
            match (step, opened) {
                (0, true) => {
                    let message = ProtoBuf::new().string(3, "partial").finish();
                    Some((Ok(frame(0, &message).expect("frame")), 1))
                }
                (0, false) | (1, true) => Some((Err(PluginError::interrupted("reset")), 2)),
                _ => None,
            }
        });
        let completion = block_on(crate::stream::open(
            body,
            "swe-2".to_owned(),
            ProtocolId::OPENAI_CHAT,
            true,
        ));
        let error = if opened {
            collect_events(completion.expect("stream opens on text")).expect_err("body error")
        } else {
            match completion {
                Err(error) => error,
                Ok(_) => panic!("body failure must not open a stream"),
            }
        };
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(502));
        assert!(error.message().starts_with("the reply broke off:"));
        assert_ne!(error.kind(), PluginErrorKind::Interrupted);
    }
}

#[test]
fn tool_calls_in_history_are_refused_even_without_tool_definitions() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-history-key", "tool-history@example.test");
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [{"role": "assistant", "content": [{
            "type": "tool_call", "id": "call-1", "name": "lookup", "arguments": {"form": "raw", "text": "{}"}}
        ]}]
    }))
    .expect("history");
    let error = match execute(&host, &id, request, ProtocolId::OPENAI_CHAT) {
        Err(error) => error,
        Ok(_) => panic!("history must be refused"),
    };
    assert_eq!(error.status(), Some(404));
    assert_eq!(error.message(), crate::stream::TOOLS_MESSAGE);
    assert!(host.http().calls().is_empty());
}

#[test]
fn messages_effort_uses_extensions_in_contract_order() {
    for (label, thinking, output, expected) in [
        (
            "messages-budget@example.test",
            Some(hofund_plugin_sdk::json::json!({"type": "enabled", "budget_tokens": 500})),
            None,
            "swe-2-medium",
        ),
        (
            "messages-output@example.test",
            None,
            Some(hofund_plugin_sdk::json::json!({"effort": "max"})),
            "swe-2-max",
        ),
        (
            "messages-disabled@example.test",
            Some(hofund_plugin_sdk::json::json!({"type": "disabled"})),
            Some(hofund_plugin_sdk::json::json!({"effort": "max"})),
            "swe-2-high",
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_account(&host, "devin-messages-key", label);
        enqueue_configs(&host);
        host.http().enqueue(chat_body(3, "ok"));
        let mut request = request("swe-2");
        if let Some(thinking) = thinking {
            request.extensions.insert("thinking", thinking);
        }
        if let Some(output) = output {
            request.extensions.insert("output_config", output);
        }
        let completion =
            execute(&host, &id, request, ProtocolId::ANTHROPIC_MESSAGES).expect("Messages execute");
        collect_events(completion).expect("events");
        let calls = host.http().calls();
        let outer = fields(&deframe(calls[1].body()).expect("frame").payload);
        assert_eq!(
            outer
                .iter()
                .find(|field| field.number == 21)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn cli_model_list_uses_isolated_credentials_and_removes_them() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-cli-key", "cli-models@example.test");
    let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
    let program = dir.path().join("devin-test-binary");
    std::fs::write(&program, b"scripted by the recording host").expect("program");
    let cwd = Arc::new(std::sync::Mutex::new(None));
    let recorded = Arc::clone(&cwd);
    host.process()
        .script_piped(PipedScript::new().line(MODELS_FIXTURE).exits(move |spawn| {
            assert!(spawn.program.is_absolute());
            assert!(spawn.cwd.is_absolute());
            assert_eq!(spawn.args, ["models", "list", "--format", "json"]);
            assert_eq!(spawn.env_var("XDG_DATA_HOME"), Some(spawn.cwd.as_os_str()));
            assert_eq!(spawn.env_var("APPDATA"), Some(spawn.cwd.as_os_str()));
            let credentials = spawn.cwd.join("devin").join("credentials.toml");
            let body = std::fs::read_to_string(&credentials).expect("credentials");
            assert!(body.contains("windsurf_api_key"));
            assert!(body.contains("devin_webapp_host"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&spawn.cwd)
                        .expect("root")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(&credentials)
                        .expect("credentials")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            *recorded.lock().expect("cwd") = Some(spawn.cwd.clone());
            Vec::new()
        }));
    let families = block_on(crate::cli::families_with_program(
        host.context(),
        &stored,
        &program,
    ))
    .expect("CLI families");
    assert_eq!(families[0].uid, "swe-2");
    let temp = cwd.lock().expect("cwd").clone().expect("spawn cwd");
    assert!(!temp.exists(), "the whole CLI temp root must be removed");
}
