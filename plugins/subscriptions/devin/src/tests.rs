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
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
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
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
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
fn quota_status_body_is_capped_at_one_mib() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-quota-cap-key", "quota-cap@example.test");
    let mut body = b"{}".to_vec();
    body.resize(1024 * 1024, b' ');
    body.push(b'x');
    host.http().enqueue_response(200, &[], body);
    let windows = block_on(sub.quota(&id, host.context())).expect("capped quota");
    assert!(windows.is_empty());
    assert_eq!(host.http().calls().len(), 1);
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
fn messages_stream_forwards_claude_signatures_and_remembers_their_type() {
    for (label, model, expected) in [
        ("claude-signature@example.test", "claude-opus-5-5", 2),
        ("other-signature@example.test", "swe-2", 0),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_account(&host, "devin-signature-stream-key", label);
        enqueue_configs(&host);
        let signature = format!("signature-{label}");
        let split = signature.len() / 2;
        let body = [
            frame(0, &ProtoBuf::new().string(9, "thinking").finish()).expect("reasoning"),
            frame(0, &ProtoBuf::new().string(10, &signature[..split]).finish())
                .expect("first signature"),
            frame(
                0,
                &ProtoBuf::new()
                    .string(10, &signature[split..])
                    .string(21, "anthropic-bedrock")
                    .finish(),
            )
            .expect("second signature"),
            frame(0, &ProtoBuf::new().string(3, "done").finish()).expect("text"),
            frame(FLAG_END, b"{}").expect("end"),
        ]
        .concat();
        host.http().enqueue(body);
        let completion =
            execute(&host, &id, request(model), ProtocolId::ANTHROPIC_MESSAGES).expect("execute");
        let events = collect_events(completion).expect("events");
        let signatures: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::SignatureDelta(delta) => Some(delta.signature.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(signatures.len(), expected);
        assert_eq!(
            signatures.concat(),
            if expected == 0 { "" } else { &signature }
        );
        if expected != 0 {
            assert_eq!(crate::stream::sig_type_of(&signature), "anthropic-bedrock");
        }
    }
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
fn tool_only_reply_streams_calls_arguments_and_usage() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-tool-stream-key", "tool-only@example.test");
    enqueue_configs(&host);
    let start = ProtoBuf::new()
        .string(1, "call:1")
        .string(2, "lookup")
        .finish();
    let first_args = ProtoBuf::new().string(3, "{\"query\":").finish();
    let second_args = ProtoBuf::new().string(3, "\"hofund\"}").finish();
    let usage = ProtoBuf::new().varint(2, 27).varint(3, 12).finish();
    let body = [
        frame(0, &ProtoBuf::new().bytes(6, &start).finish()).expect("tool start"),
        frame(0, &ProtoBuf::new().bytes(6, &first_args).finish()).expect("first args"),
        frame(
            0,
            &ProtoBuf::new()
                .bytes(6, &second_args)
                .bytes(7, &usage)
                .varint(5, 10)
                .finish(),
        )
        .expect("second args"),
        frame(FLAG_END, b"{}").expect("end"),
    ]
    .concat();
    host.http().enqueue(body);
    let events = execute_events(&host, &id, request("swe-2"));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ContentBlockStart(start)
            if start.index == 2 && matches!(&start.block, hofund_plugin_api::BlockStart::ToolCall { id, name } if id == "dv_Y2FsbDox" && name == "lookup")
    )));
    let arguments: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolArgumentsDelta(delta) if delta.index == 2 => {
                Some(delta.partial_json.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(arguments, ["{\"query\":", "\"hofund\"}"]);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageDelta(delta)
            if delta.stop.as_ref().is_some_and(|stop| stop.reason == hofund_plugin_api::StopReason::ToolUse)
                && delta.usage.as_ref().is_some_and(|usage| usage.output_tokens == Some(12))
    )));
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
            false,
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
fn tool_calls_in_history_encode_even_without_tool_definitions() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-history-key", "tool-history@example.test");
    enqueue_configs(&host);
    host.http().enqueue(chat_body(3, "ok"));
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [{"role": "assistant", "content": [{
            "type": "tool_call", "id": "call-1", "name": "lookup", "arguments": {"form": "raw", "text": "{}"}}
        ]}]
    }))
    .expect("history");
    execute_events(&host, &id, request);
    let calls = host.http().calls();
    let outer = fields(&deframe(calls[1].body()).expect("frame").payload);
    let prompts: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 3)
        .map(|field| fields(field.as_bytes().expect("prompt")))
        .collect();
    assert_eq!(prompts.len(), 2);
    let call = prompts[0]
        .iter()
        .find(|field| field.number == 6)
        .and_then(|field| field.as_bytes())
        .expect("call");
    let call = fields(call);
    assert_eq!(call[0].as_string().as_deref(), Some("call-1"));
    assert_eq!(call[1].as_string().as_deref(), Some("lookup"));
    assert_eq!(call[2].as_string().as_deref(), Some("{}"));
    assert_eq!(prompts[1][1].as_varint(), Some(4));
    assert_eq!(
        prompts[1][2].as_string().as_deref(),
        Some("Tool use was interrupted and did not produce a result.")
    );
    assert_eq!(prompts[1][3].as_string().as_deref(), Some("call-1"));
    let tool = outer
        .iter()
        .find(|field| field.number == 10)
        .and_then(|field| field.as_bytes())
        .expect("history tool");
    let tool = fields(tool);
    assert_eq!(tool[0].as_string().as_deref(), Some("lookup"));
    assert_eq!(tool[1].as_string().as_deref(), Some("Tool"));
    assert_eq!(
        tool[2].as_string().as_deref(),
        Some("{\"type\":\"object\",\"properties\":{}}")
    );
}

