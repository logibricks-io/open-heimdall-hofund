//! Sessions page keys against the app model.

use hofund_control_proto::{
    SessionConsent, SessionDayStats, SessionListResponse, SessionResume, SessionStatsResponse,
    SessionSummary, SessionTokenTotals,
};
use hofund_tui::app::AppAction;
use hofund_tui::pages::{Page, PageKeyOutcome, SessionsPage};
use hofund_tui::{App, PageId};

fn sample_session(id: &str) -> SessionSummary {
    let mut tokens = SessionTokenTotals::default();
    tokens.input = 42;
    let mut row = SessionSummary::default();
    row.id = id.to_owned();
    row.agent = "agent".to_owned();
    row.title = "fixture".to_owned();
    row.cwd = "/workspace/hofund".to_owned();
    row.last_ms = 1_700_000_000_000;
    row.tokens = tokens;
    row
}

fn day(date: &str, input: u64) -> SessionDayStats {
    let mut tokens = SessionTokenTotals::default();
    tokens.input = input;
    let mut row = SessionDayStats::default();
    row.date = date.to_owned();
    row.tokens = tokens;
    row
}

#[test]
fn stats_toggle_and_day_chart_fixture() {
    let mut page = SessionsPage::new();
    let mut consent = SessionConsent::default();
    consent.asked = true;
    let mut list = SessionListResponse::default();
    list.consent = consent;
    list.sessions = vec![sample_session("b"), sample_session("a")];
    let mut stats = SessionStatsResponse::default();
    stats.tokens.input = 30;
    stats.days = vec![day("2026-01-01", 10), day("2026-01-02", 20)];
    page.set_fixture_list(list, stats);
    assert_eq!(page.sessions()[0].id, "b");
    let outcome = page.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyModifiers::empty(),
    ));
    assert_eq!(outcome, PageKeyOutcome::None);
    let chart = hofund_tui::pages::sessions::day_chart(
        &[day("2026-01-01", 10), day("2026-01-02", 20)],
        false,
        80,
        4,
    );
    assert!(!chart.is_empty());
}

#[test]
fn range_change_requests_reload() {
    let mut page = SessionsPage::new();
    let outcome = page.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::empty(),
    ));
    assert_eq!(outcome, PageKeyOutcome::Reload);
}

#[test]
fn app_sessions_reload_bumps_generation() {
    let mut app = App::for_test(PageId::Sessions);
    let before = app.pages.page_dyn(PageId::Sessions).fetch_gen();
    let _ = app.reload_open_page();
    assert!(app.pages.page_dyn(PageId::Sessions).fetch_gen() > before);
}

#[test]
fn enter_with_null_resume_flashes_unavailable() {
    let mut app = App::for_test(PageId::Sessions);
    let mut consent = SessionConsent::default();
    consent.asked = true;
    let mut list = SessionListResponse::default();
    list.consent = consent;
    list.sessions = vec![sample_session("x")];
    app.pages
        .sessions_mut()
        .set_fixture_list(list, SessionStatsResponse::default());
    let action = app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::empty(),
    ));
    assert!(matches!(
        action,
        AppAction::Flash {
            text,
            ok: false
        } if text.contains("can't be resumed")
    ));
}

#[test]
fn enter_with_argv_requests_resume() {
    let mut app = App::for_test(PageId::Sessions);
    let mut row = sample_session("x");
    let mut resume = SessionResume::default();
    resume.argv = vec!["/bin/true".to_owned()];
    resume.cwd = "/".to_owned();
    row.resume = Some(resume);
    let mut consent = SessionConsent::default();
    consent.asked = true;
    let mut list = SessionListResponse::default();
    list.consent = consent;
    list.sessions = vec![row];
    app.pages
        .sessions_mut()
        .set_fixture_list(list, SessionStatsResponse::default());
    let action = app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::empty(),
    ));
    assert!(matches!(action, AppAction::Resume(_)));
}

#[test]
fn reload_key_bumps_generation() {
    let mut app = App::for_test(PageId::Sessions);
    let before = app.pages.page_dyn(PageId::Sessions).fetch_gen();
    let action = app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::empty(),
    ));
    assert!(matches!(action, AppAction::Reload));
    assert!(app.pages.page_dyn(PageId::Sessions).fetch_gen() > before);
}

#[test]
fn range_key_reloads_once_and_stats_key_does_not() {
    let mut app = App::for_test(PageId::Sessions);
    let start = app.pages.page_dyn(PageId::Sessions).fetch_gen();
    let action = app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::empty(),
    ));
    assert!(matches!(action, AppAction::Reload));
    let after_range = app.pages.page_dyn(PageId::Sessions).fetch_gen();
    assert!(after_range > start);
    let action = app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyModifiers::empty(),
    ));
    assert!(matches!(action, AppAction::None));
    assert_eq!(
        app.pages.page_dyn(PageId::Sessions).fetch_gen(),
        after_range
    );
}

#[test]
fn pages_sessions_returns_the_live_page() {
    let mut app = App::for_test(PageId::Sessions);
    let mut consent = SessionConsent::default();
    consent.asked = true;
    let mut list = SessionListResponse::default();
    list.consent = consent;
    list.sessions = vec![sample_session("live")];
    app.pages
        .sessions_mut()
        .set_fixture_list(list, SessionStatsResponse::default());
    assert_eq!(app.pages.sessions().sessions()[0].id, "live");
}

#[test]
fn apply_worker_msg_loads_list_and_maps_404_to_not_in_build() {
    let mut app = App::for_test(PageId::Sessions);
    let generation = 1;
    let mut consent = SessionConsent::default();
    consent.asked = true;
    let mut list = SessionListResponse::default();
    list.consent = consent;
    list.sessions = vec![sample_session("w")];
    hofund_tui::runtime::apply_worker_msg(
        &mut app,
        hofund_tui::runtime::WorkerMsg::SessionsLoad {
            generation,
            outcome: Box::new(hofund_tui::runtime::SessionsLoadOutcome::Ok {
                list,
                stats: Box::new(SessionStatsResponse::default()),
            }),
        },
    );
    assert_eq!(app.pages.sessions().sessions()[0].id, "w");
    assert!(!app.pages.sessions().not_in_build());

    hofund_tui::runtime::apply_worker_msg(
        &mut app,
        hofund_tui::runtime::WorkerMsg::SessionsLoad {
            generation,
            outcome: Box::new(hofund_tui::runtime::SessionsLoadOutcome::NotInBuild),
        },
    );
    assert!(app.pages.sessions().not_in_build());

    hofund_tui::runtime::apply_worker_msg(
        &mut app,
        hofund_tui::runtime::WorkerMsg::SessionsLoad {
            generation,
            outcome: Box::new(hofund_tui::runtime::SessionsLoadOutcome::Err(
                "dashboard down".to_owned(),
            )),
        },
    );
    assert_eq!(app.pages.sessions().load_error(), Some("dashboard down"));
}

#[test]
fn page_shell_reload_updates_fetch_gen() {
    let mut shell = hofund_tui::pages::PageShell::new(PageId::Agents, "not in this build");
    assert_eq!(shell.fetch_gen(), 0);
    shell.reload(7);
    assert_eq!(shell.fetch_gen(), 7);
    let mut app = App::for_test(PageId::Agents);
    let before = app.pages.page_dyn(PageId::Agents).fetch_gen();
    let _ = app.reload_open_page();
    assert!(app.pages.page_dyn(PageId::Agents).fetch_gen() > before);
}
