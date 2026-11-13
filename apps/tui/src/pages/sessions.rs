//! Sessions page (list, filters, stats chart, resume from server argv).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_proto::{
    SessionDayStats, SessionListResponse, SessionResume, SessionStatsResponse, SessionSummary,
    SessionTokenTotals,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::state::PageId;
use crate::theme::Theme;
use crate::ui::list_window;

/// Range tab names and `days` query values (0 = all).
const SESS_RANGES: &[(u32, &str)] = &[
    (1, "today"),
    (7, "7 days"),
    (30, "30 days"),
    (90, "90 days"),
    (0, "all"),
];

const ALL_MODELS: &str = "all models";
const ALL_FOLDERS: &str = "all folders";

/// Sessions tab state.
#[derive(Clone, Debug)]
pub struct SessionsPage {
    not_in_build: bool,
    load_error: Option<String>,
    loading: bool,
    cursor: usize,
    range_index: usize,
    model_filter: String,
    folder_filter: String,
    stats_view: bool,
    stats_by_cost: bool,
    list: SessionListResponse,
    stats: SessionStatsResponse,
    read_generation: u64,
    picker: Option<FilterPicker>,
}

#[derive(Clone, Debug)]
struct FilterPicker {
    folder: bool,
    items: Vec<String>,
    cursor: usize,
}

impl Default for SessionsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionsPage {
    /// Empty page before the first fetch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            not_in_build: false,
            load_error: None,
            loading: true,
            cursor: 0,
            range_index: 1,
            model_filter: String::new(),
            folder_filter: String::new(),
            stats_view: false,
            stats_by_cost: false,
            list: SessionListResponse::default(),
            stats: SessionStatsResponse::default(),
            read_generation: 0,
            picker: None,
        }
    }

    /// Whether the sessions route is absent from this build.
    #[must_use]
    pub fn not_in_build(&self) -> bool {
        self.not_in_build
    }

    /// Last fetch error, if the most recent load failed.
    #[must_use]
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Last fetch generation recorded.
    #[must_use]
    pub fn fetch_gen(&self) -> u64 {
        self.read_generation
    }

    /// `days` query for the active range tab.
    #[must_use]
    pub fn days(&self) -> u32 {
        SESS_RANGES[self.range_index].0
    }

    /// Applies a control GET probe outcome.
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
            super::ProbeOutcome::Error(message) => {
                self.load_error = Some(message);
                self.loading = false;
            }
        }
    }

    /// Merges list and stats from a successful load.
    pub fn apply_load(&mut self, list: SessionListResponse, stats: SessionStatsResponse) {
        self.loading = false;
        self.load_error = None;
        self.not_in_build = false;
        self.list = list;
        self.stats = stats;
        self.cursor = self.cursor.min(self.list.sessions.len().saturating_sub(1));
    }

    /// Records a fetch failure.
    pub fn apply_load_error(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Fixture hook: replace list rows.
    pub fn set_fixture_list(&mut self, list: SessionListResponse, stats: SessionStatsResponse) {
        self.not_in_build = false;
        self.loading = false;
        self.list = list;
        self.stats = stats;
    }

    #[must_use]
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.list.sessions
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.sessions().len();
        if len == 0 {
            return;
        }
        let next = self.cursor as i32 + delta;
        self.cursor = next.rem_euclid(len as i32) as usize;
    }

    fn clear_filters(&mut self) {
        self.model_filter.clear();
        self.folder_filter.clear();
        self.cursor = 0;
    }

    fn open_picker(&mut self, folder: bool) {
        let shares = if folder {
            &self.stats.folders
        } else {
            &self.stats.models
        };
        let mut items = vec![String::new()];
        for share in shares {
            if !share.name.is_empty() {
                items.push(share.name.clone());
            }
        }
        self.picker = Some(FilterPicker {
            folder,
            items,
            cursor: 0,
        });
    }

    fn apply_pick(&mut self, value: &str) {
        if let Some(picker) = self.picker.take() {
            if picker.folder {
                self.folder_filter = value.to_owned();
            } else {
                self.model_filter = value.to_owned();
            }
            self.cursor = 0;
        }
    }

    fn resume_target(&self) -> Option<&SessionResume> {
        let session = self.sessions().get(self.cursor)?;
        session.resume.as_ref().filter(|r| !r.argv.is_empty())
    }

    fn empty_message(&self) -> String {
        if self.not_in_build {
            return "Sessions are not in this build yet.".to_owned();
        }
        if let Some(err) = &self.load_error {
            return format!("Could not load: {err}");
        }
        if self.loading {
            return "Loading sessions…".to_owned();
        }
        if !self.list.consent.asked {
            return "hofund reads no agent session history until you allow it · \
                    hofund sessions read on all (or on <agent>)"
                .to_owned();
        }
        if !self.model_filter.is_empty() || !self.folder_filter.is_empty() {
            return "no session matches · x clears the filters".to_owned();
        }
        "nothing in this range · agent sessions on this machine show up here".to_owned()
    }
}

