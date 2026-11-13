//! Accounts page keys and control calls against a deterministic fake server.

use std::convert::Infallible;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use hofund_control_proto::{
    API_VERSION, AccountDiscoverImportResponse, AccountDiscoverResponse, AccountImportResponse,
    AccountRemoveResponse, AccountSwitchResponse, DiscoveredAccount, SignInChallenge,
    SignInSessionState, SignInStartResponse, SignInStatusResponse, SubscriptionAccount,
    SubscriptionEntry, SubscriptionFlow, SubscriptionListResponse, SubscriptionRefreshResponse,
};
use hofund_tui::app::AppAction;
use hofund_tui::intent::ControlIntent;
use hofund_tui::session::{AccountsOutcome, SessionOutcome, run_intent};
use hofund_tui::{App, PageId};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;

#[test]
fn key_seven_loads_accounts_and_stays_outside_the_six_page_cycle() {
    let mut app = App::for_test(PageId::Library);
    app.handle_key(key('7'));
    assert_eq!(app.page, PageId::Accounts);
    assert!(matches!(
        app.take_intents().as_slice(),
        [ControlIntent::LoadAccounts { .. }]
    ));
    app.handle_key(key(']'));
    assert_eq!(app.page, PageId::Providers);
    for _ in 0..PageId::CYCLE.len() {
        assert_ne!(app.page, PageId::Accounts);
        app.handle_key(key(']'));
    }
    assert_eq!(app.page, PageId::Providers);
}

#[tokio::test]
async fn account_mutations_use_the_control_contract_and_confirm_destructive_keys() {
    let server = FakeServer::start(SubscriptionFlow::Pkce).await;
    let client = hofund_control_client::Client::tcp(server.address, "token");
    let mut app = loaded_app(&client).await;
    app.handle_key(key('j'));
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert!(server.has_call("POST", "/v1/hofund/accounts/switch"));
    let call = server.last_call("POST", "/v1/hofund/accounts/switch");
    assert_eq!(call.body["agent"], "example");
    assert_eq!(call.body["target"], "a1");
    assert_eq!(call.body["version"], API_VERSION);

    app.handle_key(key('n'));
    drive(&client, &mut app).await;
    let call = server.last_call("POST", "/v1/hofund/accounts/switch");
    assert_eq!(call.body["target"], "next");

    app.handle_key(key('x'));
    drive(&client, &mut app).await;
    let call = server.last_call("POST", "/v1/hofund/subscriptions/example/refresh");
    assert_eq!(call.body["account"], "a1");

    app.handle_key(key('d'));
    assert!(app.take_intents().is_empty());
    app.handle_key(key('j'));
    app.handle_key(key('d'));
    assert!(app.take_intents().is_empty(), "another key disarms dd");
    app.handle_key(key('k'));
    app.handle_key(key('d'));
    app.handle_key(key('d'));
    drive(&client, &mut app).await;
    assert!(server.has_call(
        "POST",
        "/v1/hofund/subscriptions/example/accounts/a1/remove"
    ));

    app.handle_key(key('i'));
    for character in "/exports/account.json".chars() {
        app.handle_key(key(character));
    }
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let call = server.last_call("POST", "/v1/hofund/accounts/import");
    assert_eq!(call.body["path"], "/exports/account.json");

    app.handle_key(key('f'));
    drive(&client, &mut app).await;
    assert_eq!(server.count("GET", "/v1/hofund/accounts/discover"), 1);
    assert!(!server.has_call("POST", "/v1/hofund/accounts/discover"));
    app.handle_key(key('f'));
    drive(&client, &mut app).await;
    let call = server.last_call("POST", "/v1/hofund/accounts/discover");
    assert_eq!(call.body["add"], true);
}

