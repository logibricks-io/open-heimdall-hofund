//! Read-only Library page.

use crossterm::event::{KeyCode, KeyEvent};
use hofund_control_proto::{LibraryAgent, LibraryResponse, LibraryRow};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::state::PageId;
use crate::theme::Theme;

const SECTIONS: &[(&str, &str)] = &[
    (
        "instructions",
        "instructions · what every agent reads before a conversation",
    ),
    ("mcp", "MCP servers"),
    ("skill", "skills"),
    ("found-mcp", "MCP servers found in agents"),
    ("found-skill", "skills found in agents"),
    ("rtk", "RTK"),
];

/// Library tab state.
#[derive(Clone, Debug)]
pub struct LibraryPage {
    not_in_build: bool,
    load_error: Option<String>,
    loading: bool,
    cursor: usize,
    view: LibraryResponse,
    read_generation: u64,
}

impl Default for LibraryPage {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryPage {
    /// Empty page before the first control read.
    #[must_use]
    pub fn new() -> Self {
        Self {
            not_in_build: false,
            load_error: None,
            loading: true,
            cursor: 0,
            view: LibraryResponse::default(),
            read_generation: 0,
        }
    }

    /// Applies a successful library read.
    pub fn apply_load(&mut self, view: LibraryResponse) {
        self.view = view;
        self.loading = false;
        self.load_error = None;
        self.not_in_build = false;
        self.cursor = self.cursor.min(self.view.rows.len().saturating_sub(1));
    }

    /// Records a library read failure without discarding the previous rows.
    pub fn apply_load_error(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Applies a deferred-route probe outcome.
    pub fn apply_probe(&mut self, outcome: super::ProbeOutcome) {
        match outcome {
            super::ProbeOutcome::NotInBuild => {
                self.not_in_build = true;
                self.loading = false;
            }
            super::ProbeOutcome::Ready => {
                self.not_in_build = false;
                self.loading = true;
            }
            super::ProbeOutcome::Error(message) => self.apply_load_error(message),
        }
    }

    /// Current cursor index.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Current library view.
    #[must_use]
    pub fn view(&self) -> &LibraryResponse {
        &self.view
    }

    /// Whether the route is absent from this build.
    #[must_use]
    pub fn not_in_build(&self) -> bool {
        self.not_in_build
    }

    /// Most recent transport or decode error.
    #[must_use]
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.view.rows.len();
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len as i32) as usize;
    }

    fn empty_message(&self) -> String {
        if self.not_in_build {
            return "Library is not in this build yet.".to_owned();
        }
        if let Some(error) = &self.load_error {
            return format!("Could not load library: {error}");
        }
        if self.loading {
            return "Loading library…".to_owned();
        }
        if let Some(error) = &self.view.error {
            return error.clone();
        }
        "No library entries.".to_owned()
    }
}

impl Page for LibraryPage {
    fn id(&self) -> PageId {
        PageId::Library
    }

    fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        if !self.not_in_build {
            self.loading = true;
            self.load_error = None;
        }
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
            _ => PageKeyOutcome::None,
        }
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let rows = ordered_rows(&self.view.rows);
        let mut lines = Vec::new();
        if rows.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", self.empty_message()),
                theme.muted_style(),
            )));
        } else {
            draw_rows(self, &rows, area, theme, &mut lines);
        }
        if !self.not_in_build && !self.loading && self.load_error.is_none() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("  kept in {}", self.view.dir),
                theme.muted_style(),
            )));
        }
        Paragraph::new(lines).render(area, buf);
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

fn ordered_rows(rows: &[LibraryRow]) -> Vec<&LibraryRow> {
    let mut ordered = Vec::with_capacity(rows.len());
    for (kind, _) in SECTIONS {
        ordered.extend(rows.iter().filter(|row| row.kind == *kind));
    }
    ordered.extend(
        rows.iter()
            .filter(|row| !SECTIONS.iter().any(|(kind, _)| row.kind == *kind)),
    );
    ordered
}