impl Page for SessionsPage {
    fn id(&self) -> PageId {
        PageId::Sessions
    }

    fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        if !self.not_in_build {
            self.loading = true;
            self.load_error = None;
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        if self.not_in_build {
            return PageKeyOutcome::None;
        }
        if let Some(picker) = &mut self.picker {
            return handle_picker_key(picker, key);
        }
        if !self.stats_view {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_cursor(1);
                    return PageKeyOutcome::CursorDown;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_cursor(-1);
                    return PageKeyOutcome::CursorUp;
                }
                KeyCode::Enter => {
                    if let Some(resume) = self.resume_target().cloned() {
                        return PageKeyOutcome::Resume(resume);
                    }
                    return PageKeyOutcome::FlashResumeUnavailable;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('s') | KeyCode::Tab => {
                self.stats_view = !self.stats_view;
                PageKeyOutcome::None
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.range_index = (self.range_index + 1) % SESS_RANGES.len();
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let n = SESS_RANGES.len();
                self.range_index = (self.range_index + n - 1) % n;
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('t') => {
                self.range_index = 0;
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('w') => {
                self.range_index = 1;
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('m') if key.modifiers.is_empty() => {
                self.range_index = 2;
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.range_index = SESS_RANGES.len() - 1;
                self.cursor = 0;
                PageKeyOutcome::Reload
            }
            KeyCode::Char('c') => {
                self.stats_by_cost = !self.stats_by_cost;
                PageKeyOutcome::None
            }
            KeyCode::Char('M') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.open_picker(false);
                PageKeyOutcome::None
            }
            KeyCode::Char('f') => {
                self.open_picker(true);
                PageKeyOutcome::None
            }
            KeyCode::Char('x') => {
                self.clear_filters();
                PageKeyOutcome::Reload
            }
            _ => PageKeyOutcome::None,
        }
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if let Some(picker) = &self.picker {
            draw_picker(area, buf, theme, picker);
            return;
        }
        if self.stats_view {
            draw_stats(self, area, buf, theme);
        } else if self.sessions().is_empty() {
            draw_empty(self, area, buf, theme);
        } else {
            draw_list(self, area, buf, theme);
        }
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

impl SessionsPage {
    /// Builds the list query for the current filters and range.
    #[must_use]
    pub fn list_query(&self) -> hofund_control_proto::SessionListQuery {
        let mut query = hofund_control_proto::SessionListQuery::default();
        query.days = Some(self.days());
        query.model = self.model_filter.clone();
        query.folder = self.folder_filter.clone();
        query
    }

    /// Builds the stats query for the current filters and range.
    #[must_use]
    pub fn stats_query(&self) -> hofund_control_proto::SessionStatsQuery {
        let mut query = hofund_control_proto::SessionStatsQuery::default();
        query.days = Some(self.days());
        query.model = self.model_filter.clone();
        query.folder = self.folder_filter.clone();
        query
    }

    /// Applies a filter pick from the picker.
    pub fn apply_filter_pick(&mut self, value: String) {
        self.apply_pick(&value);
    }

    /// Clears an open picker.
    pub fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Whether the filter picker is open and should capture keys.
    #[must_use]
    pub fn captures(&self) -> bool {
        self.picker.is_some()
    }
}

fn handle_picker_key(picker: &mut FilterPicker, key: KeyEvent) -> PageKeyOutcome {
    match key.code {
        KeyCode::Esc => PageKeyOutcome::ClosePicker,
        KeyCode::Char('j') | KeyCode::Down => {
            let len = picker.items.len();
            if len > 0 {
                picker.cursor = (picker.cursor + 1).min(len - 1);
            }
            PageKeyOutcome::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            picker.cursor = picker.cursor.saturating_sub(1);
            PageKeyOutcome::None
        }
        KeyCode::Enter => {
            let value = picker
                .items
                .get(picker.cursor)
                .map(String::as_str)
                .unwrap_or("");
            PageKeyOutcome::PickFilter(value.to_owned())
        }
        _ => PageKeyOutcome::None,
    }
}

fn draw_empty(page: &SessionsPage, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let lines = vec![
        Line::from(Span::styled("  ", theme.muted_style())),
        draw_bar(page, theme),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", page.empty_message()),
            theme.muted_style(),
        )),
    ];
    Paragraph::new(lines)
        .block(Block::default())
        .render(area, buf);
}

fn draw_list(page: &SessionsPage, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let sessions = page.sessions();
    let mut lines = vec![
        Line::from(Span::styled("  ", theme.muted_style())),
        draw_bar(page, theme),
        Line::from(""),
    ];
    let footer_lines: u16 = if page.resume_target().is_some() { 2 } else { 0 };
    let item_rows = area.height.saturating_sub(3 + footer_lines).max(3);
    let window = list_window(item_rows, 3, sessions.len(), page.cursor);
    for (index, session) in sessions
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let marker = if index == page.cursor { "> " } else { "  " };
        let style = if index == page.cursor {
            theme.cursor_style()
        } else {
            theme.text_style()
        };
        let line = format_list_row(session, item_rows as usize);
        lines.push(Line::from(vec![
            Span::styled(marker, theme.cursor_style()),
            Span::styled(line, style),
        ]));
    }
    if let Some(resume) = page.resume_target() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  enter runs {}", resume.argv.join(" ")),
            theme.muted_style(),
        ));
    }
    Paragraph::new(lines)
        .block(Block::default())
        .render(area, buf);
}