#[tokio::test]
async fn pkce_proof_is_hidden_and_device_flow_only_polls_and_cancels() {
    let server = FakeServer::start(SubscriptionFlow::Pkce).await;
    let client = hofund_control_client::Client::tcp(server.address, "token");
    let mut app = loaded_app(&client).await;
    app.handle_key(key('a'));
    drive(&client, &mut app).await;
    let text = render(&app);
    assert!(text.contains("proof requested"), "{text}");
    assert!(!text.contains("auth-secret.test"), "{text}");
    assert!(!text.contains("callback-secret"), "{text}");
    app.mark_sign_in_dirty("example", Some("another-session"));
    assert!(app.take_intents().is_empty());
    app.mark_sign_in_dirty("example", Some("session-1"));
    assert_eq!(app.take_intents().len(), 1, "progress hints poll");
    drive(&client, &mut app).await;

    let action = app.handle_key(key('o'));
    let AppAction::OpenUrl(url) = action else {
        panic!("o did not request the OS opener");
    };
    assert_eq!(url.expose(), "https://auth-secret.test/authorize");
    assert!(!format!("{url:?}").contains("auth-secret.test"));

    for character in "http://localhost/callback?code=callback-secret".chars() {
        app.handle_key(key(character));
    }
    let text = render(&app);
    assert!(!text.contains("callback-secret"), "{text}");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let call = server.last_call(
        "POST",
        "/v1/hofund/subscriptions/example/sign-in/session-1/proof",
    );
    assert_eq!(
        call.body["url"],
        "http://localhost/callback?code=callback-secret"
    );
    assert!(call.body.get("token").is_none());
    assert!(!app.sign_in_dirty());
    assert!(!render(&app).contains("callback-secret"));

    let server = FakeServer::start(SubscriptionFlow::Device).await;
    let client = hofund_control_client::Client::tcp(server.address, "token");
    let mut app = loaded_app(&client).await;
    app.handle_key(key('a'));
    drive(&client, &mut app).await;
    let text = render(&app);
    assert!(text.contains("Polling completes"), "{text}");
    assert!(!text.contains("DEVICE-SECRET"), "{text}");
    assert!(!text.contains("device-secret.test"), "{text}");
    app.handle_key(enter());
    assert!(app.take_intents().is_empty());
    app.poll_sign_in();
    app.poll_sign_in();
    assert_eq!(app.take_intents().len(), 1, "one poll stays in flight");
    app.handle_key(esc());
    drive(&client, &mut app).await;
    assert!(server.has_call(
        "POST",
        "/v1/hofund/subscriptions/example/sign-in/session-1/cancel"
    ));
    assert!(!server.has_call(
        "POST",
        "/v1/hofund/subscriptions/example/sign-in/session-1/proof"
    ));
    assert!(!app.sign_in_dirty());
}

