//! Usage page keys and typed control calls against a local fake server.

use std::convert::Infallible;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_proto::{
    CheckinAccountRow, CheckinListResponse, CheckinRunResponse, Quota, QuotaAlertsResponse,
    QuotaAlertsSetResponse, QuotaResponse, QuotaSpan, SettingsResponse, UsageBreakdownBy,
    UsageBreakdownGroup, UsagePeriod, UsageResponse, UsageRow,
};
use hofund_tui::app::App;
use hofund_tui::intent::ControlIntent;
use hofund_tui::session::{SessionOutcome, run_intent};
use hofund_tui::{PageId, Theme, buffer_text};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;

#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: Value,
}

#[tokio::test]
async fn period_keys_load_only_the_agent_breakdown() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (address, server) = serve(Arc::clone(&calls)).await;
    let client = hofund_control_client::Client::tcp(address, "token");
    let mut app = App::for_test(PageId::Usage);

    let _ = app.reload_open_page();
    drive(&client, &mut app).await;
    for (key, period) in [
        (KeyCode::Right, "week"),
        (KeyCode::Char('m'), "month"),
        (KeyCode::Char('A'), "all"),
        (KeyCode::Char('t'), "day"),
        (KeyCode::Char('w'), "week"),
        (KeyCode::Left, "day"),
    ] {
        app.handle_key(KeyEvent::new(key, KeyModifiers::empty()));
        drive(&client, &mut app).await;
        let expected = format!("/v1/hofund/usage?period={period}&by=agent");
        assert!(
            calls
                .lock()
                .expect("calls")
                .iter()
                .any(|call| call.path == expected),
            "missing {expected}"
        );
    }
    let calls = calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .filter(|call| call.path.starts_with("/v1/hofund/usage?"))
            .all(|call| call.path.ends_with("&by=agent"))
    );
    assert!(!calls.iter().any(|call| call.path.contains("provider-key")));
    assert!(!calls.iter().any(|call| call.path.contains("gateway-key")));
    server.abort();
}

#[test]
fn quota_open_gap_retry_cap_and_fresh_reload() {
    let mut app = App::for_test(PageId::Agents);
    app.handle_key(key('4'));
    let first = quota_intent(app.take_intents());
    assert!(matches!(
        first,
        ControlIntent::LoadQuotas {
            kept: false,
            delay: false,
            ..
        }
    ));
    let ControlIntent::LoadQuotas { token, .. } = first else {
        unreachable!()
    };

    let deferred = QuotaResponse {
        data: vec![quota(true)],
        ..QuotaResponse::default()
    };
    app.apply_session(SessionOutcome::Quotas {
        token,
        result: Ok(deferred.clone()),
    });
    let retry = quota_intent(app.take_intents());
    assert!(matches!(
        retry,
        ControlIntent::LoadQuotas {
            kept: true,
            delay: true,
            ..
        }
    ));
    assert_eq!(app.pages.usage().quota_tries(), 1);

    for expected in 2..=20 {
        app.apply_session(SessionOutcome::Quotas {
            token,
            result: Ok(deferred.clone()),
        });
        assert_eq!(app.pages.usage().quota_tries(), expected);
        assert!(matches!(
            quota_intent(app.take_intents()),
            ControlIntent::LoadQuotas {
                kept: true,
                delay: true,
                ..
            }
        ));
    }
    app.apply_session(SessionOutcome::Quotas {
        token,
        result: Ok(deferred),
    });
    assert_eq!(app.pages.usage().quota_tries(), 20);
    assert!(app.take_intents().is_empty());

    app.handle_key(key('1'));
    let _ = app.take_intents();
    app.handle_key(key('4'));
    assert!(matches!(
        quota_intent(app.take_intents()),
        ControlIntent::LoadQuotas {
            kept: true,
            delay: false,
            ..
        }
    ));

    app.handle_key(key('r'));
    assert!(app.pages.usage().quotas().is_none());
    assert!(matches!(
        quota_intent(app.take_intents()),
        ControlIntent::LoadQuotas {
            kept: false,
            delay: false,
            ..
        }
    ));
}

#[test]
fn quota_open_asks_again_at_thirty_seconds() {
    let mut page = hofund_tui::pages::UsagePage::new();
    let start = std::time::Instant::now();
    assert!(matches!(
        page.open_quotas(start),
        ControlIntent::LoadQuotas { kept: false, .. }
    ));
    assert!(matches!(
        page.open_quotas(start + std::time::Duration::from_secs(29)),
        ControlIntent::LoadQuotas { kept: true, .. }
    ));
    assert!(matches!(
        page.open_quotas(start + std::time::Duration::from_secs(30)),
        ControlIntent::LoadQuotas { kept: false, .. }
    ));
}

