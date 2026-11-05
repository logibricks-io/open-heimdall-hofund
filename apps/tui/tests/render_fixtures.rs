//! Render fixture: agents page empty state at 100x30.

use std::path::PathBuf;

use hofund_tui::ui;
use hofund_tui::{App, PageId, Theme, buffer_text};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
#[ignore = "refresh fixtures after intentional visual changes"]
fn write_agents_empty_fixture() {
    let app = App::for_test(PageId::Agents);
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| ui::draw(&app, frame.area(), frame.buffer_mut(), &Theme::monochrome()))
        .expect("draw");
    let text = buffer_text(terminal.backend().buffer(), 100, 30);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agents_empty.txt");
    std::fs::write(path, text).expect("write fixture");
}

#[test]
fn agents_empty_matches_fixture() {
    let app = App::for_test(PageId::Agents);
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| ui::draw(&app, frame.area(), frame.buffer_mut(), &Theme::monochrome()))
        .expect("draw");
    let text = buffer_text(terminal.backend().buffer(), 100, 30);
    let fixture = include_str!("fixtures/agents_empty.txt");
    assert_eq!(text, fixture);
}
