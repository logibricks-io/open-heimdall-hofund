//! Usage page: quota cards, period totals, agent breakdown, check-in, and alerts.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    Quota, QuotaAlertsResponse, QuotaResponse, QuotaSpan, SettingsResponse, UsageBreakdownBy,
    UsageBreakdownGroup, UsagePeriod, UsageQuery, UsageResponse, UsageTotals,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::intent::ControlIntent;
use crate::state::PageId;
use crate::theme::Theme;

const PERIODS: &[(UsagePeriod, &str)] = &[
    (UsagePeriod::Day, "today"),
    (UsagePeriod::Week, "7 days"),
    (UsagePeriod::Month, "30 days"),
    (UsagePeriod::All, "all time"),
];
const QUOTA_OPEN_GAP: Duration = Duration::from_secs(30);
const QUOTA_TRIES: u8 = 20;
const REVISION_LEN: usize = 64;

/// An action the application runs for the Usage page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UsageAction {
    /// Toggle and persist used versus remaining quota percentages.
    ToggleQuotaLeft,
    /// Run every listed daily check-in.
    CheckIn,
    /// Save a usage-alert percentage; zero turns it off.
    SetUsageAlert(u8),
    /// Save a balance-alert amount; zero turns it off.
    SetBalanceAlert(f64),
}

/// Usage tab state.
#[derive(Clone, Debug)]
pub struct UsagePage {
    read_generation: u64,
    period_index: usize,
    usage: UsageResponse,
    usage_loading: bool,
    usage_error: Option<String>,
    quotas: Option<Vec<Quota>>,
    quota_error: Option<String>,
    quota_token: u64,
    quota_tries: u8,
    quota_asked_at: Option<Instant>,
    settings: Option<SettingsResponse>,
    alerts: Option<QuotaAlertsResponse>,
    alerts_error: Option<String>,
    alert_edit: Option<AlertEdit>,
}

#[derive(Clone, Debug)]
struct AlertEdit {
    balance: bool,
    buffer: String,
    error: Option<String>,
}

impl Default for UsagePage {
    fn default() -> Self {
        Self::new()
    }
}

