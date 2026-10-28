//! Committed JSON snapshots and protocol compatibility tests.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use hofund_control_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

macro_rules! dto {
    ($type:ty, $($field:ident : $value:expr),* $(,)?) => {{
        let mut value = <$type>::default();
        $(value.$field = $value;)*
        value
    }};
}

const JSON_SNAPSHOTS: &[&str] = &[
    "agent-change-response",
    "agent-changed-event",
    "agent-connect-request",
    "agent-disconnect-request",
    "agent-list-response",
    "agent-plan-request",
    "agent-plan-response",
    "agent-status-response",
    "config-apply-request",
    "config-rejected-event",
    "config-reloaded-event",
    "config-response",
    "config-validate-request",
    "config-validation-response",
    "empty-request",
    "error-envelope",
    "health-response",
    "listener-relisten-failed-event",
    "member-cooled-event",
    "member-recovered-event",
    "model-list-response",
    "models-refreshed-event",
    "picks-response",
    "picks-set-request-edit",
    "picks-set-request-replace",
    "provider-list-response",
    "provider-test-request",
    "provider-test-response",
    "reload-request",
    "request-finished-event",
    "request-started-event",
    "resync-required-event",
    "route-model-list-response",
    "shutdown-request",
    "shutdown-response",
    "shutdown-started-event",
    "sign-in-progress-event",
    "snapshot-event",
    "snapshot-published-event",
    "status-response",
    "token-rotate-request",
    "token-rotate-response",
    "token-rotated-event",
    "usage-query",
    "usage-response",
];

const RESPONSE_SNAPSHOTS: &[&str] = &[
    "agent-change-response",
    "agent-changed-event",
    "agent-list-response",
    "agent-plan-response",
    "agent-status-response",
    "config-rejected-event",
    "config-reloaded-event",
    "config-response",
    "config-validation-response",
    "error-envelope",
    "health-response",
    "listener-relisten-failed-event",
    "member-cooled-event",
    "member-recovered-event",
    "model-list-response",
    "models-refreshed-event",
    "picks-response",
    "provider-list-response",
    "provider-test-response",
    "request-finished-event",
    "request-started-event",
    "resync-required-event",
    "route-model-list-response",
    "shutdown-response",
    "shutdown-started-event",
    "sign-in-progress-event",
    "snapshot-event",
    "snapshot-published-event",
    "status-response",
    "token-rotate-response",
    "token-rotated-event",
    "usage-response",
];

