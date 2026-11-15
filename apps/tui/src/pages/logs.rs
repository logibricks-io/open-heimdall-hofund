//! Daemon log tail (G-7) plus optional request-line overlay from control events.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    EventPayload, LogTailResponse, MAX_LOG_TAIL_BYTES, RequestFinishedEvent, RequestStartedEvent,
    StatusClass,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::intent::ControlIntent;
use crate::state::PageId;
use crate::theme::Theme;
use crate::ui::list_window;

/// Logs tab state.
#[derive(Clone, Debug)]
pub struct LogsPage {
    read_generation: u64,
    file_offset: u64,
    daemon_lines: Vec<String>,
    request_lines: Vec<String>,
    show_requests: bool,
    follow: bool,
    scroll: usize,
    cleared: bool,
    load_error: Option<String>,
    tail_in_flight: bool,
    tail_token: u64,
}

impl Default for LogsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl LogsPage {
    /// Empty page before the first tail read.
    #[must_use]
    pub fn new() -> Self {
        Self {
            read_generation: 0,
            file_offset: 0,
            daemon_lines: Vec::new(),
            request_lines: Vec::new(),
            show_requests: false,
            follow: false,
            scroll: 0,
            cleared: false,
            load_error: None,
            tail_in_flight: false,
            tail_token: 0,
        }
    }

    /// Whether follow mode polls the log at one second.
    #[must_use]
    pub fn follow(&self) -> bool {
        self.follow
    }

    /// Whether request lines from the events stream are shown.
    #[must_use]
    pub fn show_requests(&self) -> bool {
        self.show_requests
    }

    /// Queues one follow poll when follow is on and no tail call is in flight.
    pub fn next_follow_tail(&mut self) -> Option<ControlIntent> {
        if !self.follow || self.tail_in_flight {
            return None;
        }
        Some(self.begin_tail(self.file_offset))
    }

    fn begin_tail(&mut self, after: u64) -> ControlIntent {
        self.tail_in_flight = true;
        self.tail_token = self.tail_token.wrapping_add(1);
        self.load_error = None;
        ControlIntent::TailLogs {
            generation: self.read_generation,
            after,
            token: self.tail_token,
        }
    }

    /// Starts or continues tailing after navigation or reload.
    pub fn queue_initial_tail(&mut self) -> ControlIntent {
        self.begin_tail(self.file_offset)
    }

    /// Applies one G-7 response when the token still matches.
    pub fn apply_tail(&mut self, token: u64, response: LogTailResponse) -> Option<ControlIntent> {
        if token != self.tail_token {
            return None;
        }
        self.tail_in_flight = false;
        self.load_error = None;
        if !self.cleared {
            self.daemon_lines.extend(response.lines);
        } else {
            self.cleared = false;
            self.daemon_lines = response.lines;
        }
        self.file_offset = response.next_offset;
        if self.follow {
            self.scroll = self.visible_line_count().saturating_sub(1);
        }
        let read_bytes = response.next_offset.saturating_sub(response.offset) as usize;
        if read_bytes >= MAX_LOG_TAIL_BYTES {
            return Some(self.begin_tail(self.file_offset));
        }
        None
    }

    /// Records a tail failure.
    pub fn apply_tail_error(&mut self, token: u64, message: String) {
        if token != self.tail_token {
            return;
        }
        self.tail_in_flight = false;
        self.load_error = Some(message);
    }

    /// Appends a formatted line from the requests event topic.
    pub fn push_request_event(&mut self, payload: &EventPayload) {
        let line = match payload {
            EventPayload::RequestStarted(RequestStartedEvent {
                request_id,
                route,
                inbound_protocol,
                ..
            }) => format!("request started {request_id} {route} ({inbound_protocol})"),
            EventPayload::RequestFinished(RequestFinishedEvent {
                request_id,
                status_class,
                duration_ms,
                model,
                ..
            }) => {
                let status = status_label(*status_class);
                if model.is_empty() {
                    format!("request finished {request_id} {status} {duration_ms}ms")
                } else {
                    format!("request finished {request_id} {status} {duration_ms}ms {model}")
                }
            }
            _ => return,
        };
        self.request_lines.push(line);
        if self.show_requests && self.follow {
            self.scroll = self.visible_line_count().saturating_sub(1);
        }
    }

    fn visible_line_count(&self) -> usize {
        self.visible_lines().len()
    }