impl UsagePage {
    /// Empty page before the first fetch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            read_generation: 0,
            period_index: 0,
            usage: UsageResponse::default(),
            usage_loading: true,
            usage_error: None,
            quotas: None,
            quota_error: None,
            quota_token: 0,
            quota_tries: 0,
            quota_asked_at: None,
            settings: None,
            alerts: None,
            alerts_error: None,
            alert_edit: None,
        }
    }

    /// Query for whole-period totals and the agent breakdown.
    #[must_use]
    pub fn usage_query(&self) -> UsageQuery {
        UsageQuery::with_breakdown(self.period(), UsageBreakdownBy::Agent)
    }

    /// Active reporting period.
    #[must_use]
    pub fn period(&self) -> UsagePeriod {
        PERIODS[self.period_index].0
    }

    /// Whether quota meters show remaining rather than used percentages.
    #[must_use]
    pub fn quota_left(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| settings.quota_left)
    }

    /// Current quota cards from `QuotaResponse.data`.
    #[must_use]
    pub fn quotas(&self) -> Option<&[Quota]> {
        self.quotas.as_deref()
    }

    /// Number of deferred-read retries started for the current quota load.
    #[must_use]
    pub fn quota_tries(&self) -> u8 {
        self.quota_tries
    }

    /// Whether an alert editor owns page keys.
    #[must_use]
    pub fn captures(&self) -> bool {
        self.alert_edit.is_some()
    }

    /// Footer hints for the current mode.
    #[must_use]
    pub fn hint_lines(&self) -> (&'static str, &'static str) {
        if self.captures() {
            ("enter save alert   esc cancel", "0 turns the alert off")
        } else {
            (
                "←→ period   t/w/m/A jump   u used/left   c check-in   r reload",
                "a usage alert   b balance alert   1–6 pages   ? help   q quit",
            )
        }
    }

    /// Starts a page-open quota read, fresh when the last ask is at least 30s old.
    pub fn open_quotas(&mut self, now: Instant) -> ControlIntent {
        self.begin_quota_read(now, false)
    }

    /// Drops cards and starts a fresh quota read.
    pub fn refresh_quotas(&mut self, now: Instant) -> ControlIntent {
        self.quotas = None;
        self.quota_error = None;
        self.begin_quota_read(now, true)
    }

    fn begin_quota_read(&mut self, now: Instant, force: bool) -> ControlIntent {
        let fresh = force
            || self.quota_asked_at.is_none_or(|asked| {
                now.checked_duration_since(asked).unwrap_or_default() >= QUOTA_OPEN_GAP
            });
        if fresh {
            self.quota_asked_at = Some(now);
        }
        self.quota_token = self.quota_token.wrapping_add(1);
        self.quota_tries = 0;
        self.quota_error = None;
        ControlIntent::LoadQuotas {
            token: self.quota_token,
            kept: !fresh,
            delay: false,
        }
    }

    /// Applies a successful usage read.
    pub fn apply_usage(&mut self, response: UsageResponse) {
        self.usage_loading = false;
        self.usage_error = None;
        self.usage = response;
    }

    /// Records a usage read failure.
    pub fn apply_usage_error(&mut self, message: String) {
        self.usage_loading = false;
        self.usage_error = Some(message);
    }

    /// Applies G-5 settings when the route is available.
    pub fn apply_settings(&mut self, settings: SettingsResponse) {
        self.settings = Some(settings);
    }

    /// Applies quota alert thresholds.
    pub fn apply_alerts(&mut self, alerts: QuotaAlertsResponse) {
        self.alerts_error = None;
        self.alerts = Some(alerts);
    }

    /// Records an alert read failure.
    pub fn apply_alerts_error(&mut self, message: String) {
        self.alerts_error = Some(message);
    }

    /// Applies quota cards and starts a kept retry while any card is deferred.
    pub fn apply_quotas(
        &mut self,
        token: u64,
        response: QuotaResponse,
        page_open: bool,
    ) -> Option<ControlIntent> {
        if token != self.quota_token {
            return None;
        }
        let reading = response.data.iter().any(|quota| quota.read_deferred);
        self.quotas = Some(response.data);
        self.quota_error = None;
        if reading && page_open && self.quota_tries < QUOTA_TRIES {
            self.quota_tries += 1;
            return Some(ControlIntent::LoadQuotas {
                token,
                kept: true,
                delay: true,
            });
        }
        None
    }

    /// Records a quota read failure if it belongs to the current load.
    pub fn apply_quotas_error(&mut self, token: u64, message: String) {
        if token == self.quota_token {
            self.quota_error = Some(message);
        }
    }

    /// Toggles the display and returns the G-5 write when settings are loaded.
    pub fn toggle_quota_left(&mut self) -> Option<ControlIntent> {
        let previous = self.quota_left();
        let value = !previous;
        if let Some(settings) = &mut self.settings {
            settings.quota_left = value;
        }
        let settings = self.settings.as_ref()?;
        (settings.disk_revision.len() == REVISION_LEN).then(|| ControlIntent::SetQuotaLeft {
            revision: settings.disk_revision.clone(),
            value,
            previous,
        })
    }

    /// Restores the previous display after a failed settings write.
    pub fn rollback_quota_left(&mut self, previous: bool) {
        if let Some(settings) = &mut self.settings {
            settings.quota_left = previous;
        }
    }

    fn open_alert_edit(&mut self, balance: bool) {
        let buffer = self.alerts.as_ref().map_or_else(String::new, |alerts| {
            if balance {
                trim_amount(alerts.balance_alert)
            } else {
                alerts.usage_alert.to_string()
            }
        });
        self.alert_edit = Some(AlertEdit {
            balance,
            buffer,
            error: None,
        });
    }

    fn on_alert_edit_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        let Some(edit) = &mut self.alert_edit else {
            return PageKeyOutcome::None;
        };
        match key.code {
            KeyCode::Esc => self.alert_edit = None,
            KeyCode::Backspace => {
                edit.buffer.pop();
                edit.error = None;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                edit.buffer.push(c);
                edit.error = None;
            }
            KeyCode::Enter => {
                let text = edit.buffer.trim();
                if edit.balance {
                    match text.parse::<f64>() {
                        Ok(value) if value.is_finite() && value >= 0.0 => {
                            self.alert_edit = None;
                            return PageKeyOutcome::Usage(UsageAction::SetBalanceAlert(value));
                        }
                        _ => edit.error = Some("enter a non-negative amount, or 0".to_owned()),
                    }
                } else {
                    match text.parse::<u8>() {
                        Ok(value) if value <= 100 => {
                            self.alert_edit = None;
                            return PageKeyOutcome::Usage(UsageAction::SetUsageAlert(value));
                        }
                        _ => edit.error = Some("enter a percent from 0 to 100".to_owned()),
                    }
                }
            }
            _ => {}
        }
        PageKeyOutcome::None
    }

    fn set_period(&mut self, index: usize) -> PageKeyOutcome {
        self.period_index = index;
        PageKeyOutcome::Reload
    }
}