#[test]
fn json_snapshots_round_trip() {
    snapshot("empty-request", &EmptyRequest::new());
    snapshot(
        "config-validate-request",
        &ConfigValidateRequest::new(fake_toml()),
    );
    snapshot(
        "config-apply-request",
        &ConfigApplyRequest::new("disk-revision-1", fake_toml()),
    );
    snapshot("reload-request", &ReloadRequest::new("disk-revision-1"));
    snapshot(
        "provider-test-request",
        &ProviderTestRequest::new(Some(vec!["model-a".to_owned()])),
    );
    snapshot(
        "picks-set-request-replace",
        &PicksSetRequest::replace(
            "picks-revision-1",
            vec!["model-a".to_owned(), "model-b".to_owned()],
        ),
    );
    snapshot(
        "picks-set-request-edit",
        &PicksSetRequest::edit(
            "picks-revision-1",
            vec!["+model-c".to_owned(), "-model-a".to_owned()],
        ),
    );
    snapshot(
        "agent-plan-request",
        &AgentPlanRequest::new(AgentAction::Connect, false),
    );
    snapshot(
        "agent-connect-request",
        &AgentConnectRequest::new(Some("plan-revision-1".to_owned())),
    );
    snapshot(
        "agent-disconnect-request",
        &AgentDisconnectRequest::new(true, Some("plan-revision-2".to_owned())),
    );
    snapshot("usage-query", &UsageQuery::new(UsagePeriod::Month));
    snapshot("token-rotate-request", &TokenRotateRequest::new());
    snapshot("shutdown-request", &ShutdownRequest::new());

    let mut health = HealthResponse::default();
    health.daemon_version = "0.1.0".to_owned();
    snapshot("health-response", &health);

    let mut status = StatusResponse::default();
    status.pid = 7;
    status.started_at_ms = 1_700_000_000_000;
    status.model_listener.configured = "127.0.0.1:3425".to_owned();
    status.model_listener.effective = "127.0.0.1:4425".to_owned();
    status.model_listener.running = Some("127.0.0.1:3425".to_owned());
    status.model_listener.effective_source = ListenSource::Environment;
    status.generation = 4;
    status.disk_revision = "disk-revision-2".to_owned();
    status.runtime_revision = "runtime-revision-1".to_owned();
    status.pending_restart = vec!["token_file".to_owned()];
    status.not_applied = vec![listen_issue()];
    status.in_flight = 3;
    status.shutdown = ShutdownState::Draining;
    snapshot("status-response", &status);

    let mut config = ConfigResponse::default();
    config.disk_revision = "disk-revision-2".to_owned();
    config.runtime_revision = "runtime-revision-1".to_owned();
    config.generation = 4;
    config.text = fake_toml().to_owned();
    config.pending_restart = vec!["usage_file".to_owned()];
    config.not_applied = vec![listen_issue()];
    snapshot("config-response", &config);

    let mut validation = ConfigValidationResponse::default();
    validation.valid = false;
    validation.issues = vec![FieldIssue::new(
        "providers.0.base_url",
        "invalid",
        "base URL must be absolute",
    )];
    validation.pending_restart = vec!["control.listen".to_owned()];
    snapshot("config-validation-response", &validation);

    let mut providers = ProviderListResponse::default();
    providers.providers.push(dto!(ProviderSummary,
        id: "primary".to_owned(),
        kind: "built-in".to_owned(),
        protocol: "example-protocol".to_owned(),
        ready: true,
        model_count: 2,
    ));
    snapshot("provider-list-response", &providers);

    let mut provider_test = ProviderTestResponse::default();
    provider_test.provider = "primary".to_owned();
    provider_test.results.push(dto!(ProviderTestResult,
        model: "model-a".to_owned(),
        ok: false,
        status: Some(429),
        latency_ms: Some(12),
        error: Some(ControlError::new(
            ControlErrorKind::Upstream,
            "upstream rejected the example request",
            "request-1",
        )),
    ));
    snapshot("provider-test-response", &provider_test);

    let mut models = ModelListResponse::default();
    models.provider = "primary".to_owned();
    models.models.push(dto!(ModelEntry,
        id: "model-a".to_owned(),
        display_name: Some("Example Model A".to_owned()),
        context: Some(8192),
        max_output: Some(1024),
    ));
    models.fetched_at_ms = Some(1_700_000_001_000);
    models.added = vec!["model-b".to_owned()];
    models.removed = vec!["model-c".to_owned()];
    models.delisted = vec!["model-c".to_owned()];
    snapshot("model-list-response", &models);

    let mut picks = PicksResponse::default();
    picks.picks = vec!["model-a".to_owned()];
    picks.picked_from = vec!["model-a".to_owned(), "model-c".to_owned()];
    picks.delisted = vec!["model-c".to_owned()];
    picks.picks_revision = "picks-revision-2".to_owned();
    snapshot("picks-response", &picks);

    let mut routes = RouteModelListResponse::default();
    routes
        .models
        .push(dto!(RouteModel, id: "default".to_owned()));
    snapshot("route-model-list-response", &routes);

    let mut agents = AgentListResponse::default();
    agents.agents.push(dto!(AgentSummary,
        id: "example-agent".to_owned(),
        display_name: "Example Agent".to_owned(),
        installed: true,
    ));
    snapshot("agent-list-response", &agents);

    let mut agent_status = AgentStatusResponse::default();
    agent_status.agents.push(dto!(AgentStatusEntry,
        id: "example-agent".to_owned(),
        state: AgentState::Drifted,
        model: Some("default".to_owned()),
        paths: vec!["/home/example/.agent/config.json".to_owned()],
    ));
    snapshot("agent-status-response", &agent_status);

    let mut agent_plan = AgentPlanResponse::default();
    agent_plan.agent = "example-agent".to_owned();
    agent_plan.action = AgentAction::Connect;
    agent_plan.changes.push(dto!(PlannedChange,
        path: "/home/example/.agent/config.json".to_owned(),
        diff: "+ model = default".to_owned(),
    ));
    agent_plan.plan_revision = "plan-revision-1".to_owned();
    snapshot("agent-plan-response", &agent_plan);

    let mut agent_change = AgentChangeResponse::default();
    agent_change.agent = "example-agent".to_owned();
    agent_change.action = AgentAction::Disconnect;
    agent_change.changed_paths = vec!["/home/example/.agent/config.json".to_owned()];
    agent_change.warnings = vec!["example warning".to_owned()];
    snapshot("agent-change-response", &agent_change);

    let mut usage = UsageResponse::default();
    usage.period = UsagePeriod::Month;
    usage.rows.push(dto!(UsageRow,
        created_at_ms: 1_700_000_002_000,
        model: "default".to_owned(),
        provider: "primary".to_owned(),
        input_tokens: Some(10),
        output_tokens: Some(5),
        duration_ms: 25,
        status: "200".to_owned(),
    ));
    usage.totals = dto!(UsageTotals,
        requests: 1,
        input_tokens: 10,
        output_tokens: 5,
    );
    snapshot("usage-response", &usage);

    let mut rotated = TokenRotateResponse::default();
    rotated.token = "example-token-not-real".to_owned();
    rotated.agents.push(dto!(AgentRefreshResult,
        id: "example-agent".to_owned(),
        ok: true,
        message: None,
    ));
    snapshot("token-rotate-response", &rotated);

    let mut shutdown = ShutdownResponse::default();
    shutdown.draining = true;
    snapshot("shutdown-response", &shutdown);

    let mut envelope = ErrorEnvelope::default();
    envelope.error = ControlError::new(
        ControlErrorKind::Conflict,
        "configuration changed on disk",
        "request-1",
    );
    envelope.error.retry_after_ms = Some(25);
    envelope.error.details = vec![FieldIssue::new(
        "expected_revision",
        "stale",
        "reload and retry",
    )];
    snapshot("error-envelope", &envelope);

    let mut initial = SnapshotEvent::default();
    initial.disk_revision = "disk-revision-2".to_owned();
    initial.runtime_revision = "runtime-revision-1".to_owned();
    initial.generation = 4;
    initial.pending_restart = vec!["token_file".to_owned()];
    initial.not_applied = vec![listen_issue()];
    initial.members.push(dto!(MemberSummary,
        id: "member-1".to_owned(),
        ready: true,
    ));
    initial.cursors = vec![
        EventCursor::new(Topic::Control, 12),
        EventCursor::new(Topic::Requests, 40),
    ];
    event_snapshot("snapshot-event", EventPayload::Snapshot(initial));

    let mut reloaded = ConfigReloadedEvent::default();
    reloaded.disk_revision = "disk-revision-3".to_owned();
    reloaded.runtime_revision = "runtime-revision-3".to_owned();
    reloaded.generation = 5;
    reloaded.changed_members = vec!["member-1".to_owned()];
    reloaded.pending_restart = vec!["usage_file".to_owned()];
    reloaded.not_applied = vec![listen_issue()];
    event_snapshot(
        "config-reloaded-event",
        EventPayload::ConfigReloaded(reloaded),
    );

    let mut rejected = ConfigRejectedEvent::default();
    rejected.revision = "disk-revision-4".to_owned();
    rejected.issues = vec![FieldIssue::new("routes", "invalid", "invalid route")];
    event_snapshot(
        "config-rejected-event",
        EventPayload::ConfigRejected(rejected),
    );

    let mut listener = ListenerRelistenFailedEvent::default();
    listener.issue = FieldIssue::new("listen", "bind_failed", "example bind failure");
    listener.running = "127.0.0.1:3425".to_owned();
    listener.disk_revision = "disk-revision-3".to_owned();
    listener.runtime_revision = "runtime-revision-3".to_owned();
    listener.generation = 5;
    event_snapshot(
        "listener-relisten-failed-event",
        EventPayload::ListenerRelistenFailed(listener),
    );

    let mut cooled = MemberCooledEvent::default();
    cooled.member_id = "member-1".to_owned();
    cooled.cause = CooldownReason::RateLimit;
    cooled.retry_after_ms = Some(1000);
    cooled.remaining_ms = Some(900);
    cooled.failures = 2;
    event_snapshot("member-cooled-event", EventPayload::MemberCooled(cooled));
    event_snapshot(
        "member-recovered-event",
        EventPayload::MemberRecovered(dto!(MemberRecoveredEvent,
            member_id: "member-1".to_owned(),
            ready: true,
        )),
    );
    event_snapshot(
        "snapshot-published-event",
        EventPayload::SnapshotPublished(dto!(SnapshotPublishedEvent,
            generation: 5,
            routes: 1,
            members: 1,
        )),
    );
    event_snapshot(
        "request-started-event",
        EventPayload::RequestStarted(dto!(RequestStartedEvent,
            request_id: "request-1".to_owned(),
            route: "default".to_owned(),
            inbound_protocol: "example-protocol".to_owned(),
            started_at_ms: 1_700_000_003_000,
        )),
    );
    event_snapshot(
        "request-finished-event",
        EventPayload::RequestFinished(dto!(RequestFinishedEvent,
            request_id: "request-1".to_owned(),
            member: Some("member-1".to_owned()),
            status_class: StatusClass::Success,
            duration_ms: 20,
            input_tokens: Some(10),
            output_tokens: Some(5),
        )),
    );
    event_snapshot(
        "models-refreshed-event",
        EventPayload::ModelsRefreshed(dto!(ModelsRefreshedEvent,
            provider: "primary".to_owned(),
            list_kind: "live".to_owned(),
            model_count: 2,
            ok: true,
            error: None,
        )),
    );
    event_snapshot(
        "agent-changed-event",
        EventPayload::AgentChanged(dto!(AgentChangedEvent,
            agent: "example-agent".to_owned(),
            action: AgentAction::Connect,
            changed_paths: vec!["/home/example/.agent/config.json".to_owned()],
            warnings: vec!["example warning".to_owned()],
        )),
    );
    event_snapshot(
        "sign-in-progress-event",
        EventPayload::SignInProgress(dto!(SignInProgressEvent,
            provider: "primary".to_owned(),
            state: "waiting".to_owned(),
        )),
    );
    event_snapshot(
        "token-rotated-event",
        EventPayload::TokenRotated(TokenRotatedEvent::default()),
    );
    event_snapshot(
        "shutdown-started-event",
        EventPayload::ShutdownStarted(dto!(ShutdownStartedEvent, draining: true)),
    );
    event_snapshot(
        "resync-required-event",
        EventPayload::ResyncRequired(dto!(ResyncRequiredEvent,
            topic: Topic::Requests,
            reason: ResyncReason::Lagged,
            cursors: vec![
                EventCursor::new(Topic::Control, 13),
                EventCursor::new(Topic::Requests, 41),
            ],
        )),
    );
}