fn draw_rows(
    page: &LibraryPage,
    rows: &[&LibraryRow],
    area: Rect,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
) {
    let visible = usize::from(area.height.saturating_sub(3)).max(4);
    let start = window_start(rows, page.cursor, visible);
    let name_width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(32);
    let mut spent = 0;
    let mut last_kind = None;
    for (index, row) in rows.iter().enumerate().skip(start) {
        if spent >= visible {
            break;
        }
        let new_section = last_kind != Some(row.kind.as_str());
        if new_section {
            if last_kind.is_some() || start > 0 {
                lines.push(Line::from(""));
                spent += 1;
            }
            let heading = SECTIONS
                .iter()
                .find(|(kind, _)| *kind == row.kind)
                .map(|(_, heading)| (*heading).to_owned())
                .unwrap_or_else(|| {
                    if row.kind.is_empty() {
                        "other".to_owned()
                    } else {
                        row.kind.clone()
                    }
                });
            lines.push(Line::from(Span::styled(
                format!("  {heading}"),
                theme.muted_style(),
            )));
            spent += 1;
            last_kind = Some(row.kind.as_str());
        }
        lines.push(row_line(
            row,
            &page.view.agents,
            index == page.cursor,
            name_width,
            area.width,
            theme,
        ));
        spent += 1;
    }
}

fn window_start(rows: &[&LibraryRow], cursor: usize, visible: usize) -> usize {
    let mut start = cursor.min(rows.len().saturating_sub(1));
    while start > 0 && window_spend(rows, start - 1, cursor) <= visible {
        start -= 1;
    }
    start
}

fn window_spend(rows: &[&LibraryRow], start: usize, cursor: usize) -> usize {
    let mut spent = 0;
    for index in start..=cursor.min(rows.len().saturating_sub(1)) {
        spent += 1;
        if index == start || rows[index].kind != rows[index - 1].kind {
            spent += 2;
            if index == 0 {
                spent -= 1;
            }
        }
    }
    spent
}

fn row_line(
    row: &LibraryRow,
    agents: &[LibraryAgent],
    selected: bool,
    name_width: usize,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let known = SECTIONS.iter().any(|(kind, _)| row.kind == *kind);
    let marker = if selected { "> " } else { "  " };
    let name_style = if !known {
        theme.muted_style()
    } else if selected {
        theme.cursor_style()
    } else {
        theme.text_style()
    };
    let name = pad_right(truncate(&row.name, name_width), name_width);
    let mut names = row
        .on
        .iter()
        .map(|id| {
            agents
                .iter()
                .find(|agent| agent.id == *id)
                .map_or(id.as_str(), |agent| agent.name.as_str())
        })
        .collect::<Vec<_>>()
        .join(", ");
    if row.own {
        names.push_str("'s own");
    } else if names.is_empty() && !row.kind.starts_with("found-") {
        names = "no agent".to_owned();
    }
    let mut spans = vec![
        Span::styled("  ", theme.muted_style()),
        Span::styled(marker, theme.cursor_style()),
        Span::styled(name, name_style),
    ];
    let mut used = 4 + name_width;
    if !names.is_empty() {
        spans.push(Span::styled(format!("  {names}"), theme.muted_style()));
        used += 2 + names.chars().count();
    }
    if row.bad > 0 {
        let warning = format!("  ! {}", row.bad);
        used += warning.chars().count();
        spans.push(Span::styled(warning, Style::default().fg(theme.bad())));
    }
    let room = usize::from(width).saturating_sub(used + 4);
    if !row.about.is_empty() && room > 8 {
        spans.push(Span::styled(
            format!("  {}", truncate(&row.about, room)),
            theme.muted_style(),
        ));
    }
    Line::from(spans)
}

fn truncate(value: &str, width: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= width {
        return normalized;
    }
    let mut out = normalized
        .chars()
        .take(width.saturating_sub(1).max(1))
        .collect::<String>();
    out.push('…');
    out
}

fn pad_right(value: String, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    format!("{value}{}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::window_start;
    use hofund_control_proto::LibraryRow;

    fn rows(kinds: &[&str]) -> Vec<LibraryRow> {
        kinds
            .iter()
            .map(|kind| {
                let mut row = LibraryRow::default();
                row.kind = (*kind).to_owned();
                row
            })
            .collect()
    }

    #[test]
    fn section_heads_and_blank_lines_count_toward_the_window() {
        let rows = rows(&["instructions", "mcp", "skill", "found-mcp"]);
        let borrowed = rows.iter().collect::<Vec<_>>();
        assert_eq!(window_start(&borrowed, 3, 3), 3);
    }

    #[test]
    fn cursor_stays_inside_a_scrolled_window() {
        let rows = rows(&["mcp"; 20]);
        let borrowed = rows.iter().collect::<Vec<_>>();
        for cursor in 0..rows.len() {
            let start = window_start(&borrowed, cursor, 4);
            assert!(start <= cursor);
            assert!(start + 4 > cursor);
        }
    }
}