fn format_list_row(session: &SessionSummary, _width: usize) -> String {
    let age = sess_ago(session.last_ms);
    let folder = folder_base(&session.cwd);
    let tokens = fmt_tokens(token_spent(session.tokens));
    let cost = fmt_cost(session.cost, session.unpriced);
    let title = session
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = if title.is_empty() {
        "(untitled)"
    } else {
        title.as_str()
    };
    format!(
        "{:<10}  {:<10}  {:<20}  {:>7}  {:<8}  {}",
        age,
        trunc_field(&session.agent, 10),
        trunc_field(&folder, 20),
        tokens,
        cost,
        title
    )
}

fn draw_stats(page: &SessionsPage, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let mut lines = vec![
        Line::from(Span::styled("  ", theme.muted_style())),
        draw_bar(page, theme),
        Line::from(""),
    ];
    let spent = token_spent(page.stats.tokens);
    if spent == 0 && page.stats.cost == 0.0 {
        lines.push(Line::from(Span::styled(
            format!("  {}", page.empty_message()),
            theme.muted_style(),
        )));
    } else {
        let totals = format!(
            "{} tokens · {} · cache read {}",
            fmt_tokens(spent),
            fmt_cost(page.stats.cost, page.stats.unpriced),
            fmt_tokens(page.stats.tokens.cache_read),
        );
        lines.push(Line::from(vec![
            Span::styled("  ", theme.muted_style()),
            Span::styled(totals, theme.text_style()),
        ]));
        let detail = format!(
            "in {}  out {}  cache write {}",
            fmt_tokens(page.stats.tokens.input),
            fmt_tokens(page.stats.tokens.output),
            fmt_tokens(page.stats.tokens.cache_write),
        );
        lines.push(Line::from(Span::styled(
            format!("  {detail}"),
            theme.muted_style(),
        )));
        lines.push(Line::from(""));
        let chart_w = area.width.saturating_sub(4);
        for row in day_chart(&page.stats.days, page.stats_by_cost, chart_w, 6) {
            lines.push(Line::from(Span::styled(
                format!("  {row}"),
                theme.text_style(),
            )));
        }
    }
    Paragraph::new(lines)
        .block(Block::default())
        .render(area, buf);
}

