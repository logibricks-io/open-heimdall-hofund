//! Routing page keys and control calls against a local fake control server.

use std::convert::Infallible;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::Client;
use hofund_control_proto::{
    API_VERSION, Group, GroupDeleteResponse, GroupListResponse, GroupMember, GroupMemberCandidate,
    GroupMemberCandidatesResponse, GroupRule, GroupWriteResponse, RouteListResponse, RouteQuery,
    RoutingDecision, SessionRouteResponse, UnroutedListResponse, UnroutedRequest,
};
use hofund_tui::app::{App, AppAction};
use hofund_tui::intent::ControlIntent;
use hofund_tui::pages::RoutingPage;
use hofund_tui::session::run_intent;
use hofund_tui::{PageId, Theme, buffer_text};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;

#[derive(Clone, Debug)]
struct Call {
    method: String,
    target: String,
    body: Value,
}

#[tokio::test]
async fn open_move_and_back_use_group_member_order() {
    let (mut app, client, calls) = loaded_app().await;
    app.handle_key(enter());
    assert!(app.pages.routing().group_open());
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT));
    drive(&client, &mut app).await;

    let patch = last_call(&calls, "PATCH", "/v1/hofund/groups/team");
    assert_eq!(patch.body["expected_revision"], revision(0x11));
    assert_eq!(patch.body["members"][0]["ref"], "chat/b");
    assert_eq!(patch.body["members"][1]["ref"], "chat/a");
    assert!(patch.body.get("rules").is_none_or(Value::is_null));

    assert_eq!(app.handle_key(key('q')), AppAction::None);
    assert!(!app.pages.routing().group_open());
    assert_eq!(app.handle_key(key('q')), AppAction::Quit);
}

#[tokio::test]
async fn hidden_rows_restore_instead_of_open_strategy_or_delete() {
    let (mut app, client, calls) = loaded_app().await;
    app.handle_key(key('j'));
    app.handle_key(key('j'));
    app.handle_key(enter());
    assert!(!app.pages.routing().group_open());
    assert!(flash(&app).contains("u restores"));
    app.handle_key(key('o'));
    app.handle_key(key('d'));
    app.handle_key(key('d'));
    assert!(app.take_intents().is_empty());
    assert!(!has_call(&calls, "DELETE", "/v1/hofund/groups/ghost"));

    app.handle_key(key('u'));
    drive(&client, &mut app).await;
    let restore = last_call(&calls, "POST", "/v1/hofund/groups/ghost/restore");
    assert_eq!(restore.body["expected_revision"], revision(0x11));

    app.handle_key(key('n'));
    type_text(&mut app, "fresh");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let create = last_call(&calls, "POST", "/v1/hofund/groups");
    assert_eq!(create.body["name"], "fresh");
    assert_eq!(create.body["first_member"], "chat/a");
    assert!(!calls.lock().expect("calls").iter().any(|call| {
        call.target == "/v1/hofund/models" || call.target.starts_with("/v1/hofund/models?")
    }));
}

#[tokio::test]
async fn group_editor_sends_typed_fields_rules_and_member_flags() {
    let (mut app, client, calls) = loaded_app().await;
    app.handle_key(enter());

    app.handle_key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT));
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["members"][0]["effort"],
        "low"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["members"][0]["fast"],
        true
    );
    app.handle_key(key('o'));
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["routing"],
        "order"
    );
    app.handle_key(key('s'));
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["stays"],
        "session"
    );

    app.handle_key(key('a'));
    drive(&client, &mut app).await;
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let added = last_call(&calls, "PATCH", "/v1/hofund/groups/team");
    assert_eq!(added.body["members"].as_array().expect("members").len(), 3);
    assert_eq!(added.body["members"][2]["ref"], "group/other");

    app.handle_key(key('n'));
    type_text(&mut app, "use=chat/b tokens=2k");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    let rule = last_call(&calls, "PATCH", "/v1/hofund/groups/team");
    assert_eq!(rule.body["rules"][1], "use=chat/b tokens=2k");

    app.handle_key(key('c'));
    drive(&client, &mut app).await;
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["classifier"],
        "chat/a"
    );

    app.handle_key(key('f'));
    type_text(&mut app, "relay");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["family"],
        "relay"
    );

    app.handle_key(key('x'));
    type_text(&mut app, "200k");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["context"],
        200_000
    );

    app.handle_key(key('l'));
    type_text(&mut app, "low,high");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["levels"],
        serde_json::json!(["low", "high"])
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
    for _ in 0..4 {
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
    }
    type_text(&mut app, "new");
    app.handle_key(enter());
    drive(&client, &mut app).await;
    assert_eq!(
        last_call(&calls, "PATCH", "/v1/hofund/groups/team").body["rename_to"],
        "new"
    );
    assert!(app.pages.routing().group_open());
    assert!(
        app.pages
            .routing()
            .groups()
            .iter()
            .any(|group| group.id == "new")
    );
}