#[test]
fn snapshot_files_have_no_orphans_or_missing_entries() {
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        return;
    }
    let expected: BTreeSet<String> = JSON_SNAPSHOTS
        .iter()
        .map(|name| format!("{name}.json"))
        .collect();
    let actual: BTreeSet<String> = fs::read_dir(snapshot_dir())
        .expect("snapshot directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".json"))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn response_and_event_snapshots_accept_unknown_fields() {
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        return;
    }
    for name in RESPONSE_SNAPSHOTS {
        let text =
            fs::read_to_string(snapshot_dir().join(format!("{name}.json"))).expect("read snapshot");
        let mut value: Value = serde_json::from_str(&text).expect("parse snapshot");
        add_future_fields(&mut value);
        let body = serde_json::to_vec(&value).expect("encode modified snapshot");
        decode_response_snapshot(name, &body);
    }
}

#[test]
fn every_event_payload_data_is_an_object_and_tolerates_unknown_fields() {
    // This external-crate test cannot exhaustively match the non-exhaustive
    // enum, so keep this list beside the manual decoder's tag list.
    let known = vec![
        EventPayload::Snapshot(SnapshotEvent::default()),
        EventPayload::ConfigReloaded(ConfigReloadedEvent::default()),
        EventPayload::ConfigRejected(ConfigRejectedEvent::default()),
        EventPayload::ListenerRelistenFailed(ListenerRelistenFailedEvent::default()),
        EventPayload::MemberCooled(MemberCooledEvent::default()),
        EventPayload::MemberRecovered(MemberRecoveredEvent::default()),
        EventPayload::SnapshotPublished(SnapshotPublishedEvent::default()),
        EventPayload::RequestStarted(RequestStartedEvent::default()),
        EventPayload::RequestFinished(RequestFinishedEvent::default()),
        EventPayload::ModelsRefreshed(ModelsRefreshedEvent::default()),
        EventPayload::AgentChanged(AgentChangedEvent::default()),
        EventPayload::SignInProgress(SignInProgressEvent::default()),
        EventPayload::TokenRotated(TokenRotatedEvent::default()),
        EventPayload::ShutdownStarted(ShutdownStartedEvent::default()),
        EventPayload::ResyncRequired(ResyncRequiredEvent::default()),
    ];
    let decoder = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/events.rs"))
        .expect("read event decoder");
    assert_eq!(known.len(), decoder.matches("decode!(").count());

    for payload in known {
        let mut envelope = EventEnvelope::default();
        envelope.boot_id = "boot-example".to_owned();
        envelope.topic = payload.topic().expect("known event topic");
        envelope.payload = payload.clone();
        let mut value = serde_json::to_value(envelope).expect("encode envelope");
        let data = value["payload"]["data"]
            .as_object_mut()
            .expect("event data must be an object");
        data.insert("future".to_owned(), Value::from(1));
        let body = serde_json::to_vec(&value).expect("encode modified envelope");
        let decoded = decode_response::<EventEnvelope>(API_VERSION, &body)
            .expect("event with additive data field");
        assert_eq!(decoded.payload, payload);
    }
}

