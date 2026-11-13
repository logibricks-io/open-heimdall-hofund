//! Layout, header, status, and footer rendering.

mod layout;

pub use layout::list_window;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::App;
use crate::pages::Page;
use crate::state::Mode;
use crate::theme::Theme;

const PAD: &str = "  ";

/// Paints one full frame.
pub fn draw(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(footer_lines(app)),
    ])
    .split(area);

    draw_header(app, chunks[0], buf, theme);
    if app.mode == Mode::Help {
        draw_help(chunks[1], buf, theme);
    } else {
        draw_body(app, chunks[1], buf, theme);
    }
    draw_status(app, chunks[2], buf, theme);
    draw_footer(app, chunks[3], buf, theme);
}

fn draw_body(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if app.page == crate::state::PageId::Providers && app.providers.active() {
        app.providers.draw(area, buf, theme);
        return;
    }
    if app.page == crate::state::PageId::Agents && app.agents.active() {
        app.agents.draw(area, buf, theme);
        return;
    }
    app.pages.page(app.page).draw_body(area, buf, theme);
}

fn footer_lines(app: &App) -> u16 {
    if interactive_page(app) { 2 } else { 1 }
}

fn interactive_page(app: &App) -> bool {
    (app.page == crate::state::PageId::Providers && app.providers.active())
        || (app.page == crate::state::PageId::Agents && app.agents.active())
}

fn draw_header(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(format!("{PAD}◉ hofund"), theme.title_style()));
    spans.push(Span::raw("  "));
    for page in crate::state::PageId::CYCLE {
        let label = page.pill_label();
        if page == app.page {
            spans.push(Span::styled(format!(" {label} "), theme.pill_style()));
        } else {
            spans.push(Span::styled(format!(" {label} "), theme.muted_style()));
        }
        spans.push(Span::raw(" "));
    }
    Paragraph::new(Line::from(spans)).render(area, buf);
}

fn draw_help(area: Rect, buf: &mut Buffer, theme: &Theme) {
    let lines = vec![
        Line::from(Span::styled(
            format!("{PAD}Help (placeholder — R-18)"),
            theme.title_style(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{PAD}Keys for this page will appear here."),
            theme.muted_style(),
        )),
        Line::from(Span::styled(
            format!("{PAD}Press esc to return."),
            theme.muted_style(),
        )),
    ];
    Paragraph::new(lines).render(area, buf);
}

fn draw_status(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let line = if let Some(pending) = &app.pending {
        format!("{PAD}… {}", pending.label)
    } else if let Some(flash) = &app.flash {
        let (prefix, style) = theme.status_prefix(flash.ok);
        let spans = vec![
            Span::styled(format!("{PAD}{prefix}"), style),
            Span::styled(flash.text.clone(), theme.text_style()),
        ];
        Paragraph::new(Line::from(spans)).render(area, buf);
        return;
    } else if app.catalog_syncing {
        Paragraph::new(Line::from(Span::styled(
            format!("{PAD}… syncing model catalog"),
            theme.muted_style(),
        )))
        .render(area, buf);
        return;
    } else {
        format!("{PAD}{}", app.gateway.status_line())
    };
    Paragraph::new(Line::from(Span::styled(line, theme.text_style()))).render(area, buf);
}

fn draw_footer(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if interactive_page(app) && app.mode != Mode::Help {
        let lines = match app.page {
            crate::state::PageId::Providers => app.providers.hint_lines(),
            crate::state::PageId::Agents => app.agents.hint_lines(),
            _ => ("", ""),
        };
        let text = vec![
            Line::from(Span::styled(
                format!("{PAD}{}", lines.0),
                theme.muted_style(),
            )),
            Line::from(Span::styled(
                format!("{PAD}{}", lines.1),
                theme.muted_style(),
            )),
        ];
        Paragraph::new(text).render(area, buf);
        return;
    }
    let hints = if app.mode == Mode::Help {
        "esc  back"
    } else {
        match app.page {
            crate::state::PageId::Agents => {
                "↑↓  agent  ←→  field  ↵  change  1–6  pages  ?  help  q  quit"
            }
            crate::state::PageId::Providers => {
                "↑↓  provider  1–6  pages  r  reload  ?  help  q  quit"
            }
            crate::state::PageId::Routing => "↑↓  group  1–6  pages  r  reload  ?  help  q  quit",
            crate::state::PageId::Usage => "←→  period  1–6  pages  r  reload  ?  help  q  quit",
            crate::state::PageId::Sessions => {
                "↑↓  session  1–6  pages  r  reload  ?  help  q  quit"
            }
            crate::state::PageId::Library => "↑↓  item  1–6  pages  r  reload  ?  help  q  quit",
        }
    };
    Paragraph::new(Line::from(Span::styled(
        format!("{PAD}{hints}"),
        theme.muted_style(),
    )))
    .render(area, buf);
}

/// Serializes the buffer to plain text for fixture tests.
#[must_use]
pub fn buffer_text(buf: &Buffer, width: u16, height: u16) -> String {
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            let cell = buf[(x, y)].symbol();
            out.push_str(cell);
        }
        if y + 1 < height {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{buffer_text, draw};
    use crate::app::App;
    use crate::state::PageId;
    use crate::theme::Theme;

    fn render(app: &App) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(app, frame.area(), frame.buffer_mut(), &Theme::monochrome()))
            .expect("draw");
        let backend = terminal.backend();
        buffer_text(backend.buffer(), 100, 30)
    }

    #[test]
    fn each_page_empty_state_fixture() {
        for page in PageId::CYCLE {
            let app = App::for_test(page);
            let text = render(&app);
            assert!(
                text.contains(page.slug()),
                "missing slug for {page:?}: {text}"
            );
        }
    }

    #[test]
    fn toast_appears_on_status_line() {
        let mut app = App::for_test(PageId::Agents);
        app.set_flash(crate::state::Flash::new("saved", true));
        let text = render(&app);
        assert!(text.contains("ok"));
        assert!(text.contains("saved"));
    }

    #[test]
    fn narrow_height_keeps_cursor_marker() {
        let mut app = App::for_test(PageId::Providers);
        app.pages.mut_page(PageId::Providers).set_rows(vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
            "e".to_owned(),
        ]);
        for _ in 0..3 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::empty(),
            ));
        }
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(&app, frame.area(), frame.buffer_mut(), &Theme::monochrome()))
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer(), 80, 10);
        assert!(text.contains('>'));
    }
}