fn draw_bar(page: &SessionsPage, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme.muted_style())];
    for (index, (_, name)) in SESS_RANGES.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if index == page.range_index {
            theme.pill_style()
        } else {
            theme.muted_style()
        };
        spans.push(Span::styled(format!(" {name} "), style));
    }
    spans.push(Span::raw("    "));
    if page.model_filter.is_empty() {
        spans.push(Span::styled(ALL_MODELS.to_owned(), theme.muted_style()));
    } else {
        spans.push(Span::styled(
            page.model_filter.clone(),
            theme.cursor_style(),
        ));
    }
    spans.push(Span::styled(" · ", theme.muted_style()));
    if page.folder_filter.is_empty() {
        spans.push(Span::styled(ALL_FOLDERS.to_owned(), theme.muted_style()));
    } else {
        spans.push(Span::styled(
            tilde_path(&page.folder_filter),
            theme.cursor_style(),
        ));
    }
    spans.push(Span::raw("    "));
    if page.stats_view {
        spans.push(Span::styled("sessions · ", theme.muted_style()));
        spans.push(Span::styled("stats", theme.pill_style()));
    } else {
        spans.push(Span::styled("sessions", theme.pill_style()));
        spans.push(Span::styled(" · stats", theme.muted_style()));
    }
    Line::from(spans)
}

fn draw_picker(area: Rect, buf: &mut Buffer, theme: &Theme, picker: &FilterPicker) {
    let label = if picker.folder { "folder" } else { "model" };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  sessions · pick {label}"),
            theme.title_style(),
        )),
        Line::from(""),
    ];
    let item_rows = area.height.saturating_sub(3).max(3);
    let window = list_window(item_rows, 3, picker.items.len(), picker.cursor);
    for (index, item) in picker
        .items
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let marker = if index == picker.cursor { "> " } else { "  " };
        let label = if item.is_empty() {
            if picker.folder {
                ALL_FOLDERS.to_owned()
            } else {
                ALL_MODELS.to_owned()
            }
        } else {
            item.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, theme.cursor_style()),
            Span::styled(label, theme.text_style()),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  esc cancels",
        theme.muted_style(),
    )));
    Paragraph::new(lines)
        .block(Block::default())
        .render(area, buf);
}

/// Daily bar chart rows (newest-first totals on the server; days in date order).
#[must_use]
pub fn day_chart(days: &[SessionDayStats], by_cost: bool, width: u16, height: u16) -> Vec<String> {
    if days.len() < 2 || height < 1 {
        return Vec::new();
    }
    let label = 10usize;
    let cols = usize::from(width).saturating_sub(label).max(8);
    let mut step = 1usize;
    while days.len().div_ceil(step) > cols {
        step += 1;
    }
    if step > 1 && step < 7 && days.len().div_ceil(7) <= cols {
        step = 7;
    }
    #[derive(Clone)]
    struct Bucket {
        date: String,
        value: f64,
    }
    let mut buckets = Vec::new();
    let mut index = 0;
    while index < days.len() {
        let mut bucket = Bucket {
            date: days[index].date.clone(),
            value: 0.0,
        };
        for day in &days[index..days.len().min(index + step)] {
            bucket.value += if by_cost {
                day.cost
            } else {
                token_spent(day.tokens) as f64
            };
        }
        buckets.push(bucket);
        index += step;
    }
    let peak = buckets.iter().map(|b| b.value).fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return Vec::new();
    }
    let gap = if buckets.len() * 2 <= cols { " " } else { "" };
    const BLOCKS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let height = usize::from(height);
    let mut lines = vec![String::new(); height];
    for (row, line) in lines.iter_mut().enumerate().take(height) {
        let level = height - 1 - row;
        let mut row_text = String::new();
        for (col, bucket) in buckets.iter().enumerate() {
            if col > 0 {
                row_text.push_str(gap);
            }
            let mut eighths = if bucket.value > 0.0 {
                ((bucket.value * (height * 8) as f64) / peak).round() as i32
            } else {
                0
            };
            if bucket.value > 0.0 {
                eighths = eighths.max(1);
            }
            let idx = (eighths - i32::try_from(level * 8).unwrap_or(0)).clamp(0, 8) as usize;
            row_text.push(BLOCKS[idx]);
        }
        *line = row_text;
    }
    let peak_label = if by_cost {
        fmt_cost(peak, 0)
    } else {
        fmt_tokens(peak as u64)
    };
    lines[0].push_str(&format!("  {peak_label}"));
    let unit = if step == 7 {
        "a week".to_owned()
    } else if step > 1 {
        format!("{step} days")
    } else {
        "a day".to_owned()
    };
    let metric = if by_cost { "cost" } else { "tokens" };
    lines[height - 1].push_str(&format!("  {metric} · {unit}"));
    if let (Some(first), Some(last)) = (buckets.first(), buckets.last()) {
        let span = buckets.len() + (buckets.len().saturating_sub(1)) * gap.len();
        let pad = cols.saturating_sub(span);
        lines[height - 1].push_str(&format!(
            "{:>pad$}  {} … {}",
            "",
            first.date,
            last.date,
            pad = pad
        ));
    }
    lines
}

