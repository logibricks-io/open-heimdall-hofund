//! Read-only Library page behavior against fixtures and a local fake control server.

use std::convert::Infallible;
use std::io::Cursor;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_proto::{LibraryAgent, LibraryResponse, LibraryRow};
use hofund_tui::app::AppAction;
use hofund_tui::pages::{LibraryPage, Page};
use hofund_tui::runtime::{
    LibraryLoadOutcome, WorkerMsg, apply_worker_msg, schedule_open_page_load,
};
use hofund_tui::{App, PageId, Theme, buffer_text};
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

fn row(kind: &str, name: &str) -> LibraryRow {
    let mut row = LibraryRow::default();
    row.kind = kind.to_owned();
    row.name = name.to_owned();
    row
}

fn view(rows: Vec<LibraryRow>) -> LibraryResponse {
    let mut agent = LibraryAgent::default();
    agent.id = "agent-one".to_owned();
    agent.name = "First Agent".to_owned();
    let mut view = LibraryResponse::default();
    view.dir = "/work/config/library".to_owned();
    view.rows = rows;
    view.agents = vec![agent];
    view
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn render(page: &LibraryPage, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            page.draw_body(
                Rect::new(0, 0, width, height),
                frame.buffer_mut(),
                &Theme::monochrome(),
            );
        })
        .expect("draw");
    buffer_text(terminal.backend().buffer(), width, height)
}

fn position(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("missing {needle}: {text}"))
}

#[test]
fn successful_load_draws_sections_rows_agents_warnings_and_directory() {
    let mut instructions = row("instructions", "shared instructions");
    instructions.on = vec!["agent-one".to_owned()];
    let mut server = row("mcp", "docs");
    server.about = "https://docs.example/mcp".to_owned();
    server.bad = 2;
    let mut found = row("found-mcp", "built-in");
    found.on = vec!["agent-one".to_owned()];
    found.own = true;
    let mut unknown = row("future-kind", "future row");
    unknown.about = "additive row".to_owned();
    let mut page = LibraryPage::new();
    page.apply_load(view(vec![
        unknown,
        row("rtk", "RTK"),
        row("found-skill", "local-skill"),
        found,
        row("skill", "review"),
        server,
        instructions,
    ]));
    let text = render(&page, 100, 30);
    let ordered = [
        "instructions · what every agent reads before a conversation",
        "MCP servers",
        "skills",
        "MCP servers found in agents",
        "skills found in agents",
        "RTK",
        "future-kind",
    ];
    let mut previous = 0;
    for heading in ordered {
        let next = position(&text, heading);
        assert!(next >= previous, "section out of order: {heading}");
        previous = next;
    }
    assert!(text.contains("First Agent"), "{text}");
    assert!(text.contains("! 2"), "{text}");
    assert!(text.contains("First Agent's own"), "{text}");
    assert!(text.contains("https://docs.example/mcp"), "{text}");
    assert!(text.contains("kept in /work/config/library"), "{text}");
}

#[test]
fn empty_success_is_a_real_page_and_not_the_deferred_frame() {
    let mut page = LibraryPage::new();
    page.apply_load(view(Vec::new()));
    let text = render(&page, 100, 20);
    assert!(text.contains("No library entries."), "{text}");
    assert!(text.contains("kept in /work/config/library"), "{text}");
    assert!(!text.contains("not in this build"), "{text}");
}

#[test]
fn response_file_error_and_transport_error_are_visible() {
    let mut page = LibraryPage::new();
    let mut response = view(Vec::new());
    response.error = Some("could not read library.json".to_owned());
    page.apply_load(response);
    assert!(render(&page, 100, 10).contains("could not read library.json"));
    page.apply_load_error("control unavailable".to_owned());
    let text = render(&page, 100, 10);
    assert!(
        text.contains("Could not load library: control unavailable"),
        "{text}"
    );
}

#[test]
fn navigation_wraps_and_mutation_or_command_keys_have_no_effect() {
    let mut app = App::for_test(PageId::Library);
    app.pages
        .library_mut()
        .apply_load(view(vec![row("instructions", "one"), row("mcp", "two")]));
    let generation = app.pages.library().fetch_gen();
    for code in [
        KeyCode::Char('s'),
        KeyCode::Char('a'),
        KeyCode::Char('e'),
        KeyCode::Char('u'),
        KeyCode::Char('i'),
        KeyCode::Char('d'),
        KeyCode::Enter,
        KeyCode::Char(' '),
    ] {
        assert!(matches!(app.handle_key(key(code)), AppAction::None));
    }
    assert_eq!(app.pages.library().cursor(), 0);
    assert_eq!(app.pages.library().fetch_gen(), generation);
    assert!(app.take_intents().is_empty());

    assert!(matches!(
        app.handle_key(key(KeyCode::Char('j'))),
        AppAction::None
    ));
    assert_eq!(app.pages.library().cursor(), 1);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.pages.library().cursor(), 0);
    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.pages.library().cursor(), 1);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.pages.library().cursor(), 0);
}

#[test]
fn global_reload_and_404_behavior_remain_available() {
    let mut app = App::for_test(PageId::Library);
    let before = app.pages.library().fetch_gen();
    assert!(matches!(
        app.handle_key(key(KeyCode::Char('r'))),
        AppAction::Reload
    ));
    let generation = app.pages.library().fetch_gen();
    assert!(generation > before);
    apply_worker_msg(
        &mut app,
        WorkerMsg::LibraryLoad {
            generation,
            outcome: LibraryLoadOutcome::NotInBuild,
        },
    );
    assert!(app.pages.library().not_in_build());
    assert!(render(app.pages.library(), 100, 10).contains("Library is not in this build yet."));
}

#[test]
fn narrow_window_keeps_the_cursor_visible_with_four_body_rows() {
    let rows = (0..20)
        .map(|index| row("mcp", &format!("server-{index}")))
        .collect();
    let mut page = LibraryPage::new();
    page.apply_load(view(rows));
    for _ in 0..12 {
        page.on_key(key(KeyCode::Down));
    }
    let text = render(&page, 80, 10);
    assert!(
        text.lines()
            .any(|line| line.contains("> ") && line.contains("server-12")),
        "{text}"
    );
    assert!(text.matches("server-").count() >= 4, "{text}");
    assert!(!text.contains("server-0 "), "{text}");
}

#[tokio::test]
async fn open_page_makes_one_typed_get_and_applies_its_rows() {
    let response = view(vec![row("skill", "from-control")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("connection");
        let service = service_fn(move |request: http::Request<hyper::body::Incoming>| {
            let response = response.clone();
            async move {
                assert_eq!(request.method(), http::Method::GET);
                assert_eq!(request.uri().path(), "/v1/hofund/library");
                let body = serde_json::to_vec(&response).expect("library JSON");
                Ok::<_, Infallible>(
                    http::Response::builder()
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Full::new(Cursor::new(body)))
                        .expect("response"),
                )
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await
            .expect("serve connection");
    });
    let client = hofund_control_client::Client::tcp(address, "test-token");
    let mut app = App::for_test(PageId::Library);
    let generation = app.reload_open_page();
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel(1);
    schedule_open_page_load(&app, Some(&client), &work_tx);
    let message = work_rx.recv().await.expect("library worker result");
    apply_worker_msg(&mut app, message);
    assert_eq!(app.pages.library().fetch_gen(), generation);
    assert_eq!(app.pages.library().view().rows[0].name, "from-control");
    drop(client);
    server.await.expect("server");
}