fn encoded_tool_request(request: Request) -> Vec<u8> {
    encoded_request_for(request, ProtocolId::OPENAI_CHAT)
}

fn encoded_request_for(request: Request, protocol: ProtocolId) -> Vec<u8> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(
        &host,
        &format!("devin-tool-request-key-{unique}"),
        &format!("tools-{unique}@example.test"),
    );
    enqueue_configs(&host);
    host.http().enqueue(chat_body(3, "ok"));
    let completion = execute(&host, &id, request, protocol).expect("execute");
    collect_events(completion).expect("events");
    host.http().calls()[1].body().to_vec()
}

fn decoded_chat_prompts(body: &[u8]) -> Vec<Vec<hofund_plugin_sdk::protobuf::Field>> {
    fields(&deframe(body).expect("frame").payload)
        .iter()
        .filter(|field| field.number == 3)
        .map(|field| fields(field.as_bytes().expect("prompt")))
        .collect()
}

fn prompt_images(prompt: &[hofund_plugin_sdk::protobuf::Field]) -> Vec<(String, String)> {
    prompt
        .iter()
        .filter(|field| field.number == 10)
        .map(|field| {
            let image = fields(field.as_bytes().expect("image"));
            (
                image[0].as_string().expect("data"),
                image[1].as_string().expect("media type"),
            )
        })
        .collect()
}

#[test]
fn structured_formats_append_the_reference_prose_to_instructions() {
    let object = "Respond with a single JSON object and nothing else.";
    let told = "Respond with a single JSON value and nothing else, matching this JSON schema:\n{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}},\"required\":[\"answer\"]}.";
    let schema = hofund_plugin_sdk::json::json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    for (key, value, expected) in [
        (
            "response_format",
            hofund_plugin_sdk::json::json!({"type":"json_schema","json_schema":{"name":"r","schema":schema.clone(),"strict":true}}),
            told,
        ),
        (
            "text",
            hofund_plugin_sdk::json::json!({"format":{"type":"json_schema","schema":schema}}),
            told,
        ),
        (
            "output_config",
            hofund_plugin_sdk::json::json!({"format":{"type":"json_object"}}),
            object,
        ),
        (
            "response_format",
            hofund_plugin_sdk::json::json!({"type":"json_schema","json_schema":{"name":"r"}}),
            object,
        ),
        (
            "response_format",
            hofund_plugin_sdk::json::json!({"type":"text"}),
            "",
        ),
    ] {
        let mut request: Request =
            hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
                "model": "swe-2",
                "stream": true,
                "messages": [
                    {"role": "system", "content": [{"type": "text", "text": "Be brief."}]},
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ]
            }))
            .expect("structured request");
        request.extensions.insert(key, value);
        let body = encoded_tool_request(request);
        let prompts = decoded_chat_prompts(&body);
        let expected = if expected.is_empty() {
            "Be brief.\n\nhi".to_owned()
        } else {
            format!("Be brief.\n\n{expected}\n\nhi")
        };
        assert_eq!(
            prompts[0]
                .iter()
                .find(|field| field.number == 3)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some(expected.as_str())
        );
    }
}