#[must_use]
pub fn token_spent(tokens: SessionTokenTotals) -> u64 {
    tokens.input + tokens.output + tokens.cache_read + tokens.cache_write
}

#[must_use]
pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[must_use]
pub fn fmt_cost(cost: f64, unpriced: u64) -> String {
    if cost > 0.0 {
        format!("${cost:.2}")
    } else if unpriced > 0 {
        "no price".to_owned()
    } else {
        "—".to_owned()
    }
}

#[must_use]
pub fn sess_ago(last_ms: i64) -> String {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "—".to_owned();
    };
    let now_ms = now.as_millis() as i64;
    let delta = (now_ms - last_ms).unsigned_abs();
    let d = Duration::from_millis(delta);
    if last_ms > now_ms {
        return "just now".to_owned();
    }
    if d < Duration::from_secs(60) {
        "just now".to_owned()
    } else if d < Duration::from_secs(3600) {
        format!("{}m ago", d.as_secs() / 60)
    } else if d < Duration::from_secs(48 * 3600) {
        format!("{}h ago", d.as_secs() / 3600)
    } else if d < Duration::from_secs(60 * 24 * 3600) {
        format!("{}d ago", d.as_secs() / 3600 / 24)
    } else {
        local_date_ymd(last_ms)
    }
}