#[tokio::test]
async fn token_errors_and_debug_output_never_echo_proof_material() {
    let server = FakeServer::start(SubscriptionFlow::TokenFile).await;
    let client = hofund_control_client::Client::tcp(server.address, "token");
    let mut app = loaded_app(&client).await;
    app.handle_key(key('a'));
    drive(&client, &mut app).await;
    for character in "/tmp/export.json".chars() {
        app.handle_key(key(character));
    }
    app.handle_key(enter());
    assert!(app.take_intents().is_empty());
    assert_eq!(
        server.count(
            "POST",
            "/v1/hofund/subscriptions/example/sign-in/session-1/proof"
        ),
        0,
        "a proof path must go through import instead"
    );
    assert!(!render(&app).contains("/tmp/export.json"));

    for character in "plain-text-token-secret".chars() {
        app.handle_key(key(character));
    }
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let call = server.last_call(
        "POST",
        "/v1/hofund/subscriptions/example/sign-in/session-1/proof",
    );
    assert_eq!(call.body["token"], "plain-text-token-secret");
    assert!(call.body.get("url").is_none());
    let text = render(&app);
    assert!(!text.contains("plain-text-token-secret"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");

    let mut status = SignInStatusResponse::default();
    status.challenge = Some(SignInChallenge::DeviceCode {
        user_code: "DEVICE-SECRET".to_owned(),
        verification_url: "https://device-secret.test".to_owned(),
    });
    let outcome = SessionOutcome::Accounts(AccountsOutcome::Poll {
        token: 1,
        subscription: "example".to_owned(),
        session: "session-1".to_owned(),
        result: Ok(status),
    });
    let debug = format!("{outcome:?}");
    assert!(!debug.contains("DEVICE-SECRET"), "{debug}");
    assert!(!debug.contains("device-secret.test"), "{debug}");
}

#[test]
fn newer_reload_drops_older_account_read_and_mutations_block_keys() {
    let mut app = App::for_test(PageId::Accounts);
    let old = app.reload_open_page();
    let current = app.reload_open_page();
    app.apply_session(SessionOutcome::Accounts(AccountsOutcome::List {
        generation: old,
        result: Ok(subscriptions("old")),
    }));
    app.apply_session(SessionOutcome::Accounts(AccountsOutcome::List {
        generation: current,
        result: Ok(subscriptions("current")),
    }));
    let text = render(&app);
    assert!(text.contains("current"), "{text}");
    assert!(!text.contains("old"), "{text}");
    let _ = app.take_intents();

    app.handle_key(key('a'));
    assert!(app.pending.is_some());
    let before = app.take_intents();
    app.handle_key(key('d'));
    app.handle_key(key('d'));
    assert!(app.take_intents().is_empty());
    assert_eq!(before.len(), 1);
}

#[derive(Clone)]
struct Call {
    method: String,
    path: String,
    body: Value,
}

struct FakeServer {
    address: SocketAddr,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeServer {
    async fn start(flow: SubscriptionFlow) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server_calls = Arc::clone(&calls);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let calls = Arc::clone(&server_calls);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let calls = Arc::clone(&calls);
                        async move { handle(request, calls, flow).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { address, calls }
    }

    fn has_call(&self, method: &str, path: &str) -> bool {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.method == method && call.path == path)
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.method == method && call.path == path)
            .count()
    }

    fn last_call(&self, method: &str, path: &str) -> Call {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .rev()
            .find(|call| call.method == method && call.path == path)
            .expect("expected control call")
            .clone()
    }
}

async fn handle(
    request: http::Request<Incoming>,
    calls: Arc<Mutex<Vec<Call>>>,
    flow: SubscriptionFlow,
) -> Result<http::Response<Full<Cursor<Vec<u8>>>>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let bytes = request
        .into_body()
        .collect()
        .await
        .expect("body")
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
    if method == http::Method::POST
        && path.ends_with("/proof")
        && flow == SubscriptionFlow::TokenFile
    {
        return Ok(json_response(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "version": API_VERSION,
                "error": {
                    "kind": "unprocessable",
                    "message": "rejected plain-text-token-secret",
                    "request_id": "test",
                    "retry_after_ms": null,
                    "details": []
                }
            }),
        ));
    }
    let payload = response_for(&method, &path, flow);
    Ok(json_response(http::StatusCode::OK, payload))
}