#[tokio::test]
async fn rule_delete_and_member_delete_keep_group_invariants() {
    let (mut app, client, calls) = loaded_app().await;
    app.handle_key(enter());
    app.handle_key(key('j'));
    app.handle_key(key('j'));
    app.handle_key(key('d'));
    assert!(app.take_intents().is_empty());
    app.handle_key(key('d'));
    drive(&client, &mut app).await;
    let removed = last_call(&calls, "PATCH", "/v1/hofund/groups/team");
    assert_eq!(removed.body["rules"], serde_json::json!([]));

    app.handle_key(key('k'));
    app.handle_key(key('d'));
    app.handle_key(key('d'));
    drive(&client, &mut app).await;
    let removed = last_call(&calls, "PATCH", "/v1/hofund/groups/team");
    assert_eq!(
        removed.body["members"].as_array().expect("members").len(),
        1
    );
    assert!(removed.body.get("rules").is_none_or(Value::is_null));

    let mut page = RoutingPage::new();
    let mut list = GroupListResponse::default();
    list.disk_revision = revision(0x11);
    let mut single = group("single", false);
    single.members.truncate(1);
    list.groups = vec![single];
    page.apply_load(list);
    page.on_key(enter());
    page.on_key(key('d'));
    let effect = page.on_key(key('d'));
    assert!(effect.intents.is_empty());
    assert!(
        effect
            .flash
            .expect("last member hint")
            .0
            .contains("needs a member")
    );
}

#[tokio::test]
async fn route_view_reads_decisions_session_and_unrouted() {
    let (mut app, client, calls) = loaded_app().await;
    app.handle_key(key('v'));
    drive(&client, &mut app).await;
    let seen = calls.lock().expect("calls").clone();
    for target in [
        "/v1/hofund/route",
        "/v1/hofund/route/session?session=s1",
        "/v1/hofund/route/unrouted",
    ] {
        assert!(
            seen.iter()
                .any(|call| call.method == "GET" && call.target == target),
            "missing {target}: {seen:?}"
        );
    }
    let text = render(&app);
    assert!(text.contains("group/team -> chat/a"), "{text}");
    assert!(text.contains("1 unrouted hold"), "{text}");
    assert!(text.contains("session s1"), "{text}");
    assert_eq!(app.handle_key(esc()), AppAction::None);
    assert!(!app.pages.routing().captures());
}

#[test]
fn list_reload_uses_groups_and_group_r_does_not_reload() {
    let mut app = App::for_test(PageId::Routing);
    let before = app.pages.routing().fetch_gen();
    assert_eq!(app.handle_key(key('r')), AppAction::Reload);
    assert!(app.pages.routing().fetch_gen() > before);
    assert!(app.take_intents().iter().any(|intent| {
        matches!(intent, ControlIntent::LoadGroups { generation } if *generation > before)
    }));

    let mut list = GroupListResponse::default();
    list.disk_revision = revision(0x11);
    list.groups = vec![group("team", false)];
    app.pages.routing_mut().apply_load(list);
    app.handle_key(enter());
    let generation = app.pages.routing().fetch_gen();
    assert_eq!(app.handle_key(key('r')), AppAction::None);
    assert_eq!(app.pages.routing().fetch_gen(), generation);
    assert!(app.take_intents().is_empty());
}