#[must_use]
fn local_date_ymd(ms: i64) -> String {
    let days = ms / 86_400_000;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Converts Unix days to a civil date (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[must_use]
pub fn folder_base(cwd: &str) -> String {
    if cwd.is_empty() {
        return "—".to_owned();
    }
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cwd)
        .to_owned()
}

#[must_use]
pub fn tilde_path(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_owned();
    };
    let home_str = home.to_string_lossy();
    if path == home_str.as_ref() {
        return "~".to_owned();
    }
    let prefix = format!("{}/", home_str);
    if let Some(rest) = path.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    path.to_owned()
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn trunc_field(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{SESS_RANGES, SessionsPage, day_chart, fmt_tokens, sess_ago, token_spent};
    use crate::pages::Page;
    use crate::theme::Theme;
    use crate::ui::buffer_text;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use hofund_control_proto::{
        SessionConsent, SessionDayStats, SessionListResponse, SessionResume, SessionStatsResponse,
        SessionSummary, SessionTokenTotals,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[test]
    fn ranges_match_contract() {
        assert_eq!(SESS_RANGES[0].0, 1);
        assert_eq!(SESS_RANGES.last().map(|r| r.0), Some(0));
    }

    #[test]
    fn newest_first_fixture_order_is_preserved() {
        let mut page = SessionsPage::new();
        let older = sample("old", 1);
        let newer = sample("new", 2);
        let mut consent = SessionConsent::default();
        consent.asked = true;
        consent.all = true;
        let mut list = SessionListResponse::default();
        list.consent = consent;
        list.sessions = vec![newer, older];
        page.set_fixture_list(list, SessionStatsResponse::default());
        assert_eq!(page.sessions()[0].id, "new");
    }

    fn day_stat(date: &str, input: u64) -> SessionDayStats {
        let mut tokens = SessionTokenTotals::default();
        tokens.input = input;
        let mut day = SessionDayStats::default();
        day.date = date.to_owned();
        day.tokens = tokens;
        day
    }

    #[test]
    fn day_chart_needs_at_least_two_days() {
        let one = vec![day_stat("2026-01-01", 100)];
        assert!(day_chart(&one, false, 80, 4).is_empty());
        let two = vec![day_stat("2026-01-01", 100), day_stat("2026-01-02", 200)];
        let chart = day_chart(&two, false, 80, 4);
        assert!(!chart.is_empty());
    }

    #[test]
    fn null_resume_row_is_not_resumable() {
        let mut page = SessionsPage::new();
        let mut consent = SessionConsent::default();
        consent.asked = true;
        let mut list = SessionListResponse::default();
        list.consent = consent;
        list.sessions = vec![sample("x", 1)];
        page.set_fixture_list(list, SessionStatsResponse::default());
        assert!(page.sessions().first().unwrap().resume.is_none());
    }

    #[test]
    fn argv_resume_is_taken() {
        let mut page = SessionsPage::new();
        let mut row = sample("x", 1);
        let mut resume = SessionResume::default();
        resume.argv = vec!["claude".to_owned(), "--resume".to_owned(), "x".to_owned()];
        resume.cwd = "/work".to_owned();
        row.resume = Some(resume);
        let mut consent = SessionConsent::default();
        consent.asked = true;
        let mut list = SessionListResponse::default();
        list.consent = consent;
        list.sessions = vec![row];
        page.set_fixture_list(list, SessionStatsResponse::default());
        assert!(page.sessions().first().unwrap().resume.is_some());
    }

    #[test]
    fn token_spent_sums_components() {
        let mut t = SessionTokenTotals::default();
        t.input = 1;
        t.output = 2;
        t.cache_read = 3;
        t.cache_write = 4;
        assert_eq!(token_spent(t), 10);
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(500), "500");
        assert_eq!(fmt_tokens(12_000), "12.0k");
    }

    #[test]
    fn sess_ago_recent_is_short() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as i64;
        assert_eq!(sess_ago(now - 30_000), "just now");
    }

    fn sample(id: &str, last_ms: i64) -> SessionSummary {
        let mut tokens = SessionTokenTotals::default();
        tokens.input = 10;
        let mut row = SessionSummary::default();
        row.id = id.to_owned();
        row.agent = "claude".to_owned();
        row.title = "title".to_owned();
        row.cwd = "/work/hofund".to_owned();
        row.last_ms = last_ms;
        row.models = vec!["m".to_owned()];
        row.tokens = tokens;
        row.cost = 0.01;
        row
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn loaded_page(count: usize, with_resume: bool) -> SessionsPage {
        let mut page = SessionsPage::new();
        let mut list = SessionListResponse::default();
        list.sessions = (0..count)
            .map(|index| {
                let mut row = sample(&format!("s{index}"), 1);
                row.title = format!("title-{index}");
                if with_resume {
                    let mut resume = SessionResume::default();
                    resume.argv = vec![
                        "agent".to_owned(),
                        "--resume".to_owned(),
                        format!("s{index}"),
                    ];
                    resume.cwd = "/work/hofund".to_owned();
                    row.resume = Some(resume);
                }
                row
            })
            .collect();
        page.set_fixture_list(list, SessionStatsResponse::default());
        page
    }

    fn render_sessions(page: &SessionsPage, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                page.draw_body(
                    Rect::new(0, 0, width, height),
                    frame.buffer_mut(),
                    &Theme::monochrome(),
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        buffer_text(buffer, width, height)
    }

    #[test]
    fn list_keeps_the_selected_row_visible_past_the_first_viewport() {
        let mut page = loaded_page(20, false);
        for _ in 0..12 {
            assert_eq!(
                page.on_key(key(KeyCode::Down)),
                super::PageKeyOutcome::CursorDown
            );
        }
        let text = render_sessions(&page, 80, 10);
        assert!(text.contains("title-12"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.contains('>') && line.contains("title-12")),
            "{text}"
        );
        assert!(!text.contains("title-0"), "{text}");
    }

    #[test]
    fn list_viewport_accounts_for_the_resume_hint() {
        let mut page = loaded_page(20, true);
        for _ in 0..12 {
            let _ = page.on_key(key(KeyCode::Down));
        }
        let text = render_sessions(&page, 80, 10);
        assert!(text.contains("title-12"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.contains('>') && line.contains("title-12")),
            "{text}"
        );
        assert!(text.contains("enter runs"), "{text}");
    }

    #[test]
    fn picker_keeps_the_selected_row_visible_past_the_first_viewport() {
        let mut page = loaded_page(1, false);
        let mut stats = SessionStatsResponse::default();
        stats.models = (0..20)
            .map(|index| {
                let mut share = hofund_control_proto::SessionStatShare::default();
                share.name = format!("model-{index}");
                share
            })
            .collect();
        let mut list = SessionListResponse::default();
        list.sessions = vec![sample("s0", 1)];
        page.set_fixture_list(list, stats);
        let _ = page.on_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));
        // items are [all models, model-0, ...]; 13 downs selects model-12.
        for _ in 0..13 {
            let _ = page.on_key(key(KeyCode::Down));
        }
        let text = render_sessions(&page, 40, 10);
        assert!(text.contains("model-12"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.contains('>') && line.contains("model-12")),
            "{text}"
        );
        assert!(!text.contains("model-0"), "{text}");
    }
}