#[test]
fn inline_images_encode_and_url_only_images_are_ignored() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}
            ]},
            {"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.test/b.png"}}
            ]}
        ]
    }))
    .expect("images");
    let body = encoded_tool_request(request);
    let prompts = decoded_chat_prompts(&body);
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompt_images(&prompts[0]),
        [("AAAA".into(), "image/png".into())]
    );
}

#[test]
fn direct_tool_result_images_stay_on_the_tool_prompt() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "look"}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "shot", "arguments": {"form": "raw", "text": "{}"}}]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "c1", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "DDDD"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}
            ]}]}
        ]
    }))
    .expect("tool image");
    let body = encoded_tool_request(request);
    let prompts = decoded_chat_prompts(&body);
    assert_eq!(prompts.len(), 3);
    assert_eq!(
        prompt_images(&prompts[2]),
        [("DDDD".into(), "image/webp".into())]
    );
    assert_eq!(
        prompts[2]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("(no output)")
    );
}

#[test]
fn following_user_images_attach_to_their_noted_tool_result() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "shot"}]},
            {"role": "assistant", "content": [
                {"type": "tool_call", "id": "c1", "name": "read", "arguments": {"form": "raw", "text": "{}"}},
                {"type": "tool_call", "id": "c2", "name": "read", "arguments": {"form": "raw", "text": "{}"}}
            ]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "c1", "content": [{"type": "text", "text": "two pics\n\n[The tool returned 2 images; they follow in the next message.]"}]}]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "c2", "content": [{"type": "text", "text": "plain"}]}]},
            {"role": "user", "content": [
                {"type": "text", "text": "[From the result of read (tool call c1):]"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
                {"type": "text", "text": "what are these?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": "CCCC"}}
            ]}
        ]
    }))
    .expect("following images");
    let body = encoded_tool_request(request);
    let prompts = decoded_chat_prompts(&body);
    assert_eq!(prompts.len(), 5);
    assert_eq!(
        prompts[2]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("two pics")
    );
    assert_eq!(
        prompt_images(&prompts[2]),
        [
            ("AAAA".into(), "image/png".into()),
            ("BBBB".into(), "image/jpeg".into())
        ]
    );
    assert!(prompt_images(&prompts[3]).is_empty());
    assert_eq!(
        prompt_images(&prompts[4]),
        [("CCCC".into(), "image/gif".into())]
    );
    assert_eq!(
        prompts[4]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("what are these?")
    );
}

fn prompt_value(prompt: &[hofund_plugin_sdk::protobuf::Field], number: u32) -> Option<String> {
    prompt
        .iter()
        .find(|field| field.number == number)
        .and_then(|field| field.as_string())
}

fn prompt_role(prompt: &[hofund_plugin_sdk::protobuf::Field]) -> Option<u64> {
    prompt
        .iter()
        .find(|field| field.number == 2)
        .and_then(|field| field.as_varint())
}