#[test]
fn unknown_response_enum_strings_use_fallbacks() {
    assert_eq!(
        serde_json::from_str::<ControlErrorKind>("\"future\"").expect("error kind"),
        ControlErrorKind::Unknown
    );
    assert_eq!(
        serde_json::from_str::<Topic>("\"future\"").expect("topic"),
        Topic::Unknown
    );
    assert_eq!(
        serde_json::from_str::<ListenSource>("\"future\"").expect("listen source"),
        ListenSource::Unknown
    );
    assert_eq!(
        serde_json::from_str::<ShutdownState>("\"future\"").expect("shutdown state"),
        ShutdownState::Unknown
    );
    assert_eq!(
        serde_json::from_str::<CooldownReason>("\"future\"").expect("cooldown"),
        CooldownReason::Unknown
    );
    assert_eq!(
        serde_json::from_str::<AgentAction>("\"future\"").expect("agent action"),
        AgentAction::Unknown
    );
    assert_eq!(
        serde_json::from_str::<AgentState>("\"future\"").expect("agent state"),
        AgentState::Unknown
    );
    assert_eq!(
        serde_json::from_str::<UsagePeriod>("\"future\"").expect("usage period"),
        UsagePeriod::Unknown
    );
    assert_eq!(
        serde_json::from_str::<StatusClass>("\"future\"").expect("status class"),
        StatusClass::Unknown
    );
    assert_eq!(
        serde_json::from_str::<ResyncReason>("\"future\"").expect("resync reason"),
        ResyncReason::Unknown
    );
    assert_eq!(
        serde_json::from_str::<EventPayload>(
            r#"{"event":"future_event","data":{"anything":[1,2,3]}}"#
        )
        .expect("event payload"),
        EventPayload::Unknown
    );
}