impl Page for UsagePage {
    fn id(&self) -> PageId {
        PageId::Usage
    }

    fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        self.usage_loading = true;
        self.usage_error = None;
        self.alerts_error = None;
    }

    fn on_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        if self.captures() {
            return self.on_alert_edit_key(key);
        }
        match key.code {
            KeyCode::Char('l') | KeyCode::Right => {
                self.set_period((self.period_index + 1) % PERIODS.len())
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.set_period((self.period_index + PERIODS.len() - 1) % PERIODS.len())
            }
            KeyCode::Char('t') => self.set_period(0),
            KeyCode::Char('w') => self.set_period(1),
            KeyCode::Char('m') => self.set_period(2),
            KeyCode::Char('A') => self.set_period(PERIODS.len() - 1),
            KeyCode::Char('u') => PageKeyOutcome::Usage(UsageAction::ToggleQuotaLeft),
            KeyCode::Char('c') => PageKeyOutcome::Usage(UsageAction::CheckIn),
            KeyCode::Char('a') => {
                self.open_alert_edit(false);
                PageKeyOutcome::None
            }
            KeyCode::Char('b') => {
                self.open_alert_edit(true);
                PageKeyOutcome::None
            }
            _ => PageKeyOutcome::None,
        }
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if let Some(edit) = &self.alert_edit {
            draw_alert_edit(edit, area, buf, theme);
            return;
        }
        let mut lines = vec![period_bar(self.period_index, theme), Line::from("")];
        lines.extend(quota_lines(self, theme));
        if self.quotas.is_some() || self.quota_error.is_some() {
            lines.push(Line::from(""));
        }
        lines.push(alert_line(self, theme));
        lines.push(Line::from(""));
        lines.extend(usage_lines(self, theme));
        Paragraph::new(lines).render(area, buf);
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

fn period_bar(selected: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme.muted_style())];
    for (index, (_, name)) in PERIODS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if index == selected {
            theme.pill_style()
        } else {
            theme.muted_style()
        };
        spans.push(Span::styled(format!(" {name} "), style));
    }
    Line::from(spans)
}