async fn loaded_app() -> (App, Client, Arc<Mutex<Vec<Call>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let address = serve(Arc::clone(&calls)).await;
    let client = Client::tcp(address, "token");
    let mut app = App::for_test(PageId::Routing);
    app.handle_key(key('r'));
    drive(&client, &mut app).await;
    (app, client, calls)
}

async fn drive(client: &Client, app: &mut App) {
    for intent in app.take_intents() {
        let outcome = run_intent(client, intent).await;
        app.apply_session(outcome);
    }
}

fn flash(app: &App) -> &str {
    app.flash
        .as_ref()
        .map(|flash| flash.text.as_str())
        .unwrap_or("")
}

fn last_call(calls: &Arc<Mutex<Vec<Call>>>, method: &str, path: &str) -> Call {
    calls
        .lock()
        .expect("calls")
        .iter()
        .rev()
        .find(|call| call.method == method && call.target == path)
        .cloned()
        .unwrap_or_else(|| panic!("missing {method} {path}"))
}

fn has_call(calls: &Arc<Mutex<Vec<Call>>>, method: &str, path: &str) -> bool {
    calls
        .lock()
        .expect("calls")
        .iter()
        .any(|call| call.method == method && call.target == path)
}

fn render(app: &App) -> String {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            hofund_tui::ui::draw(app, frame.area(), frame.buffer_mut(), &Theme::monochrome())
        })
        .expect("draw");
    buffer_text(terminal.backend().buffer(), 120, 30)
}

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty())
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(character));
    }
}

fn revision(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn member(reference: &str) -> GroupMember {
    let mut member = GroupMember::default();
    member.reference = reference.to_owned();
    member.enabled = true;
    member
}

fn rule(line: &str) -> GroupRule {
    let mut rule = GroupRule::default();
    rule.line = line.to_owned();
    rule
}

fn group(id: &str, hidden: bool) -> Group {
    let mut group = Group::default();
    group.id = id.to_owned();
    group.name = id.to_owned();
    group.routing = "smart".to_owned();
    group.stays = "auto".to_owned();
    group.members = vec![member("chat/a"), member("chat/b")];
    group.rules = vec![rule("use=chat/a intent=\"quick\"")];
    group.hidden = hidden;
    group.found = hidden;
    group
}

fn groups() -> GroupListResponse {
    let mut list = GroupListResponse::default();
    list.disk_revision = revision(0x11);
    list.groups = vec![
        group("team", false),
        group("other", false),
        group("ghost", true),
    ];
    list
}

fn candidates() -> GroupMemberCandidatesResponse {
    let mut response = GroupMemberCandidatesResponse::default();
    response.served = vec![
        candidate("chat/a", false),
        candidate("chat/b", false),
        candidate("group/other", false),
        candidate("group/team", false),
    ];
    response.deciders = vec![candidate("decide/a", true)];
    response
}

fn candidate(reference: &str, decision: bool) -> GroupMemberCandidate {
    let mut candidate = GroupMemberCandidate::default();
    candidate.reference = reference.to_owned();
    candidate.decision = decision;
    candidate
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
    let target = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
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
        target: target.clone(),
        body: body.clone(),
    });
    let payload = response_for(&method, &path, &target, &body);
    Ok(json(payload))
}