#[test]
fn non_claude_reasoning_replays_unsigned_and_joins_split_calls() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "text": "First /a."},
                {"type": "tool_call", "id": "call_a", "name": "read", "arguments": {"form": "raw", "text": "{}"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "text": "And /b."},
                {"type": "tool_call", "id": "call_b", "name": "read", "arguments": {"form": "raw", "text": "{}"}}
            ]}
        ]
    }))
    .expect("reasoning history");
    let body = encoded_tool_request(request);
    let prompts = decoded_chat_prompts(&body);
    assert_eq!(
        prompt_value(&prompts[0], 11).as_deref(),
        Some("First /a.\n\nAnd /b.")
    );
    assert_eq!(prompt_value(&prompts[0], 12), None);
    assert_eq!(prompt_value(&prompts[0], 18), None);
    assert_eq!(
        prompts[0].iter().filter(|field| field.number == 6).count(),
        2
    );
}

#[test]
fn messages_replay_signed_thinking_only_on_claude_uids() {
    for (model, text, signature, expected_text, expected_signature) in [
        (
            "claude-opus-5-5",
            "Read it first.",
            "sig-one",
            Some("Read it first."),
            Some("sig-one"),
        ),
        ("claude-opus-5-5", "Unsigned", "", None, None),
        (
            "claude-opus-5-5",
            "",
            "bare-signature",
            None,
            Some("bare-signature"),
        ),
        (
            "swe-2",
            "Read it first.",
            "sig-one",
            Some("Read it first."),
            None,
        ),
    ] {
        let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": text, "signature": signature},
                    {"type": "tool_call", "id": "toolu_01", "name": "read", "arguments": {"form": "json", "value": {}}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "id": "toolu_01", "content": [{"type": "text", "text": "A"}]}]}
            ]
        }))
        .expect("thinking history");
        let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
        let prompts = decoded_chat_prompts(&body);
        let assistant = prompts
            .iter()
            .find(|prompt| prompt_role(prompt) == Some(2))
            .expect("assistant");
        assert_eq!(prompt_value(assistant, 11).as_deref(), expected_text);
        assert_eq!(prompt_value(assistant, 12).as_deref(), expected_signature);
        assert_eq!(
            prompt_value(assistant, 18).as_deref(),
            expected_signature.map(|_| "anthropic")
        );
    }
}

#[test]
fn thinking_after_text_gets_the_following_calls_prompt() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "text": "one", "signature": "sig-one"},
                {"type": "text", "text": "Reading it."},
                {"type": "thinking", "text": "two", "signature": "sig-two"},
                {"type": "tool_call", "id": "toolu_01", "name": "read", "arguments": {"form": "json", "value": {}}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "id": "toolu_01", "content": [{"type": "text", "text": "A"}]}]}
        ]
    }))
    .expect("ordered thinking");
    let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
    let prompts = decoded_chat_prompts(&body);
    let assistants: Vec<_> = prompts
        .iter()
        .filter(|prompt| prompt_role(prompt) == Some(2))
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(
        prompt_value(assistants[0], 3).as_deref(),
        Some("Reading it.")
    );
    assert_eq!(prompt_value(assistants[0], 11).as_deref(), Some("one"));
    assert_eq!(prompt_value(assistants[0], 12).as_deref(), Some("sig-one"));
    assert_eq!(prompt_value(assistants[1], 3), None);
    assert_eq!(prompt_value(assistants[1], 11).as_deref(), Some("two"));
    assert_eq!(prompt_value(assistants[1], 12).as_deref(), Some("sig-two"));
    assert_eq!(
        assistants[1]
            .iter()
            .filter(|field| field.number == 6)
            .count(),
        1
    );
}

#[test]
fn text_only_history_keeps_system_preamble_and_trailing_nudge() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Be brief"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "user", "content": []},
            {"role": "assistant", "content": [{"type": "text", "text": "done"}]}
        ]
    }))
    .expect("text history");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let prompts: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 3)
        .map(|field| fields(field.as_bytes().expect("prompt")))
        .collect();
    assert_eq!(prompts.len(), 3);
    assert_eq!(
        prompts[0]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("Be brief\n\nhi")
    );
    assert_eq!(
        prompts[2]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("Please proceed with the task.")
    );
}