    fn visible_lines(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for line in &self.daemon_lines {
            out.push(line.as_str());
        }
        if self.show_requests {
            for line in &self.request_lines {
                out.push(line.as_str());
            }
        }
        out
    }

    fn move_scroll(&mut self, delta: i32) {
        let len = self.visible_line_count();
        if len == 0 {
            return;
        }
        self.follow = false;
        self.scroll = (self.scroll as i32 + delta).clamp(0, len.saturating_sub(1) as i32) as usize;
    }

    fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.scroll = self.visible_line_count().saturating_sub(1);
        }
    }

    fn clear_view(&mut self) {
        self.daemon_lines.clear();
        self.request_lines.clear();
        self.scroll = 0;
        self.cleared = true;
    }
}

impl Page for LogsPage {
    fn id(&self) -> PageId {
        PageId::Logs
    }

    fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        self.load_error = None;
    }

    fn on_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_scroll(1);
                PageKeyOutcome::CursorDown
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_scroll(-1);
                PageKeyOutcome::CursorUp
            }
            KeyCode::Char('f') => {
                self.toggle_follow();
                PageKeyOutcome::None
            }
            KeyCode::Char('e') => {
                self.show_requests = !self.show_requests;
                if self.follow {
                    self.scroll = self.visible_line_count().saturating_sub(1);
                }
                PageKeyOutcome::None
            }
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_view();
                PageKeyOutcome::None
            }
            KeyCode::Char('r') => PageKeyOutcome::Reload,
            _ => PageKeyOutcome::None,
        }
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut lines = Vec::new();
        if let Some(error) = &self.load_error {
            lines.push(Line::from(Span::styled(
                format!("  could not read logs: {error}"),
                Style::default().fg(theme.bad()),
            )));
        }
        let visible = self.visible_lines();
        if visible.is_empty() && self.load_error.is_none() {
            let hint = if self.follow {
                "following daemon log…"
            } else {
                "no log lines yet"
            };
            lines.push(Line::from(Span::styled(
                format!("  {hint}"),
                theme.muted_style(),
            )));
        } else {
            let window = list_window(area.height, 3, visible.len(), self.scroll);
            for (index, line) in visible
                .iter()
                .enumerate()
                .skip(window.start)
                .take(window.visible)
            {
                let marker = if index == self.scroll { '>' } else { ' ' };
                lines.push(Line::from(vec![
                    Span::raw(format!("  {marker} ")),
                    Span::styled((*line).to_owned(), theme.text_style()),
                ]));
            }
        }
        let status = format!(
            "  offset {}  follow {}  requests {}",
            self.file_offset,
            if self.follow { "on" } else { "off" },
            if self.show_requests { "on" } else { "off" }
        );
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(status, theme.muted_style())));
        Paragraph::new(lines).render(area, buf);
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

fn status_label(class: StatusClass) -> &'static str {
    match class {
        StatusClass::Informational => "1xx",
        StatusClass::Success => "2xx",
        StatusClass::Redirection => "3xx",
        StatusClass::ClientError => "4xx",
        StatusClass::ServerError => "5xx",
        StatusClass::Unknown => "unknown",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use hofund_control_client::hofund_control_proto::LogTailResponse;

    use super::LogsPage;
    use crate::pages::Page;

    #[test]
    fn tail_appends_lines_and_advances_offset() {
        let mut page = LogsPage::new();
        page.reload(1);
        let mut response = LogTailResponse::default();
        response.lines = vec!["alpha".to_owned(), "beta".to_owned()];
        response.offset = 0;
        response.next_offset = 12;
        page.tail_token = 1;
        let follow = page.apply_tail(1, response);
        assert!(follow.is_none());
        assert_eq!(page.file_offset, 12);
        assert_eq!(page.daemon_lines, ["alpha", "beta"]);
    }

    #[test]
    fn clear_drops_visible_lines_but_keeps_offset() {
        let mut page = LogsPage::new();
        page.file_offset = 99;
        page.daemon_lines = vec!["old".to_owned()];
        page.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::empty(),
        ));
        assert!(page.daemon_lines.is_empty());
        assert_eq!(page.file_offset, 99);
        assert!(page.cleared);
    }

    #[test]
    fn follow_snaps_scroll_to_end() {
        let mut page = LogsPage::new();
        page.follow = true;
        let mut response = LogTailResponse::default();
        response.lines = vec!["line".to_owned()];
        response.next_offset = 4;
        page.tail_token = 1;
        page.apply_tail(1, response);
        assert_eq!(page.scroll, 0);
    }
}