fn quota_lines(page: &UsagePage, theme: &Theme) -> Vec<Line<'static>> {
    let heading = if page.quota_left() {
        "  accounts · % is how much is left"
    } else {
        "  accounts · % is how much is used"
    };
    let mut lines = vec![Line::from(Span::styled(heading, theme.muted_style()))];
    if let Some(error) = &page.quota_error {
        lines.push(Line::from(Span::styled(
            format!("  could not read quotas: {error}"),
            theme.muted_style(),
        )));
        return lines;
    }
    let Some(quotas) = &page.quotas else {
        lines.push(Line::from(Span::styled(
            "  asking the vendors what is left…",
            theme.muted_style(),
        )));
        return lines;
    };
    for quota in quotas {
        let mut title = quota.name.clone();
        if title.is_empty() {
            title.clone_from(&quota.provider);
        }
        if !quota.plan.is_empty() {
            title.push_str(&format!(" · {}", quota.plan));
        }
        let account = if quota.alias.is_empty() {
            &quota.user
        } else {
            &quota.alias
        };
        if !account.is_empty() {
            title.push_str(&format!(" · {account}"));
        }
        lines.push(Line::from(Span::styled(
            format!("  {title}"),
            theme.text_style(),
        )));
        if !quota.error.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}", quota.error),
                theme.muted_style(),
            )));
            continue;
        }
        if quota.windows.is_empty() {
            let text = if quota.balance.is_empty() {
                "no usage reported".to_owned()
            } else {
                format!("{} left", quota.balance)
            };
            lines.push(Line::from(Span::styled(
                format!("    {text}"),
                theme.muted_style(),
            )));
        }
        for span in &quota.windows {
            lines.push(Line::from(Span::styled(
                format!("    {}", quota_span(span, page.quota_left())),
                theme.text_style(),
            )));
        }
        if !quota.balance.is_empty() && !quota.windows.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {} left", quota.balance),
                theme.text_style(),
            )));
        }
        if quota.read_deferred {
            lines.push(Line::from(Span::styled(
                "    reading…",
                theme.muted_style(),
            )));
        }
    }
    lines
}

fn quota_span(span: &QuotaSpan, left: bool) -> String {
    if span.unlimited {
        return format!("{} unlimited", span.name);
    }
    if span.text {
        return format!("{} {}", span.name, span.display);
    }
    let (percent, word) = if left {
        (span.remaining, "left")
    } else {
        (span.used, "used")
    };
    let percent = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((percent * 8.0 / 100.0).round() as usize).clamp(0, 8);
    let meter = format!("{}{}", "█".repeat(filled), "░".repeat(8 - filled));
    let count = if span.display.is_empty() {
        String::new()
    } else {
        format!("{} · ", span.display)
    };
    let reset = span
        .resets_at
        .as_ref()
        .map_or_else(String::new, |at| format!(" · resets {}", at.as_str()));
    format!("{} {meter} {count}{percent:.0}% {word}{reset}", span.name)
}

fn alert_line(page: &UsagePage, theme: &Theme) -> Line<'static> {
    let text = if let Some(error) = &page.alerts_error {
        format!("  alerts · could not load: {error}")
    } else if let Some(alerts) = &page.alerts {
        let usage = if alerts.usage_alert == 0 {
            "off".to_owned()
        } else {
            format!("{}% used", alerts.usage_alert)
        };
        let balance = if alerts.balance_alert == 0.0 {
            "off".to_owned()
        } else {
            trim_amount(alerts.balance_alert)
        };
        format!("  alerts · usage {usage} · balance {balance}")
    } else {
        "  alerts · loading…".to_owned()
    };
    Line::from(Span::styled(text, theme.muted_style()))
}