#[test]
fn tool_definitions_use_instruction_descriptions_and_pointer_entries() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Follow directions"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ],
        "tools": [
            {"name": "lookup", "description": "  Look up values.  ", "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}},
            {"name": "silent", "input_schema": null}
        ]
    }))
    .expect("tool request");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let first = outer
        .iter()
        .find(|field| field.number == 3)
        .and_then(|field| field.as_bytes())
        .expect("first prompt");
    assert_eq!(
        fields(first)
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some(
            "Follow directions\n\n<tool_descriptions>\n<tool name=\"lookup\">\nLook up values.\n</tool>\n</tool_descriptions>\n\nhi"
        )
    );
    let tools: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 10)
        .map(|field| fields(field.as_bytes().expect("tool")))
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0][0].as_string().as_deref(), Some("lookup"));
    assert_eq!(
        tools[0][1].as_string().as_deref(),
        Some("Described under <tool name=\"lookup\"> in <tool_descriptions>, in the instructions.")
    );
    assert_eq!(
        tools[0][2].as_string().as_deref(),
        Some("{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}}}")
    );
    assert_eq!(tools[1][0].as_string().as_deref(), Some("silent"));
    assert_eq!(tools[1][1].as_string().as_deref(), Some(""));
    assert_eq!(
        tools[1][2].as_string().as_deref(),
        Some("{\"type\":\"object\",\"properties\":{}}")
    );
}

#[test]
fn split_assistant_calls_join_and_keep_their_real_results() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "read a and b"}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "toolu_a", "name": "read", "arguments": {"form": "raw", "text": "{\"path\":\"a\"}"}}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "reading"},
                {"type": "tool_call", "id": "toolu_b", "name": "read", "arguments": {"form": "raw", "text": "{\"path\":\"b\"}"}}
            ]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "toolu_a", "content": [{"type": "text", "text": "A!"}]}]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "toolu_b", "content": [{"type": "text", "text": "B!"}]}]},
            {"role": "user", "content": [{"type": "text", "text": "next"}]}
        ]
    }))
    .expect("split history");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let prompts: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 3)
        .map(|field| fields(field.as_bytes().expect("prompt")))
        .collect();
    let roles: Vec<_> = prompts
        .iter()
        .map(|prompt| {
            prompt
                .iter()
                .find(|field| field.number == 2)
                .and_then(|field| field.as_varint())
        })
        .collect();
    assert_eq!(roles, [Some(1), Some(2), Some(4), Some(4), Some(1)]);
    assert_eq!(
        prompts[1]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("reading")
    );
    let calls: Vec<_> = prompts[1]
        .iter()
        .filter(|field| field.number == 6)
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        fields(calls[0].as_bytes().expect("first call"))[0]
            .as_string()
            .as_deref(),
        Some("toolu_a")
    );
    assert_eq!(
        fields(calls[1].as_bytes().expect("second call"))[0]
            .as_string()
            .as_deref(),
        Some("toolu_b")
    );
    let answers: Vec<_> = prompts
        .iter()
        .filter_map(|prompt| prompt.iter().find(|field| field.number == 7))
        .map(|field| field.as_string().expect("answer"))
        .collect();
    assert_eq!(answers, ["toolu_a", "toolu_b"]);
    assert!(!String::from_utf8_lossy(&body).contains("Tool use was interrupted"));
}