#[tokio::test]
async fn checkins_settings_alerts_and_rendering_use_control_data() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (address, server) = serve(Arc::clone(&calls)).await;
    let client = hofund_control_client::Client::tcp(address, "token");
    let mut app = App::for_test(PageId::Usage);
    app.handle_key(key('r'));
    drive(&client, &mut app).await;
    drive(&client, &mut app).await;

    let text = render(&app);
    assert!(text.contains("¥14.00"), "{text}");
    assert!(text.contains("mean ttft 125 ms"), "{text}");
    assert!(text.contains("mean output 42.5 tokens/s"), "{text}");
    assert!(text.contains("editor"), "{text}");
    assert!(text.contains("125"), "{text}");
    assert!(text.contains("40% used"), "{text}");
    assert!(text.contains("usage 80% used"), "{text}");
    assert!(text.contains("models"), "{text}");
    assert!(!text.contains("capped-row-model"), "{text}");
    assert!(!text.contains("incomplete"), "{text}");

    app.handle_key(key('c'));
    drive(&client, &mut app).await;
    let calls_now = calls.lock().expect("calls").clone();
    assert_eq!(
        calls_now
            .iter()
            .filter(|call| { call.method == "GET" && call.path == "/v1/hofund/checkins" })
            .count(),
        1
    );
    let runs: Vec<&Call> = calls_now
        .iter()
        .filter(|call| call.method == "POST" && call.path == "/v1/hofund/checkins/run")
        .collect();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].body["subscription"], "one");
    assert_eq!(runs[0].body["account"], "a@example.test");
    assert_eq!(runs[1].body["subscription"], "two");
    assert_eq!(runs[1].body["account"], "b@example.test");
    let flash = app.flash.as_ref().expect("check-in flash");
    assert!(flash.text.contains("check-in one/a@example.test: claimed"));
    assert!(flash.text.contains("+1 credit"));
    assert!(flash.text.contains("check-in two/b@example.test: done"));

    app.handle_key(key('u'));
    drive(&client, &mut app).await;
    assert!(app.pages.usage().quota_left());
    let calls_now = calls.lock().expect("calls").clone();
    let patch = calls_now
        .iter()
        .find(|call| call.method == "PATCH" && call.path == "/v1/hofund/settings")
        .expect("settings patch");
    assert_eq!(patch.body["quota_left"], true);
    assert_eq!(patch.body["expected_revision"], revision());
    let text = render(&app);
    assert!(text.contains("60% left"), "{text}");
    assert!(!text.contains("40% used"), "{text}");

    app.handle_key(key('a'));
    app.handle_key(KeyCode::Backspace.into());
    app.handle_key(KeyCode::Backspace.into());
    app.handle_key(key('9'));
    app.handle_key(key('0'));
    app.handle_key(KeyCode::Enter.into());
    drive(&client, &mut app).await;
    app.handle_key(key('b'));
    app.handle_key(KeyCode::Backspace.into());
    app.handle_key(key('0'));
    app.handle_key(KeyCode::Enter.into());
    drive(&client, &mut app).await;

    let calls_now = calls.lock().expect("calls").clone();
    let writes: Vec<&Call> = calls_now
        .iter()
        .filter(|call| call.method == "POST" && call.path == "/v1/hofund/quotas/alerts/settings")
        .collect();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(writes[0].body["usageAlert"], 90, "{writes:?}");
    assert!(writes[0].body.get("balanceAlert").is_none());
    assert_eq!(writes[1].body["balanceAlert"], 0.0, "{writes:?}");
    assert!(writes[1].body.get("usageAlert").is_none());
    assert!(
        calls_now
            .iter()
            .any(|call| { call.method == "GET" && call.path == "/v1/hofund/quotas/alerts" })
    );
    assert!(!app.pages.usage().captures());
    server.abort();
}

#[test]
fn missing_speed_fields_are_empty_cells_not_errors() {
    let mut app = App::for_test(PageId::Usage);
    let mut response = UsageResponse::default();
    response.totals.requests = 1;
    response.breakdown_by = Some(UsageBreakdownBy::Agent);
    let mut group = UsageBreakdownGroup::default();
    group.id = "plain".to_owned();
    group.requests = 1;
    response.breakdown = vec![group];
    app.apply_session(SessionOutcome::Usage {
        generation: app.generation(),
        result: Ok(response),
    });
    let text = render(&app);
    assert!(text.contains("plain"), "{text}");
    assert!(!text.contains("mean ttft"), "{text}");
    assert!(!text.contains("could not"), "{text}");
}

fn quota_intent(intents: Vec<ControlIntent>) -> ControlIntent {
    intents
        .into_iter()
        .find(|intent| matches!(intent, ControlIntent::LoadQuotas { .. }))
        .expect("quota intent")
}

fn quota(deferred: bool) -> Quota {
    Quota {
        provider: "one".to_owned(),
        name: "Account".to_owned(),
        read_deferred: deferred,
        windows: vec![QuotaSpan {
            name: "weekly".to_owned(),
            used: 40.0,
            remaining: 60.0,
            ..QuotaSpan::default()
        }],
        ..Quota::default()
    }
}