fn response_for(method: &http::Method, path: &str, flow: SubscriptionFlow) -> Value {
    if path == "/v1/hofund/subscriptions" && *method == http::Method::GET {
        let mut response = SubscriptionListResponse::default();
        response.subscriptions = subscriptions("example");
        serde_json::to_value(response).expect("subscriptions")
    } else if method == http::Method::POST && path.ends_with("/sign-in") {
        let challenge = match flow {
            SubscriptionFlow::Pkce => SignInChallenge::OpenUrl {
                url: "https://auth-secret.test/authorize".to_owned(),
            },
            SubscriptionFlow::Device => SignInChallenge::DeviceCode {
                user_code: "DEVICE-SECRET".to_owned(),
                verification_url: "https://device-secret.test/verify".to_owned(),
            },
            _ => SignInChallenge::Instructions {
                text: "Paste token text when requested.".to_owned(),
            },
        };
        let mut response = SignInStartResponse::default();
        response.session = "session-1".to_owned();
        response.flow = flow;
        response.challenge = challenge;
        serde_json::to_value(response).expect("sign-in start")
    } else if *method == http::Method::GET && path.contains("/sign-in/session-1") {
        let mut response = SignInStatusResponse::default();
        response.state = if flow == SubscriptionFlow::Device {
            SignInSessionState::Pending
        } else {
            SignInSessionState::NeedsProof
        };
        serde_json::to_value(response).expect("sign-in status")
    } else if *method == http::Method::POST && path.ends_with("/proof") {
        let mut response = SignInStatusResponse::default();
        response.state = SignInSessionState::Ready;
        serde_json::to_value(response).expect("proof")
    } else if *method == http::Method::POST && path.ends_with("/cancel") {
        serde_json::json!({"version": API_VERSION, "state": "cancelled"})
    } else if path == "/v1/hofund/accounts/switch" {
        let mut response = AccountSwitchResponse::default();
        response.agent = "example".to_owned();
        response.account = "a1".to_owned();
        serde_json::to_value(response).expect("switch")
    } else if path == "/v1/hofund/accounts/import" {
        serde_json::to_value(AccountImportResponse::default()).expect("import")
    } else if path == "/v1/hofund/accounts/discover" && *method == http::Method::GET {
        let mut found = DiscoveredAccount::default();
        found.subscription = "example".to_owned();
        found.label = "discovered".to_owned();
        let mut response = AccountDiscoverResponse::default();
        response.found = vec![found];
        serde_json::to_value(response).expect("discovery")
    } else if path == "/v1/hofund/accounts/discover" {
        serde_json::to_value(AccountDiscoverImportResponse::default()).expect("discovery import")
    } else if path.ends_with("/remove") {
        let mut response = AccountRemoveResponse::default();
        response.removed = true;
        serde_json::to_value(response).expect("remove")
    } else if path.ends_with("/refresh") {
        let mut response = SubscriptionRefreshResponse::default();
        response.refreshed = vec!["a1".to_owned()];
        serde_json::to_value(response).expect("refresh")
    } else {
        serde_json::json!({"version": API_VERSION})
    }
}

fn subscriptions(id: &str) -> Vec<SubscriptionEntry> {
    let mut first = SubscriptionAccount::default();
    first.id = "a1".to_owned();
    first.label = "first".to_owned();
    first.ready = true;
    let mut second = SubscriptionAccount::default();
    second.id = "a2".to_owned();
    second.label = "second".to_owned();
    second.needs_sign_in = Some("expired".to_owned());
    let mut subscription = SubscriptionEntry::default();
    subscription.id = id.to_owned();
    subscription.name = id.to_owned();
    subscription.flow = SubscriptionFlow::Pkce;
    subscription.accounts = vec![first, second];
    vec![subscription]
}

async fn loaded_app(client: &hofund_control_client::Client) -> App {
    let mut app = App::for_test(PageId::Accounts);
    app.handle_key(key('r'));
    drive(client, &mut app).await;
    app
}

async fn drive(client: &hofund_control_client::Client, app: &mut App) {
    for _ in 0..8 {
        let intents = app.take_intents();
        if intents.is_empty() {
            return;
        }
        for intent in intents {
            let outcome = run_intent(client, intent).await;
            app.apply_session(outcome);
        }
    }
    panic!("control intents did not quiesce");
}

fn render(app: &App) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(120, 30);
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
    hofund_tui::buffer_text(terminal.backend().buffer(), 120, 30)
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

fn json_response(status: http::StatusCode, body: Value) -> http::Response<Full<Cursor<Vec<u8>>>> {
    let bytes = serde_json::to_vec(&body).expect("JSON");
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Cursor::new(bytes)))
        .expect("response")
}