#[test]
fn unanswered_calls_get_no_result_before_a_later_user_turn() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_call", "id": "call_a", "name": "read", "arguments": {"form": "raw", "text": "{}"}},
                {"type": "tool_call", "id": "call_b", "name": "read", "arguments": {"form": "raw", "text": "{}"}}
            ]},
            {"role": "tool", "content": [{"type": "tool_result", "id": "call_a", "content": [{"type": "text", "text": "A"}]}]},
            {"role": "user", "content": [{"type": "text", "text": "continue"}]}
        ]
    }))
    .expect("unanswered history");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let prompts: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 3)
        .map(|field| fields(field.as_bytes().expect("prompt")))
        .collect();
    assert_eq!(prompts.len(), 4);
    let results: Vec<_> = prompts
        .iter()
        .filter(|prompt| prompt.iter().any(|field| field.number == 7))
        .map(|prompt| {
            (
                prompt
                    .iter()
                    .find(|field| field.number == 7)
                    .and_then(|field| field.as_string())
                    .expect("id"),
                prompt
                    .iter()
                    .find(|field| field.number == 3)
                    .and_then(|field| field.as_string())
                    .expect("text"),
            )
        })
        .collect();
    assert_eq!(results[0].0, "call_a");
    assert_eq!(results[0].1, "A");
    assert_eq!(results[1].0, "call_b");
    assert_eq!(
        results[1].1,
        "Tool use was interrupted and did not produce a result."
    );
    assert_eq!(
        prompts[3]
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_string())
            .as_deref(),
        Some("continue")
    );
}

#[test]
fn array_tool_arguments_keep_their_json_value() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "messages": [{"role": "assistant", "content": [{
            "type": "tool_call", "id": "call_a", "name": "lookup",
            "arguments": {"form": "json", "value": [1, 2]}
        }]}]
    }))
    .expect("array arguments");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let prompt = outer
        .iter()
        .find(|field| field.number == 3)
        .and_then(|field| field.as_bytes())
        .expect("assistant");
    let call = fields(prompt)
        .into_iter()
        .find(|field| field.number == 6)
        .expect("call");
    assert_eq!(
        fields(call.as_bytes().expect("call bytes"))[2]
            .as_string()
            .as_deref(),
        Some("[1,2]")
    );
}

#[test]
fn tool_choice_none_omits_current_definitions_but_keeps_history_tools() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2",
        "stream": true,
        "tool_choice": {"mode": {"mode": "none"}},
        "tools": [{"name": "unused", "input_schema": {"type": "object"}}],
        "messages": [{"role": "assistant", "content": [{
            "type": "tool_call", "id": "call_a", "name": "used_before", "arguments": {"form": "raw", "text": "{}"}
        }]}]
    }))
    .expect("disabled tools");
    let body = encoded_tool_request(request);
    let outer = fields(&deframe(&body).expect("frame").payload);
    let tools: Vec<_> = outer
        .iter()
        .filter(|field| field.number == 10)
        .map(|field| fields(field.as_bytes().expect("tool")))
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0][0].as_string().as_deref(), Some("used_before"));
    assert_eq!(tools[0][1].as_string().as_deref(), Some("Tool"));
}

#[test]
fn encoded_tool_ids_decode_back_to_the_original_history_ids() {
    for (raw, encoded) in [
        ("call:1", "dv_Y2FsbDox"),
        ("#foo", "dv_I2Zvbw"),
        ("dv_abc", "dv_ZHZfYWJj"),
    ] {
        let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
            "model": "swe-2",
            "stream": true,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_call", "id": encoded, "name": "lookup", "arguments": {"form": "raw", "text": "{}"}}]},
                {"role": "tool", "content": [{"type": "tool_result", "id": encoded, "content": [{"type": "text", "text": "done"}]}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ]
        }))
        .expect("id history");
        let body = encoded_tool_request(request);
        let outer = fields(&deframe(&body).expect("frame").payload);
        let prompts: Vec<_> = outer
            .iter()
            .filter(|field| field.number == 3)
            .map(|field| fields(field.as_bytes().expect("prompt")))
            .collect();
        let call = prompts[0]
            .iter()
            .find(|field| field.number == 6)
            .expect("call");
        assert_eq!(
            fields(call.as_bytes().expect("call bytes"))[0]
                .as_string()
                .as_deref(),
            Some(raw)
        );
        assert_eq!(
            prompts[1]
                .iter()
                .find(|field| field.number == 7)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some(raw)
        );
    }
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

fn prompt_cache(prompt: &[hofund_plugin_sdk::protobuf::Field]) -> Option<Vec<u8>> {
    prompt
        .iter()
        .find(|field| field.number == 8)
        .and_then(|field| field.as_bytes())
        .map(<[u8]>::to_vec)
}

