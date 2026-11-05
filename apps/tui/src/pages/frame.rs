//! Shared empty page frame with list window and cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::state::{Generation, PageId};
use crate::theme::Theme;
use crate::ui::list_window;

/// Empty list shell used for every page in R-11.
#[derive(Clone, Debug)]
pub struct PageShell {
    id: PageId,
    cursor: usize,
    rows: Vec<String>,
    message: String,
    not_in_build: bool,
    fetch_gen: Generation,
}

impl PageShell {
    /// Creates a page with a default empty message.
    #[must_use]
    pub fn new(id: PageId, empty_message: impl Into<String>) -> Self {
        Self {
            id,
            cursor: 0,
            rows: Vec::new(),
            message: empty_message.into(),
            not_in_build: matches!(id, PageId::Sessions | PageId::Library),
            fetch_gen: 0,
        }
    }

    /// Marks the deferred API as absent (HTTP 404).
    pub fn set_not_in_build(&mut self, value: bool) {
        self.not_in_build = value;
        if value {
            self.message = match self.id {
                PageId::Sessions => "Sessions are not in this build yet.".to_owned(),
                PageId::Library => "Library is not in this build yet.".to_owned(),
                _ => self.message.clone(),
            };
        }
    }

    /// Sets the center message when the list is empty.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// Replaces placeholder rows (for render tests).
    pub fn set_rows(&mut self, rows: Vec<String>) {
        self.rows = rows;
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Current cursor index.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Last fetch generation recorded.
    #[must_use]
    pub fn fetch_gen(&self) -> Generation {
        self.fetch_gen
    }

    fn move_cursor(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len();
        let next = self.cursor as i32 + delta;
        self.cursor = next.rem_euclid(len as i32) as usize;
    }
}

impl Page for PageShell {
    fn id(&self) -> PageId {
        self.id
    }

    fn reload(&mut self, generation: u64) {
        self.fetch_gen = generation;
    }

    fn on_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor(1);
                PageKeyOutcome::CursorDown
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor(-1);
                PageKeyOutcome::CursorUp
            }
            KeyCode::Char('J') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_cursor(1);
                PageKeyOutcome::CursorDown
            }
            KeyCode::Char('K') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_cursor(-1);
                PageKeyOutcome::CursorUp
            }
            _ => PageKeyOutcome::None,
        }
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let min_rows = match self.id {
            PageId::Library => 4,
            PageId::Agents => 3,
            _ => 3,
        };
        let window = list_window(area.height, min_rows, self.rows.len(), self.cursor);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", self.message),
                theme.muted_style(),
            )));
        } else {
            for (index, row) in self
                .rows
                .iter()
                .enumerate()
                .skip(window.start)
                .take(window.visible)
            {
                let marker = if index == self.cursor { "> " } else { "  " };
                let style = if index == self.cursor {
                    theme.cursor_style()
                } else {
                    theme.text_style()
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, theme.cursor_style()),
                    Span::styled(row.clone(), style),
                ]));
            }
        }
        Paragraph::new(lines)
            .block(Block::default())
            .render(area, buf);
    }
}
