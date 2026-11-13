//! Providers and Agents control calls against a local fake control server.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use hofund_control_proto::{
    API_VERSION, AgentListResponse, AgentStatusResponse, ControlError, ControlErrorKind,
    ErrorEnvelope, ModelEntry, ModelListResponse, PicksResponse, PresetEntry, PresetKind,
    PresetListResponse, ProfileListResponse, ProviderDeleteResponse, ProviderListResponse,
    ProviderSummary, ProviderTestResponse, QuotaResponse, RouteModelListResponse, StatusResponse,
};
use hofund_tui::app::App;
use hofund_tui::intent::ControlIntent;
use hofund_tui::session::run_intent;
use hofund_tui::state::PageId;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::io::Cursor;

#[tokio::test]
async fn provider_keys_post_the_control_routes() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let address = serve(Arc::clone(&calls)).await;
    let client = hofund_control_client::Client::tcp(address, "token");
    let mut app = App::for_test(PageId::Providers);

    app.handle_key(key('r'));
    drive(&client, &mut app).await;
    assert!(app.providers.active());

    app.handle_key(key('j'));
    app.handle_key(key('t'));
    drive(&client, &mut app).await;
    let seen = calls.lock().expect("calls").clone();
    assert!(
        seen.iter()
            .any(|call| call.method == "POST" && call.path.ends_with("/second/test")),
        "test call missing: {seen:?}"
    );

    app.handle_key(key('k'));
    app.handle_key(key('d'));
    let held = app.take_intents();
    assert!(held.is_empty(), "one d must not delete");
    app.handle_key(key('d'));
    drive(&client, &mut app).await;
    let seen = calls.lock().expect("calls").clone();
    let delete = seen
        .iter()
        .find(|call| call.method == "DELETE" && call.path.ends_with("/first"))
        .expect("delete");
    assert_eq!(delete.body["expected_revision"], revision());

    app.handle_key(enter());
    drive(&client, &mut app).await;
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let text = render(&app);
    assert!(text.contains("revision changed"), "{text}");
    assert!(text.contains("> [x] a"), "{text}");

    app.handle_key(esc());
    app.handle_key(key('a'));
    app.handle_key(enter());
    for character in "secret-value".chars() {
        app.handle_key(key(character));
    }
    app.handle_key(enter());
    for character in "http://gw.test".chars() {
        app.handle_key(key(character));
    }
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let seen = calls.lock().expect("calls").clone();
    let create = seen
        .iter()
        .rev()
        .find(|call| call.method == "POST" && call.path == "/v1/hofund/providers")
        .expect("create");
    assert_eq!(create.body["api_key"], "secret-value");
    let text = render(&app);
    assert!(!text.contains("secret-value"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: Value,
}

async fn drive(client: &hofund_control_client::Client, app: &mut App) {
    for intent in app.take_intents() {
        let outcome = run_intent(client, intent).await;
        app.apply_session(outcome);
    }
}

fn render(app: &App) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            hofund_tui::ui::draw(
                app,
                frame.area(),
                frame.buffer_mut(),
                &hofund_tui::Theme::monochrome(),
            )
        })
        .expect("draw");
    hofund_tui::buffer_text(terminal.backend().buffer(), 100, 30)
}

fn key(character: char) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(character),
        crossterm::event::KeyModifiers::empty(),
    )
}

fn enter() -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::empty(),
    )
}

fn esc() -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::empty(),
    )
}

fn revision() -> String {
    "ab".repeat(32)
}

async fn serve(calls: Arc<Mutex<Vec<Call>>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                let calls = Arc::clone(&calls);
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
    address
}

async fn handle(
    request: http::Request<Incoming>,
    calls: Arc<Mutex<Vec<Call>>>,
) -> Result<http::Response<Full<Cursor<Vec<u8>>>>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let collected = request.into_body().collect().await.expect("body");
    let bytes = collected.to_bytes();
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
    if method == http::Method::PUT && path.ends_with("/picks") {
        return Ok(json_status(
            http::StatusCode::CONFLICT,
            serde_json::to_value(envelope(ControlErrorKind::Conflict, "revision changed"))
                .expect("error"),
        ));
    }
    if method == http::Method::POST && path == "/v1/hofund/providers" {
        let echoed = body
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("missing");
        return Ok(json_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(envelope(
                ControlErrorKind::Unprocessable,
                &format!("rejected {echoed}"),
            ))
            .expect("error"),
        ));
    }
    let payload = response_for(&method, &path);
    Ok(json_status(http::StatusCode::OK, payload))
}