#[test]
fn messages_blocks_keep_their_boundaries_and_only_marks_encode_field_eight() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-haiku-5-5",
        "stream": true,
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.316.e5f; cc_entrypoint=cli; cch=8a1f2;\n"},
                {"type": "text", "text": "You are a security monitor.", "extensions": {"cache_control": {"type": "ephemeral"}}}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "<transcript>"},
                {"type": "text", "text": "history", "extensions": {"cache_control": {"type": "ephemeral"}}},
                {"type": "text", "text": "Current action: rm -rf build/"},
                {"type": "text", "text": "</transcript>\nClassify."}
            ]}
        ]
    }))
    .expect("marked Messages history");
    for (message, part) in [(0, 1), (1, 1)] {
        let ContentPart::Text(part) = &request.messages[message].content[part] else {
            panic!("marked text part");
        };
        assert!(
            part.extensions
                .get("cache_control")
                .is_some_and(hofund_plugin_sdk::json::Value::is_object)
        );
    }
    let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
    let prompts = decoded_chat_prompts(&body);
    let actual: Vec<_> = prompts
        .iter()
        .map(|prompt| {
            (
                prompt_role(prompt),
                prompt_value(prompt, 3),
                prompt_cache(prompt),
            )
        })
        .collect();
    assert_eq!(
        actual,
        [
            (
                Some(1),
                Some("You are a security monitor.".into()),
                Some(vec![8, 1])
            ),
            (Some(1), Some("<transcript>".into()), None),
            (Some(1), Some("history".into()), Some(vec![8, 1])),
            (Some(1), Some("Current action: rm -rf build/".into()), None),
            (Some(1), Some("</transcript>\nClassify.".into()), None),
        ]
    );
    assert!(prompts.iter().all(|prompt| {
        prompt_value(prompt, 3).is_none_or(|text| !text.contains("x-anthropic-billing-header"))
    }));
}

#[test]
fn moving_a_messages_mark_keeps_the_old_prompt_prefix_unchanged() {
    let first: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-haiku-5-5", "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "<transcript>"},
            {"type": "text", "text": "history A", "extensions": {"cache_control": {"type": "ephemeral"}}},
            {"type": "text", "text": "Current action: one"}
        ]}]
    }))
    .expect("first history");
    let second: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-haiku-5-5", "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "<transcript>"},
            {"type": "text", "text": "history A"},
            {"type": "text", "text": "history B", "extensions": {"cache_control": {"type": "ephemeral"}}},
            {"type": "text", "text": "Current action: two"}
        ]}]
    }))
    .expect("grown history");
    let first = decoded_chat_prompts(&encoded_request_for(first, ProtocolId::ANTHROPIC_MESSAGES));
    let second = decoded_chat_prompts(&encoded_request_for(second, ProtocolId::ANTHROPIC_MESSAGES));
    assert_eq!(prompt_value(&second[0], 3), prompt_value(&first[0], 3));
    assert_eq!(prompt_value(&second[1], 3), prompt_value(&first[1], 3));
    assert_eq!(prompt_cache(&second[1]), None);
    assert_eq!(prompt_cache(&first[1]), Some(vec![8, 1]));
    assert_eq!(prompt_value(&second[2], 3).as_deref(), Some("history B"));
    assert_eq!(prompt_cache(&second[2]), Some(vec![8, 1]));
}