fn response_for(method: &http::Method, path: &str, target: &str, body: &Value) -> Value {
    if path == "/v1/hofund/groups" && *method == http::Method::GET {
        return serde_json::to_value(groups()).expect("groups");
    }
    if path == "/v1/hofund/groups/member-candidates" {
        return serde_json::to_value(candidates()).expect("candidates");
    }
    if path == "/v1/hofund/groups" && *method == http::Method::POST {
        let name = body["name"].as_str().unwrap_or("new");
        let mut group = group(name, false);
        group.members = vec![member(body["first_member"].as_str().unwrap_or("chat/a"))];
        group.rules.clear();
        return write_response(group);
    }
    if let Some(id) = path
        .strip_prefix("/v1/hofund/groups/")
        .and_then(|tail| tail.strip_suffix("/restore"))
    {
        return write_response(group(id, false));
    }
    if let Some(id) = path.strip_prefix("/v1/hofund/groups/") {
        if *method == http::Method::DELETE {
            let mut response = GroupDeleteResponse::default();
            response.disk_revision = revision(0x22);
            response.removed = id.to_owned();
            return serde_json::to_value(response).expect("delete");
        }
        let mut group = group(id, false);
        if let Some(routing) = body["routing"].as_str() {
            group.routing = routing.to_owned();
        }
        if let Some(stays) = body["stays"].as_str() {
            group.stays = stays.to_owned();
        }
        if let Some(family) = body["family"].as_str() {
            group.family = Some(family.to_owned());
        }
        if body.get("family").is_some_and(|value| value.is_null()) {
            group.family = None;
        }
        if let Some(context) = body["context"].as_i64() {
            group.context = Some(context);
        }
        if body.get("context").is_some_and(|value| value.is_null()) {
            group.context = None;
        }
        if let Some(levels) = body["levels"].as_array() {
            group.levels = levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        if let Some(classifier) = body["classifier"].as_str() {
            group.classifier = classifier.to_owned();
        }
        if let Some(members) = body["members"].as_array() {
            let removed_member = members.len() < group.members.len();
            group.members = members
                .iter()
                .map(|value| {
                    let mut member = member(value["ref"].as_str().unwrap_or("chat/a"));
                    member.effort = value["effort"].as_str().unwrap_or("").to_owned();
                    member.fast = value["fast"].as_bool().unwrap_or(false);
                    member.enabled = value["enabled"].as_bool().unwrap_or(true);
                    member
                })
                .collect();
            if removed_member && body.get("rules").is_none_or(Value::is_null) {
                group.rules.clear();
            }
        }
        if let Some(rules) = body["rules"].as_array() {
            group.rules = rules.iter().filter_map(Value::as_str).map(rule).collect();
        }
        if let Some(rename) = body["rename_to"].as_str() {
            group.id = rename.to_owned();
            group.name = rename.to_owned();
        }
        return write_response(group);
    }
    if path == "/v1/hofund/route" {
        let mut response = RouteListResponse::default();
        response.decisions = vec![decision()];
        return serde_json::to_value(response).expect("route");
    }
    if path == "/v1/hofund/route/session" {
        assert_eq!(
            RouteQuery::parse(target.split_once('?').map(|pair| pair.1))
                .session
                .as_deref(),
            Some("s1")
        );
        let mut response = SessionRouteResponse::default();
        response.version = API_VERSION;
        response.session = "s1".to_owned();
        response.decision = Some(decision());
        return serde_json::to_value(response).expect("session route");
    }
    if path == "/v1/hofund/route/unrouted" {
        let mut response = UnroutedListResponse::default();
        let mut hold = UnroutedRequest::default();
        hold.id = 1;
        hold.since_ms = 1;
        hold.session = "s1".to_owned();
        hold.asked = "group/team".to_owned();
        hold.inbound_protocol = "chat".to_owned();
        hold.phase = "waiting".to_owned();
        response.holds = vec![hold];
        return serde_json::to_value(response).expect("unrouted");
    }
    serde_json::json!({"version": API_VERSION})
}

fn decision() -> RoutingDecision {
    let mut decision = RoutingDecision::default();
    decision.id = 1;
    decision.session = "s1".to_owned();
    decision.asked = "group/team".to_owned();
    decision.routing = "smart".to_owned();
    decision.served_member = "chat/a".to_owned();
    decision.done = true;
    decision.status = Some(200);
    decision
}

fn write_response(group: Group) -> Value {
    let mut response = GroupWriteResponse::default();
    response.group = group;
    response.disk_revision = revision(0x22);
    serde_json::to_value(response).expect("group write")
}

fn json(body: Value) -> http::Response<Full<Cursor<Vec<u8>>>> {
    let bytes = serde_json::to_vec(&body).expect("json");
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Cursor::new(bytes)))
        .expect("response")
}
