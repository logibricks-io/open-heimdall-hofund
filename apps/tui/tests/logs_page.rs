//! Logs page and help overlay (R-18).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    EventPayload, LogTailResponse, RequestStartedEvent,
};
use hofund_tui::app::App;
use hofund_tui::events::apply_control_event;
use hofund_tui::intent::ControlIntent;
use hofund_tui::pages::Page;
use hofund_tui::session::SessionOutcome;
use hofund_tui::state::{Mode, PageId};
use hofund_tui::theme::Theme;
use hofund_tui::ui::{buffer_text, draw};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn render(app: &App) -> String {
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| draw(app, frame.area(), frame.buffer_mut(), &Theme::monochrome()))
        .expect("draw");
    buffer_text(terminal.backend().buffer(), 100, 24)
}

#[test]
fn key_nine_opens_logs_and_queues_tail() {
    let mut app = App::for_test(PageId::Agents);
    app.handle_key(key(KeyCode::Char('9')));
    assert_eq!(app.page, PageId::Logs);
    assert!(
        app.take_intents()
            .iter()
            .any(|intent| matches!(intent, ControlIntent::TailLogs { .. }))
    );
}

#[test]
fn follow_and_request_toggle_in_footer() {
    let mut app = App::for_test(PageId::Logs);
    app.handle_key(key(KeyCode::Char('f')));
    app.handle_key(key(KeyCode::Char('e')));
    let text = render(&app);
    assert!(text.contains("follow on"));
    assert!(text.contains("requests on"));
}

#[test]
fn help_overlay_lists_logs_keys() {
    let mut app = App::for_test(PageId::Logs);
    app.handle_key(key(KeyCode::Char('?')));
    assert_eq!(app.mode, Mode::Help);
    let text = render(&app);
    assert!(text.contains("Help — logs"));
    assert!(text.contains("follow"));
    assert!(text.contains("toggle request"));
}

#[test]
fn tail_response_appends_lines() {
    let mut app = App::for_test(PageId::Logs);
    let _ = app.reload_open_page();
    let intent = app.take_intents().pop().expect("tail intent");
    let (generation, token) = match intent {
        ControlIntent::TailLogs {
            generation, token, ..
        } => (generation, token),
        other => panic!("unexpected intent: {other:?}"),
    };
    let mut response = LogTailResponse::default();
    response.lines = vec!["gateway ready".to_owned()];
    response.next_offset = 14;
    app.apply_session(SessionOutcome::Logs {
        generation,
        token,
        after: 0,
        result: Ok(response),
    });
    assert_eq!(app.pages.logs().fetch_gen(), generation);
    let text = render(&app);
    assert!(text.contains("gateway ready"));
}

#[test]
fn request_events_append_when_on_logs_page() {
    let mut app = App::for_test(PageId::Logs);
    app.pages.logs_mut().on_key(key(KeyCode::Char('e')));
    let mut event = RequestStartedEvent::default();
    event.request_id = "req-1".to_owned();
    event.route = "chat".to_owned();
    event.inbound_protocol = "openai".to_owned();
    apply_control_event(&mut app, &EventPayload::RequestStarted(event));
    let text = render(&app);
    assert!(text.contains("request started req-1"));
}