#[test]
fn body_requests_reject_unknown_fields() {
    reject_unknown::<ConfigValidateRequest>(r#"{"version":1,"text":"","future":true}"#);
    reject_unknown::<ConfigApplyRequest>(
        r#"{"version":1,"expected_revision":"r","text":"","future":true}"#,
    );
    reject_unknown::<ReloadRequest>(r#"{"version":1,"expected_revision":"r","future":true}"#);
    reject_unknown::<ProviderTestRequest>(r#"{"version":1,"models":null,"future":true}"#);
    reject_unknown::<PicksSetRequest>(
        r#"{"version":1,"expected_picks_revision":"r","ids":["a"],"edit":null,"future":true}"#,
    );
    reject_unknown::<AgentPlanRequest>(
        r#"{"version":1,"action":"connect","force":false,"future":true}"#,
    );
    reject_unknown::<AgentConnectRequest>(
        r#"{"version":1,"expected_plan_revision":null,"future":true}"#,
    );
    reject_unknown::<AgentDisconnectRequest>(
        r#"{"version":1,"force":false,"expected_plan_revision":null,"future":true}"#,
    );
    reject_unknown::<TokenRotateRequest>(r#"{"version":1,"future":true}"#);
    reject_unknown::<ShutdownRequest>(r#"{"version":1,"future":true}"#);
    let error = serde_json::from_str::<UsageQuery>(r#"{"period":"all","future":true}"#)
        .expect_err("query unknown field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn decode_checks_limits_versions_and_shapes_in_order() {
    let oversized = vec![b' '; MAX_REQUEST_BODY_BYTES + 1];
    let error = decode_request::<TokenRotateRequest>(99, &oversized).expect_err("body limit");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    let error =
        decode_request::<TokenRotateRequest>(2, br#"{"version":1}"#).expect_err("path version");
    assert_eq!(error.kind, ControlErrorKind::UnsupportedVersion);
    let error = decode_request::<TokenRotateRequest>(API_VERSION, br#"{"version":2}"#)
        .expect_err("body version");
    assert_eq!(error.kind, ControlErrorKind::UnsupportedVersion);
    let error =
        decode_request::<TokenRotateRequest>(API_VERSION, br#"{}"#).expect_err("missing version");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    let error =
        decode_request::<TokenRotateRequest>(API_VERSION, b"not json").expect_err("malformed JSON");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    let oversized_response = vec![b' '; MAX_RESPONSE_BODY_BYTES + 1];
    let error = decode_response::<HealthResponse>(API_VERSION, &oversized_response)
        .expect_err("response limit");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);

    let request = ConfigValidateRequest::new("x".repeat(MAX_CONFIG_TEXT_BYTES + 1));
    let body = serde_json::to_vec(&request).expect("encode large request");
    let error = decode_request::<ConfigValidateRequest>(API_VERSION, &body)
        .expect_err("decoded text limit");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert_eq!(error.details[0].field, "text");

    let mut response = ConfigResponse::default();
    response.text = "x".repeat(MAX_CONFIG_TEXT_BYTES + 1);
    let body = serde_json::to_vec(&response).expect("encode large response");
    let error = decode_response::<ConfigResponse>(API_VERSION, &body)
        .expect_err("decoded response text limit");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
}

#[test]
fn decode_errors_never_echo_request_values() {
    const SECRET: &str = "sk-live-0123456789abcdefSECRET";
    let number = "987654321";
    let mut errors = Vec::new();
    for body in [
        format!(r#"{{"version":1,"action":"connect","force":"{SECRET}"}}"#),
        format!(r#"{{"version":1,"action":"connect","force":{number}}}"#),
        format!(r#"{{"version":1,"action":"{SECRET}","force":false}}"#),
    ] {
        errors.push(
            decode_request::<AgentPlanRequest>(API_VERSION, body.as_bytes())
                .expect_err("malformed agent request"),
        );
    }
    for body in [
        format!(r#"{{"version":1,"text":"","{SECRET}":true}}"#),
        format!(r#"{{"version":1,"text":"{SECRET}"#),
        format!(r#"{{"version":1,"text":"{SECRET}",}}"#),
    ] {
        errors.push(
            decode_request::<ConfigValidateRequest>(API_VERSION, body.as_bytes())
                .expect_err("malformed config request"),
        );
    }
    for body in [
        format!(r#"{{"version":1,"draining":"{SECRET}"}}"#),
        format!(r#"{{"version":1,"draining":{number}}}"#),
    ] {
        errors.push(
            decode_response::<ShutdownResponse>(API_VERSION, body.as_bytes())
                .expect_err("wrong response type"),
        );
    }
    for body in [
        format!(r#"{{"version":1,"text":"{SECRET}"#),
        format!(r#"{{"version":1,"text":"{SECRET}",}}"#),
    ] {
        errors.push(
            decode_response::<ConfigResponse>(API_VERSION, body.as_bytes())
                .expect_err("malformed response"),
        );
    }
    for error in errors {
        assert!(!error.message.contains(SECRET), "{}", error.message);
        assert!(!error.message.contains(number), "{}", error.message);
        assert!(!format!("{error:?}").contains(SECRET), "{error:?}");
        assert!(!format!("{error:?}").contains(number), "{error:?}");
        for issue in error.details {
            assert!(!issue.field.contains(SECRET));
            assert!(!issue.reason.contains(SECRET));
            assert!(!issue.message.contains(SECRET));
            assert!(!issue.field.contains(number));
            assert!(!issue.reason.contains(number));
            assert!(!issue.message.contains(number));
        }
    }

    let error =
        decode_request::<TokenRotateRequest>(API_VERSION, br#"{"version":1,"colour":true}"#)
            .expect_err("unknown field");
    assert!(error.message.contains("unknown_field `colour`"));
    assert_eq!(error.details[0].field, "colour");
    assert_eq!(error.details[0].reason, "unknown_field");
}

#[test]
fn picks_forms_are_typed_and_validated() {
    let replace = PicksSetRequest::replace("r", vec!["a".to_owned()]);
    assert_eq!(
        replace.form().expect("replace"),
        PicksForm::Replace(vec!["a".to_owned()])
    );
    let edit = PicksSetRequest::edit("r", vec!["+a".to_owned(), "-b".to_owned()]);
    assert_eq!(
        edit.form().expect("edit"),
        PicksForm::Edit(vec![
            PickEdit::Add("a".to_owned()),
            PickEdit::Remove("b".to_owned())
        ])
    );
    for token in ["all", "-"] {
        assert_eq!(
            PicksSetRequest::edit("r", vec![token.to_owned()])
                .form()
                .expect("reset"),
            PicksForm::Reset
        );
    }
    for request in [
        PicksSetRequest::replace("r", Vec::new()),
        PicksSetRequest::replace("r", vec![String::new()]),
        PicksSetRequest::replace("r", vec!["none".to_owned()]),
        PicksSetRequest::edit("r", Vec::new()),
        PicksSetRequest::edit("r", vec!["none".to_owned()]),
    ] {
        assert_eq!(
            request.form().expect_err("semantic rejection").kind,
            ControlErrorKind::Unprocessable
        );
    }
    for token in ["a", "all,+a", "none,+a"] {
        let error = PicksSetRequest::edit("r", vec![token.to_owned()])
            .form()
            .expect_err("mixed form");
        assert_eq!(error.kind, ControlErrorKind::Unprocessable);
        assert_eq!(error.details[0].reason, "mixed_forms");
    }
    let mut both = PicksSetRequest::replace("r", vec!["a".to_owned()]);
    both.edit = Some(vec!["+b".to_owned()]);
    assert_eq!(
        both.form().expect_err("both forms").kind,
        ControlErrorKind::BadRequest
    );
    let mut neither = PicksSetRequest::replace("r", vec!["a".to_owned()]);
    neither.ids = None;
    assert_eq!(
        neither.form().expect_err("neither form").kind,
        ControlErrorKind::BadRequest
    );
}

#[test]
fn debug_output_never_contains_token_or_config_text() {
    let secret = "SECRET-CONFIG-TEXT-NOT-REAL";
    let token = "SECRET-TOKEN-NOT-REAL";
    let values = [
        format!("{:?}", ConfigValidateRequest::new(secret)),
        format!("{:?}", ConfigApplyRequest::new("r", secret)),
        format!("{:?}", {
            let mut response = ConfigResponse::default();
            response.text = secret.to_owned();
            response
        }),
        format!("{:?}", {
            let mut response = TokenRotateResponse::default();
            response.token = token.to_owned();
            response
        }),
    ];
    for output in values {
        assert!(!output.contains(secret), "{output}");
        assert!(!output.contains(token), "{output}");
    }
}

#[test]
fn control_error_construction_bounds_sensitive_metadata() {
    let error = ControlError::new(ControlErrorKind::Internal, "a".repeat(5000), "bad id");
    assert_eq!(error.message.len(), MAX_ERROR_MESSAGE_BYTES);
    assert_eq!(error.request_id, "invalid-request-id");
    let error = ControlError::new(ControlErrorKind::Internal, "valid", "request-1");
    assert_eq!(error.request_id, "request-1");
    assert_eq!(ControlErrorKind::Upstream.http_status(), 502);
    assert_eq!(ControlErrorKind::BadRequest.http_status(), 400);
    assert_eq!(ControlErrorKind::Unprocessable.http_status(), 422);
}

#[test]
fn event_payload_topics_are_stable() {
    assert_eq!(
        EventPayload::RequestStarted(RequestStartedEvent::default()).topic(),
        Some(Topic::Requests)
    );
    assert_eq!(
        EventPayload::RequestFinished(RequestFinishedEvent::default()).topic(),
        Some(Topic::Requests)
    );
    assert_eq!(
        EventPayload::Snapshot(SnapshotEvent::default()).topic(),
        Some(Topic::Control)
    );
    assert_eq!(EventPayload::Unknown.topic(), None);
}

fn reject_unknown<T: Request + std::fmt::Debug>(body: &str) {
    let error = decode_request::<T>(API_VERSION, body.as_bytes()).expect_err("unknown field");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert!(error.message.contains("unknown_field"), "{}", error.message);
}

fn event_snapshot(name: &str, payload: EventPayload) {
    let mut envelope = EventEnvelope::default();
    envelope.boot_id = "boot-example".to_owned();
    envelope.topic = payload.topic().expect("known payload topic");
    envelope.seq = 12;
    envelope.time_ms = 1_700_000_004_000;
    envelope.payload = payload;
    snapshot(name, &envelope);
}

fn snapshot<T: Serialize + DeserializeOwned>(name: &str, value: &T) {
    assert!(
        JSON_SNAPSHOTS.contains(&name),
        "unregistered snapshot {name}"
    );
    let path = snapshot_dir().join(format!("{name}.json"));
    let actual = pretty(value);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(snapshot_dir()).expect("create snapshot directory");
        fs::write(&path, &actual).expect("write snapshot");
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(actual, expected, "snapshot {name} drifted");
    let decoded: T = serde_json::from_str(&expected).expect("decode snapshot");
    assert_eq!(pretty(&decoded), expected, "snapshot {name} round trip");
}

fn pretty<T: Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("encode snapshot");
    text.push('\n');
    text
}

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

fn fake_toml() -> &'static str {
    "listen = \"127.0.0.1:3425\"\n"
}

fn listen_issue() -> FieldIssue {
    FieldIssue::new(
        "listen",
        "env_override",
        "HOFUND_ADDR overrides the configured listener",
    )
}

fn add_future_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.insert(
                "future_nested".to_owned(),
                Value::String("ignored".to_owned()),
            );
            for nested in object.values_mut() {
                add_future_fields(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                add_future_fields(item);
            }
        }
        _ => {}
    }
    if let Value::Object(object) = value {
        object.insert(
            "future_top".to_owned(),
            Value::Object(serde_json::Map::new()),
        );
    }
}

fn decode_response_snapshot(name: &str, body: &[u8]) {
    match name {
        "health-response" => decode::<HealthResponse>(body),
        "status-response" => decode::<StatusResponse>(body),
        "config-response" => decode::<ConfigResponse>(body),
        "config-validation-response" => decode::<ConfigValidationResponse>(body),
        "provider-list-response" => decode::<ProviderListResponse>(body),
        "provider-test-response" => decode::<ProviderTestResponse>(body),
        "model-list-response" => decode::<ModelListResponse>(body),
        "picks-response" => decode::<PicksResponse>(body),
        "route-model-list-response" => decode::<RouteModelListResponse>(body),
        "agent-list-response" => decode::<AgentListResponse>(body),
        "agent-status-response" => decode::<AgentStatusResponse>(body),
        "agent-plan-response" => decode::<AgentPlanResponse>(body),
        "agent-change-response" => decode::<AgentChangeResponse>(body),
        "usage-response" => decode::<UsageResponse>(body),
        "token-rotate-response" => decode::<TokenRotateResponse>(body),
        "shutdown-response" => decode::<ShutdownResponse>(body),
        "error-envelope" => decode::<ErrorEnvelope>(body),
        _ if name.ends_with("-event") => decode::<EventEnvelope>(body),
        _ => panic!("unregistered response snapshot {name}"),
    }
}

fn decode<T: Response>(body: &[u8]) {
    decode_response::<T>(API_VERSION, body).expect("forward-compatible response");
}

#[test]
fn unknown_request_enum_strings_are_rejected_not_reinterpreted() {
    let error = decode_request::<AgentPlanRequest>(
        API_VERSION,
        br#"{"version":1,"action":"conect","force":false}"#,
    )
    .expect_err("typoed action");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert_eq!(error.details.len(), 1);
    assert_eq!(error.details[0].field, "action");
    assert_eq!(error.details[0].reason, "unknown_variant");
    let plan = decode_request::<AgentPlanRequest>(
        API_VERSION,
        br#"{"version":1,"action":"disconnect","force":true}"#,
    )
    .expect("known action");
    assert_eq!(plan.action, AgentAction::Disconnect);

    let query: UsageQuery = serde_json::from_str(r#"{"period":"weak"}"#).expect("lenient enum");
    let error = query.validate().expect_err("typoed period");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert_eq!(error.details[0].field, "period");
    for period in [
        UsagePeriod::Day,
        UsagePeriod::Week,
        UsagePeriod::Month,
        UsagePeriod::All,
    ] {
        UsageQuery::new(period).validate().expect("known period");
    }
}

#[test]
fn every_error_kind_has_its_reviewed_http_status() {
    let table = [
        (ControlErrorKind::Unauthorized, 401),
        (ControlErrorKind::Forbidden, 403),
        (ControlErrorKind::NotFound, 404),
        (ControlErrorKind::Conflict, 409),
        (ControlErrorKind::BadRequest, 400),
        (ControlErrorKind::UnsupportedVersion, 400),
        (ControlErrorKind::InvalidConfig, 422),
        (ControlErrorKind::Unprocessable, 422),
        (ControlErrorKind::Unavailable, 503),
        (ControlErrorKind::Upstream, 502),
        (ControlErrorKind::Timeout, 504),
        (ControlErrorKind::Internal, 500),
        (ControlErrorKind::Unknown, 500),
    ];
    for (kind, status) in table {
        assert_eq!(kind.http_status(), status, "{kind:?}");
    }
}

#[test]
fn limits_match_the_plan_and_bound_config_text_at_the_edge() {
    assert_eq!(MAX_CONFIG_TEXT_BYTES, 1024 * 1024);
    assert_eq!(MAX_ERROR_MESSAGE_BYTES, 4096);
    assert_eq!(MAX_REQUEST_ID_LEN, 128);
    assert_eq!(MAX_REQUEST_BODY_BYTES, 6 * 1024 * 1024 + 64 * 1024);
    assert_eq!(MAX_RESPONSE_BODY_BYTES, MAX_REQUEST_BODY_BYTES);
    assert_eq!(MAX_IDS, 4096);
    assert_eq!(MAX_ID_BYTES, 256);
    assert_eq!(MAX_BOOT_ID_LEN, 64);

    let at_limit = serde_json::to_vec(&ConfigValidateRequest::new("x".repeat(1024 * 1024)))
        .expect("encode at limit");
    decode_request::<ConfigValidateRequest>(API_VERSION, &at_limit).expect("text at limit");
    let over = serde_json::to_vec(&ConfigApplyRequest::new("r", "x".repeat(1024 * 1024 + 1)))
        .expect("encode over limit");
    let error = decode_request::<ConfigApplyRequest>(API_VERSION, &over).expect_err("over limit");
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert_eq!(error.details[0].field, "text");
    assert_eq!(error.details[0].reason, "too_large");
}

#[test]
fn revision_fields_are_bounded_by_max_id_bytes() {
    let at = "r".repeat(MAX_ID_BYTES);
    let over = "r".repeat(MAX_ID_BYTES + 1);
    assert_revision_limit::<ConfigApplyRequest>(
        "expected_revision",
        serde_json::to_vec(&ConfigApplyRequest::new(at.clone(), "")).expect("encode apply"),
        serde_json::to_vec(&ConfigApplyRequest::new(over.clone(), "")).expect("encode apply"),
    );
    assert_revision_limit::<ReloadRequest>(
        "expected_revision",
        serde_json::to_vec(&ReloadRequest::new(at.clone())).expect("encode reload"),
        serde_json::to_vec(&ReloadRequest::new(over.clone())).expect("encode reload"),
    );
    assert_revision_limit::<PicksSetRequest>(
        "expected_picks_revision",
        serde_json::to_vec(&PicksSetRequest::replace(at.clone(), vec!["a".to_owned()]))
            .expect("encode picks"),
        serde_json::to_vec(&PicksSetRequest::replace(
            over.clone(),
            vec!["a".to_owned()],
        ))
        .expect("encode picks"),
    );
    assert_revision_limit::<AgentConnectRequest>(
        "expected_plan_revision",
        serde_json::to_vec(&AgentConnectRequest::new(Some(at.clone()))).expect("encode connect"),
        serde_json::to_vec(&AgentConnectRequest::new(Some(over.clone()))).expect("encode connect"),
    );
    assert_revision_limit::<AgentDisconnectRequest>(
        "expected_plan_revision",
        serde_json::to_vec(&AgentDisconnectRequest::new(false, Some(at.clone())))
            .expect("encode disconnect"),
        serde_json::to_vec(&AgentDisconnectRequest::new(false, Some(over)))
            .expect("encode disconnect"),
    );
}

#[test]
fn id_lists_enforce_the_max_ids_count() {
    let ids = (0..MAX_IDS)
        .map(|index| format!("m{index}"))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&PicksSetRequest::replace("r", ids.clone())).expect("encode ids");
    decode_request::<PicksSetRequest>(API_VERSION, &body).expect("ids at limit");
    let mut over_ids = ids;
    over_ids.push("overflow".to_owned());
    let body = serde_json::to_vec(&PicksSetRequest::replace("r", over_ids)).expect("encode ids");
    let error = decode_request::<PicksSetRequest>(API_VERSION, &body).expect_err("too many ids");
    assert_eq!(error.details[0].field, "ids");
    assert_eq!(error.details[0].reason, "too_many");

    let edits = (0..MAX_IDS)
        .map(|index| format!("+m{index}"))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&PicksSetRequest::edit("r", edits.clone())).expect("encode edit");
    decode_request::<PicksSetRequest>(API_VERSION, &body).expect("edit at limit");
    let mut over_edits = edits;
    over_edits.push("+overflow".to_owned());
    let body = serde_json::to_vec(&PicksSetRequest::edit("r", over_edits)).expect("encode edit");
    let error = decode_request::<PicksSetRequest>(API_VERSION, &body).expect_err("too many edits");
    assert_eq!(error.details[0].field, "edit");
    assert_eq!(error.details[0].reason, "too_many");
}

#[test]
fn edit_token_failures_report_specific_reasons() {
    let at_limit = format!("+{}", "m".repeat(MAX_ID_BYTES));
    PicksSetRequest::edit("r", vec![at_limit])
        .form()
        .expect("256-byte edit id");
    let over_limit = format!("+{}", "m".repeat(MAX_ID_BYTES + 1));
    for (token, reason) in [
        (String::new(), "invalid"),
        ("+".to_owned(), "invalid"),
        (over_limit, "too_large"),
        ("plain".to_owned(), "mixed_forms"),
        ("é".to_owned(), "mixed_forms"),
    ] {
        let error = PicksSetRequest::edit("r", vec![token])
            .form()
            .expect_err("invalid edit token");
        assert_eq!(error.details[0].field, "edit");
        assert_eq!(error.details[0].reason, reason);
    }
}

fn assert_revision_limit<T: Request + std::fmt::Debug>(
    field: &str,
    at_body: Vec<u8>,
    over_body: Vec<u8>,
) {
    decode_request::<T>(API_VERSION, &at_body).expect("revision at limit");
    let error = decode_request::<T>(API_VERSION, &over_body).expect_err("revision over limit");
    assert_eq!(error.kind, ControlErrorKind::Unprocessable);
    assert_eq!(error.details[0].field, field);
    assert_eq!(error.details[0].reason, "too_large");
}