#[test]
fn messages_system_notices_follow_real_results_without_losing_marks() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-opus-5-5", "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Read both files."}]},
            {"role": "assistant", "content": [
                {"type": "tool_call", "id": "c1", "name": "read_file", "arguments": {"form": "raw", "text": "{}"}},
                {"type": "tool_call", "id": "c2", "name": "read_file", "arguments": {"form": "raw", "text": "{}"}}
            ]},
            {"role": "system", "content": [{"type": "text", "text": "Stop after this read."}]},
            {"role": "system", "content": [{"type": "text", "text": "Report both results.", "extensions": {"cache_control": {"type": "ephemeral"}}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "id": "c1", "content": [{"type": "text", "text": "first"}]},
                {"type": "tool_result", "id": "c2", "content": [{"type": "text", "text": "second"}]}
            ]}
        ]
    }))
    .expect("system notices");
    let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
    let prompts = decoded_chat_prompts(&body);
    let actual: Vec<_> = prompts
        .iter()
        .map(|prompt| {
            (
                prompt_role(prompt),
                prompt_value(prompt, 3),
                prompt_cache(prompt),
            )
        })
        .collect();
    assert_eq!(
        actual,
        [
            (Some(1), Some("Read both files.".into()), None),
            (Some(2), None, None),
            (Some(4), Some("first".into()), None),
            (Some(4), Some("second".into()), None),
            (Some(1), Some("Stop after this read.".into()), None),
            (
                Some(1),
                Some("Report both results.".into()),
                Some(vec![8, 1])
            ),
        ]
    );
    let calls = prompts[1].iter().filter(|field| field.number == 6).count();
    assert_eq!(calls, 2);
}

#[test]
fn empty_messages_notices_do_not_add_prompts_or_split_tool_calls() {
    for content in [
        hofund_plugin_sdk::json::json!([]),
        hofund_plugin_sdk::json::json!([{"type": "text", "text": ""}]),
    ] {
        let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
            "model": "claude-opus-5-5", "stream": true,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read both."}]},
                {"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "read", "arguments": {"form": "raw", "text": "{}"}}]},
                {"role": "system", "content": content},
                {"role": "assistant", "content": [{"type": "tool_call", "id": "c2", "name": "read", "arguments": {"form": "raw", "text": "{}"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "id": "c1", "content": [{"type": "text", "text": "first"}]},
                    {"type": "tool_result", "id": "c2", "content": [{"type": "text", "text": "second"}]}
                ]}
            ]
        }))
        .expect("empty system notice");
        let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
        let prompts = decoded_chat_prompts(&body);
        assert_eq!(prompts.len(), 4);
        assert_eq!(prompt_role(&prompts[1]), Some(2));
        assert_eq!(
            prompts[1].iter().filter(|field| field.number == 6).count(),
            2
        );
        assert_eq!(prompt_role(&prompts[2]), Some(4));
        assert_eq!(prompt_role(&prompts[3]), Some(4));
    }
}

#[test]
fn messages_tool_results_keep_error_text_images_and_cache_marks() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "claude-opus-5-5", "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Read /etc/motd."}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "toolu_01", "name": "read", "arguments": {"form": "raw", "text": "{}"}}]},
            {"role": "user", "content": [{
                "type": "tool_result", "id": "toolu_01", "is_error": true,
                "content": [
                    {"type": "text", "text": "exit status 1"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}, "extensions": {"cache_control": {"type": "ephemeral"}}}
                ]
            }]}
        ]
    }))
    .expect("failed marked result");
    let body = encoded_request_for(request, ProtocolId::ANTHROPIC_MESSAGES);
    let prompts = decoded_chat_prompts(&body);
    let result = prompts
        .iter()
        .find(|prompt| prompt_role(prompt) == Some(4))
        .expect("tool result");
    assert_eq!(
        prompt_value(result, 3).as_deref(),
        Some("Error: exit status 1")
    );
    assert_eq!(prompt_cache(result), Some(vec![8, 1]));
    assert_eq!(prompt_images(result), [("AAAA".into(), "image/png".into())]);
}

#[test]
fn chat_history_still_joins_text_without_cache_marks() {
    let request: Request = hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "swe-2", "stream": true,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b", "extensions": {"cache_control": {"type": "ephemeral"}}}
        ]}]
    }))
    .expect("chat history");
    let body = encoded_tool_request(request);
    let prompts = decoded_chat_prompts(&body);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompt_value(&prompts[0], 3).as_deref(), Some("a\n\nb"));
    assert_eq!(prompt_cache(&prompts[0]), None);
}