fn usage_response() -> UsageResponse {
    let mut response = UsageResponse::default();
    response.period = UsagePeriod::Day;
    response.currency = "CNY".to_owned();
    response.cny_per_usd = Some(7.0);
    response.rows_truncated = true;
    let mut row = UsageRow::default();
    row.model = "capped-row-model".to_owned();
    response.rows = vec![row];
    response.totals.requests = 2;
    response.totals.input_tokens = 100;
    response.totals.output_tokens = 50;
    response.totals.cost = 14.0;
    response.totals.mean_ttft_ms = Some(125.0);
    response.totals.mean_tokens_per_second = Some(42.5);
    response.breakdown_by = Some(UsageBreakdownBy::Agent);
    let mut group = UsageBreakdownGroup::default();
    group.id = "editor".to_owned();
    group.label = "editor".to_owned();
    group.requests = 2;
    group.input_tokens = 100;
    group.output_tokens = 50;
    group.cost = 14.0;
    group.mean_ttft_ms = Some(125.0);
    group.mean_tokens_per_second = Some(42.5);
    response.breakdown = vec![group];
    response
}

fn settings_response() -> SettingsResponse {
    let mut settings = SettingsResponse::default();
    settings.disk_revision = revision();
    settings.quota_left = false;
    settings
}

fn revision() -> String {
    "ab".repeat(32)
}

async fn drive(client: &hofund_control_client::Client, app: &mut App) {
    for intent in app.take_intents() {
        let outcome = run_intent(client, intent).await;
        app.apply_session(outcome);
    }
}

fn render(app: &App) -> String {
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            hofund_tui::ui::draw(app, frame.area(), frame.buffer_mut(), &Theme::monochrome())
        })
        .expect("draw");
    buffer_text(terminal.backend().buffer(), 120, 40)
}

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty())
}

async fn serve(calls: Arc<Mutex<Vec<Call>>>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let calls = Arc::clone(&calls);
                    async move { handle(request, calls).await }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (address, server)
}

async fn handle(
    request: http::Request<Incoming>,
    calls: Arc<Mutex<Vec<Call>>>,
) -> Result<http::Response<Full<Cursor<Vec<u8>>>>, Infallible> {
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_default();
    let bytes = request
        .into_body()
        .collect()
        .await
        .expect("request body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    calls.lock().expect("calls").push(Call {
        method: method.to_string(),
        path: path.clone(),
        body: body.clone(),
    });
    let payload = response_for(&method, &path, &body);
    let response = http::Response::builder()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Cursor::new(
            serde_json::to_vec(&payload).expect("response JSON"),
        )))
        .expect("response");
    Ok(response)
}

fn response_for(method: &http::Method, path: &str, body: &Value) -> Value {
    let base = path.split('?').next().unwrap_or(path);
    match (method.as_str(), base) {
        ("GET", "/v1/hofund/usage") => serde_json::to_value(usage_response()).expect("usage"),
        ("GET", "/v1/hofund/settings") => {
            serde_json::to_value(settings_response()).expect("settings")
        }
        ("PATCH", "/v1/hofund/settings") => {
            let mut settings = settings_response();
            settings.quota_left = body["quota_left"].as_bool().unwrap_or(false);
            settings.disk_revision = "cd".repeat(32);
            serde_json::to_value(settings).expect("settings")
        }
        ("GET", "/v1/hofund/quotas") => {
            let quotas = QuotaResponse {
                data: vec![quota(!path.contains("kept=1"))],
                ..QuotaResponse::default()
            };
            serde_json::to_value(quotas).expect("quotas")
        }
        ("GET", "/v1/hofund/quotas/alerts") => {
            let alerts = QuotaAlertsResponse {
                usage_alert: 80,
                balance_alert: 5.0,
                ..QuotaAlertsResponse::default()
            };
            serde_json::to_value(alerts).expect("alerts")
        }
        ("POST", "/v1/hofund/quotas/alerts/settings") => {
            let alerts = QuotaAlertsSetResponse {
                usage_alert: body["usageAlert"].as_u64().unwrap_or(80) as u8,
                balance_alert: body["balanceAlert"].as_f64().unwrap_or(5.0),
                ..QuotaAlertsSetResponse::default()
            };
            serde_json::to_value(alerts).expect("alerts")
        }
        ("GET", "/v1/hofund/checkins") => {
            let mut first = CheckinAccountRow::default();
            first.subscription = "one".to_owned();
            first.account = "a@example.test".to_owned();
            let mut second = CheckinAccountRow::default();
            second.subscription = "two".to_owned();
            second.account = "b@example.test".to_owned();
            let mut list = CheckinListResponse::default();
            list.accounts = vec![first, second];
            serde_json::to_value(list).expect("check-ins")
        }
        ("POST", "/v1/hofund/checkins/run") => {
            let mut response = CheckinRunResponse::default();
            response.outcome = if body["subscription"] == "one" {
                "claimed".to_owned()
            } else {
                "done".to_owned()
            };
            response.detail = if body["subscription"] == "one" {
                "+1 credit".to_owned()
            } else {
                "already today".to_owned()
            };
            response.at_ms = 1;
            serde_json::to_value(response).expect("check-in")
        }
        other => panic!("unexpected control request {other:?}"),
    }
}