fn usage_lines(page: &UsagePage, theme: &Theme) -> Vec<Line<'static>> {
    if page.usage_loading && page.usage.totals.requests == 0 {
        return vec![Line::from(Span::styled(
            "  loading gateway usage…",
            theme.muted_style(),
        ))];
    }
    if let Some(error) = &page.usage_error {
        return vec![Line::from(Span::styled(
            format!("  could not load gateway usage: {error}"),
            theme.muted_style(),
        ))];
    }
    if page.usage.totals.requests == 0 && page.usage.breakdown.is_empty() {
        return vec![Line::from(Span::styled(
            "  no calls through the gateway in this time",
            theme.muted_style(),
        ))];
    }
    let totals = &page.usage.totals;
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "  {} tokens · {} requests · {}",
                format_tokens(total_tokens(totals)),
                totals.requests,
                format_cost(totals.cost, totals.unpriced, &page.usage.currency)
            ),
            theme.text_style(),
        )),
        Line::from(Span::styled(
            format!(
                "  in {}  out {}  cache read {}  cache write {}  reasoning {}",
                format_tokens(totals.input_tokens),
                format_tokens(totals.output_tokens),
                format_tokens(totals.cache_read_tokens),
                format_tokens(totals.cache_write_5m_tokens + totals.cache_write_1h_tokens),
                format_tokens(totals.reasoning_tokens)
            ),
            theme.muted_style(),
        )),
    ];
    if let Some(speed) = speed_text(totals.mean_ttft_ms, totals.mean_tokens_per_second) {
        lines.push(Line::from(Span::styled(
            format!("  {speed}"),
            theme.muted_style(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  agents", theme.muted_style())));
    lines.push(Line::from(Span::styled(
        "  agent                  requests  input   output  ttft ms  tokens/s  cost",
        theme.muted_style(),
    )));
    for group in &page.usage.breakdown {
        lines.push(agent_row(group, &page.usage.currency, theme));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  models", theme.muted_style())));
    lines
}

fn agent_row(group: &UsageBreakdownGroup, currency: &str, theme: &Theme) -> Line<'static> {
    let name = if group.label.is_empty() {
        group.id.as_str()
    } else {
        group.label.as_str()
    };
    let ttft = group
        .mean_ttft_ms
        .map(|value| format!("{value:.0}"))
        .unwrap_or_default();
    let speed = group
        .mean_tokens_per_second
        .map(|value| format!("{value:.1}"))
        .unwrap_or_default();
    Line::from(Span::styled(
        format!(
            "  {:<22}  {:>8}  {:>7}  {:>7}  {:>7}  {:>8}  {}",
            truncate(name, 22),
            group.requests,
            format_tokens(group.input_tokens),
            format_tokens(group.output_tokens),
            ttft,
            speed,
            format_cost(group.cost, group.unpriced, currency)
        ),
        theme.text_style(),
    ))
}

fn speed_text(ttft: Option<f64>, tokens_per_second: Option<f64>) -> Option<String> {
    let mut fields = Vec::new();
    if let Some(value) = ttft {
        fields.push(format!("mean ttft {value:.0} ms"));
    }
    if let Some(value) = tokens_per_second {
        fields.push(format!("mean output {value:.1} tokens/s"));
    }
    (!fields.is_empty()).then(|| fields.join(" · "))
}

fn total_tokens(totals: &UsageTotals) -> u64 {
    totals
        .input_tokens
        .saturating_add(totals.output_tokens)
        .saturating_add(totals.cache_read_tokens)
        .saturating_add(totals.cache_write_5m_tokens)
        .saturating_add(totals.cache_write_1h_tokens)
        .saturating_add(totals.reasoning_tokens)
}

/// Formats a cost already denominated in the usage response currency.
#[must_use]
pub fn format_cost(cost: f64, unpriced: u64, currency: &str) -> String {
    if cost == 0.0 && unpriced > 0 {
        return "no price".to_owned();
    }
    let mut text = if currency.eq_ignore_ascii_case("CNY") {
        format!("¥{cost:.2}")
    } else {
        format!("${cost:.2}")
    };
    if unpriced > 0 {
        text.push('+');
    }
    text
}

#[must_use]
fn format_tokens(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if value >= 10_000_000 {
        format!("{:.0}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 100_000 {
        format!("{:.0}K", value as f64 / 1_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn trim_amount(value: f64) -> String {
    value.to_string()
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        let mut out: String = value.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn draw_alert_edit(edit: &AlertEdit, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let title = if edit.balance {
        "usage · balance alert"
    } else {
        "usage · usage alert"
    };
    let hint = if edit.balance {
        "balance amount, or 0 to turn off"
    } else {
        "percent used, or 0 to turn off"
    };
    let mut lines = vec![
        Line::from(Span::styled(format!("  {title}"), theme.title_style())),
        Line::from(""),
        Line::from(Span::styled(
            format!("  > {}", edit.buffer),
            theme.cursor_style(),
        )),
        Line::from(Span::styled(format!("  {hint}"), theme.muted_style())),
    ];
    if let Some(error) = &edit.error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            theme.muted_style(),
        )));
    }
    Paragraph::new(lines).render(area, buf);
}