fn response_for(method: &http::Method, path: &str) -> Value {
    if path == "/v1/hofund/providers" && *method == http::Method::GET {
        serde_json::to_value(providers()).expect("providers")
    } else if path == "/v1/hofund/presets" {
        let mut presets = PresetListResponse::default();
        presets.presets = vec![remote_preset()];
        serde_json::to_value(presets).expect("presets")
    } else if path == "/v1/hofund/status" {
        let mut status = StatusResponse::default();
        status.disk_revision = revision();
        serde_json::to_value(status).expect("status")
    } else if path == "/v1/hofund/quotas" {
        serde_json::to_value(QuotaResponse::default()).expect("quotas")
    } else if path.ends_with("/test") {
        serde_json::to_value(ProviderTestResponse::default()).expect("test")
    } else if *method == http::Method::DELETE {
        let mut deleted = ProviderDeleteResponse::default();
        deleted.disk_revision = revision();
        deleted.removed = "first".to_owned();
        serde_json::to_value(deleted).expect("delete")
    } else if path.ends_with("/models") {
        let mut models = ModelListResponse::default();
        models.models = vec![entry("a"), entry("b")];
        serde_json::to_value(models).expect("models")
    } else if path.ends_with("/picks") {
        let mut picks = PicksResponse::default();
        picks.picks = vec!["a".to_owned(), "b".to_owned()];
        picks.picks_revision = revision();
        serde_json::to_value(picks).expect("picks")
    } else if path == "/v1/hofund/agents" {
        serde_json::to_value(AgentListResponse::default()).expect("agents")
    } else if path == "/v1/hofund/agents/status" {
        serde_json::to_value(AgentStatusResponse::default()).expect("status")
    } else if path == "/v1/hofund/models" {
        serde_json::to_value(RouteModelListResponse::default()).expect("routes")
    } else if path == "/v1/hofund/profiles" {
        serde_json::to_value(ProfileListResponse::default()).expect("profiles")
    } else if path == "/v1/hofund/catalog/sync" {
        serde_json::json!({"version": API_VERSION, "ok": true})
    } else {
        serde_json::json!({"version": API_VERSION})
    }
}

fn providers() -> ProviderListResponse {
    let mut list = ProviderListResponse::default();
    list.providers = vec![summary("first"), summary("second")];
    list
}

fn remote_preset() -> PresetEntry {
    let mut preset = PresetEntry::default();
    preset.id = "remote-hofund".to_owned();
    preset.name = "Remote hofund".to_owned();
    preset.kind = PresetKind::Remote;
    preset.needs_key = true;
    preset.needs_address = true;
    preset
}

fn envelope(kind: ControlErrorKind, message: &str) -> ErrorEnvelope {
    let mut envelope = ErrorEnvelope::default();
    envelope.error = ControlError::new(kind, message, "test");
    envelope
}

fn summary(id: &str) -> ProviderSummary {
    let mut summary = ProviderSummary::default();
    summary.id = id.to_owned();
    summary.enabled = true;
    summary.listed = true;
    summary.base_url = "https://example.test/v1".to_owned();
    summary
}

fn entry(id: &str) -> ModelEntry {
    let mut model = ModelEntry::default();
    model.id = id.to_owned();
    model
}

fn json_status(status: http::StatusCode, body: Value) -> http::Response<Full<Cursor<Vec<u8>>>> {
    let bytes = serde_json::to_vec(&body).expect("json");
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Cursor::new(bytes)))
        .expect("response")
}

#[test]
fn intent_names_stay_in_the_public_module() {
    let _ = ControlIntent::CatalogSync { force: false };
}
