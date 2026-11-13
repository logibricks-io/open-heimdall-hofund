//! Accounts page: subscription sign-in, account switching, import, and removal.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent};
use hofund_control_client::hofund_control_proto::{
    DiscoveredAccount, SignInChallenge, SignInSessionState, SignInStartResponse,
    SignInStatusResponse, SubscriptionAccount, SubscriptionEntry, SubscriptionFlow,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::intent::{ControlIntent, PageEffect, Secret};
use crate::state::PageId;
use crate::theme::Theme;
use crate::ui::list_window;

/// Accounts screen and its modal state.
#[derive(Clone)]
pub struct AccountsPage {
    loaded: bool,
    loading: bool,
    load_error: Option<String>,
    rows: Vec<Row>,
    cursor: usize,
    confirm: bool,
    mode: Mode,
    discovered: Option<Vec<DiscoveredAccount>>,
    read_generation: u64,
    token: u64,
    poll_in_flight: bool,
}

#[derive(Clone)]
struct Row {
    subscription: String,
    name: String,
    flow: SubscriptionFlow,
    account: Option<SubscriptionAccount>,
}

#[derive(Clone)]
enum Mode {
    List,
    Import { buffer: String },
    SignIn(SignInDialog),
}

#[derive(Clone)]
struct SignInDialog {
    subscription: String,
    session: String,
    flow: SubscriptionFlow,
    state: SignInSessionState,
    challenge: SignInChallenge,
    proof: String,
}

/// A key's control effect and optional request to open a URL with the OS.
#[derive(Clone, Debug, Default)]
pub struct AccountsAction {
    /// Control calls, pending state, and toast.
    pub effect: PageEffect,
    /// Sensitive URL that must only be passed to the platform opener.
    pub open_url: Option<Secret>,
}

impl fmt::Debug for AccountsPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountsPage")
            .field("loaded", &self.loaded)
            .field("loading", &self.loading)
            .field("rows", &self.rows.len())
            .field("cursor", &self.cursor)
            .field("read_generation", &self.read_generation)
            .finish_non_exhaustive()
    }
}

impl Default for AccountsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountsPage {
    /// Empty page before the first control read.
    #[must_use]
    pub fn new() -> Self {
        Self {
            loaded: false,
            loading: true,
            load_error: None,
            rows: Vec::new(),
            cursor: 0,
            confirm: false,
            mode: Mode::List,
            discovered: None,
            read_generation: 0,
            token: 0,
            poll_in_flight: false,
        }
    }

    /// Whether this page owns keys that would otherwise quit or change pages.
    #[must_use]
    pub fn captures(&self) -> bool {
        !matches!(self.mode, Mode::List)
    }

    /// Whether a sign-in dialog needs one-second polling.
    #[must_use]
    pub fn sign_in_active(&self) -> bool {
        matches!(self.mode, Mode::SignIn(_))
    }

    /// Last fetch generation recorded.
    #[must_use]
    pub fn fetch_gen(&self) -> u64 {
        self.read_generation
    }

    /// Current list cursor, for focused tests.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Footer hints for the current mode.
    #[must_use]
    pub fn hint_lines(&self) -> (&'static str, &'static str) {
        match self.mode {
            Mode::List => (
                "j/k row  enter switch  n next  a sign-in  i import  f discover",
                "dd remove  x refresh  r reload  1–6/7 pages  ? help  q quit",
            ),
            Mode::Import { .. } => ("type daemon-host path  enter import  esc cancel", ""),
            Mode::SignIn(_) => (
                "o open page  enter submit hidden proof  esc cancel",
                "URLs, device codes, and tokens are never displayed",
            ),
        }
    }

    /// Applies a current-generation subscription list.
    pub fn apply_load(&mut self, subscriptions: Vec<SubscriptionEntry>) {
        let selected = self.selected().map(selection_key);
        self.rows = subscriptions
            .into_iter()
            .flat_map(|subscription| {
                let mut rows = vec![Row {
                    subscription: subscription.id.clone(),
                    name: subscription.name.clone(),
                    flow: subscription.flow,
                    account: None,
                }];
                rows.extend(subscription.accounts.into_iter().map(|account| Row {
                    subscription: subscription.id.clone(),
                    name: subscription.name.clone(),
                    flow: subscription.flow,
                    account: Some(account),
                }));
                rows
            })
            .collect();
        self.loaded = true;
        self.loading = false;
        self.load_error = None;
        self.cursor = selected
            .and_then(|(subscription, account)| {
                self.rows.iter().position(|row| {
                    row.subscription == subscription
                        && row.account.as_ref().map(|row| row.id.as_str()) == account.as_deref()
                })
            })
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// Records a list-read error without discarding the previous rows.
    pub fn apply_load_error(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Applies a successful sign-in start and opens its dialog.
    pub fn apply_start(&mut self, subscription: &str, start: SignInStartResponse) -> bool {
        if self
            .selected()
            .is_none_or(|row| row.subscription != subscription)
        {
            return false;
        }
        self.mode = Mode::SignIn(SignInDialog {
            subscription: subscription.to_owned(),
            session: start.session,
            flow: start.flow,
            state: SignInSessionState::Pending,
            challenge: start.challenge,
            proof: String::new(),
        });
        self.poll_in_flight = false;
        true
    }

    /// Queues one poll unless another poll for this dialog is already running.
    pub fn next_poll(&mut self) -> Option<ControlIntent> {
        let Mode::SignIn(dialog) = &self.mode else {
            return None;
        };
        if self.poll_in_flight
            || matches!(
                dialog.state,
                SignInSessionState::Ready
                    | SignInSessionState::Failed
                    | SignInSessionState::Cancelled
            )
        {
            return None;
        }
        self.poll_in_flight = true;
        Some(ControlIntent::PollSignIn {
            id: dialog.subscription.clone(),
            session: dialog.session.clone(),
            token: self.token,
        })
    }

    /// Whether an event belongs to the open sign-in dialog.
    #[must_use]
    pub fn matches_sign_in(&self, provider: &str, session: Option<&str>) -> bool {
        let Mode::SignIn(dialog) = &self.mode else {
            return false;
        };
        session.map_or_else(
            || dialog.subscription == provider,
            |session| dialog.session == session,
        )
    }

    /// Applies a poll response. Returns a terminal toast and whether to reload.
    pub fn apply_poll(
        &mut self,
        token: u64,
        subscription: &str,
        session: &str,
        status: SignInStatusResponse,
    ) -> Option<(String, bool, bool)> {
        if token != self.token {
            return None;
        }
        self.poll_in_flight = false;
        self.apply_status(subscription, session, status)
    }

    /// Records a failed poll for the open dialog.
    pub fn apply_poll_error(&mut self, token: u64) -> bool {
        if token != self.token {
            return false;
        }
        self.poll_in_flight = false;
        true
    }

    /// Applies a proof response while preserving the dialog on nonterminal states.
    pub fn apply_proof(
        &mut self,
        token: u64,
        subscription: &str,
        session: &str,
        status: SignInStatusResponse,
    ) -> Option<(String, bool, bool)> {
        if token != self.token {
            return None;
        }
        self.apply_status(subscription, session, status)
    }

    /// Closes a cancelled dialog if it is still the same session.
    pub fn apply_cancelled(&mut self, token: u64, subscription: &str, session: &str) -> bool {
        if token != self.token || !self.session_matches(subscription, session) {
            return false;
        }
        self.close_sign_in();
        true
    }

    /// Applies discovery results if no newer discovery or reload retired them.
    pub fn apply_discovered(&mut self, token: u64, found: Vec<DiscoveredAccount>) -> bool {
        if token != self.token {
            return false;
        }
        self.discovered = Some(found);
        true
    }

    /// Whether a discovery error still belongs to the latest discovery read.
    #[must_use]
    pub fn discovery_is_current(&self, token: u64) -> bool {
        token == self.token && self.discovered.is_none()
    }

    /// Clears a pending remove confirmation.
    pub fn disarm_confirm(&mut self) {
        self.confirm = false;
    }

    /// Handles a key and returns its control or OS side effects.
    pub fn on_accounts_key(&mut self, key: KeyEvent) -> AccountsAction {
        match &self.mode {
            Mode::List => self.on_list(key),
            Mode::Import { .. } => self.on_import(key),
            Mode::SignIn(_) => self.on_sign_in(key),
        }
    }

    /// Draws the account list or active dialog.
    pub fn draw(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let lines = match &self.mode {
            Mode::List => self.list_lines(area.height, theme),
            Mode::Import { buffer } => vec![
                title("import account", theme),
                Line::from(""),
                Line::from(format!("  daemon-host path: {buffer}")),
                Line::from(Span::styled(
                    "  The file is read by the daemon, not by this terminal.",
                    theme.muted_style(),
                )),
            ],
            Mode::SignIn(dialog) => sign_in_lines(dialog),
        };
        Paragraph::new(lines).render(area, buf);
    }

    fn on_list(&mut self, key: KeyEvent) -> AccountsAction {
        if key.code != KeyCode::Char('d') {
            self.confirm = false;
        }
        let effect = match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut self.cursor, self.rows.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut self.cursor, self.rows.len(), -1);
                PageEffect::none()
            }
            KeyCode::Char('a') => self.with_subscription(|id| {
                PageEffect::mutation(
                    "starting sign-in",
                    ControlIntent::StartSignIn { id, generation: 0 },
                )
            }),
            KeyCode::Enter => self.switch_selected(),
            KeyCode::Char('n') => self.with_subscription(|agent| {
                PageEffect::mutation(
                    "switching account",
                    ControlIntent::SwitchAccount {
                        agent,
                        target: "next".to_owned(),
                        generation: 0,
                    },
                )
            }),
            KeyCode::Char('i') => {
                self.mode = Mode::Import {
                    buffer: String::new(),
                };
                PageEffect::none()
            }
            KeyCode::Char('f') => self.discover(),
            KeyCode::Char('d') => self.remove(),
            KeyCode::Char('x') => self.refresh(),
            _ => PageEffect::none(),
        };
        AccountsAction {
            effect,
            open_url: None,
        }
    }

    fn on_import(&mut self, key: KeyEvent) -> AccountsAction {
        let effect = match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                PageEffect::none()
            }
            KeyCode::Backspace => {
                if let Mode::Import { buffer } = &mut self.mode {
                    buffer.pop();
                }
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Mode::Import { buffer } = &mut self.mode else {
                    return AccountsAction::default();
                };
                let path = buffer.trim().to_owned();
                if path.is_empty() {
                    PageEffect::flash("enter an export path", false)
                } else {
                    self.mode = Mode::List;
                    PageEffect::mutation(
                        "importing account",
                        ControlIntent::ImportAccount {
                            path,
                            generation: 0,
                        },
                    )
                }
            }
            KeyCode::Char(character) if !character.is_control() => {
                if let Mode::Import { buffer } = &mut self.mode {
                    buffer.push(character);
                }
                PageEffect::none()
            }
            _ => PageEffect::none(),
        };
        AccountsAction {
            effect,
            open_url: None,
        }
    }

    fn on_sign_in(&mut self, key: KeyEvent) -> AccountsAction {
        let Mode::SignIn(dialog) = &self.mode else {
            return AccountsAction::default();
        };
        if key.code == KeyCode::Esc {
            return AccountsAction {
                effect: PageEffect::mutation(
                    "cancelling sign-in",
                    ControlIntent::CancelSignIn {
                        id: dialog.subscription.clone(),
                        session: dialog.session.clone(),
                        token: self.token,
                        generation: 0,
                    },
                ),
                open_url: None,
            };
        }
        if key.code == KeyCode::Char('o')
            && dialog.proof.is_empty()
            && matches!(
                dialog.challenge,
                SignInChallenge::OpenUrl { .. } | SignInChallenge::DeviceCode { .. }
            )
        {
            let url = match &dialog.challenge {
                SignInChallenge::OpenUrl { url } => Some(url.clone()),
                SignInChallenge::DeviceCode {
                    verification_url, ..
                } => Some(verification_url.clone()),
                SignInChallenge::Instructions { .. } => None,
                _ => None,
            };
            return AccountsAction {
                effect: if url.is_some() {
                    PageEffect::none()
                } else {
                    PageEffect::flash("this sign-in has no page to open", false)
                },
                open_url: url.map(Secret::new),
            };
        }
        let effect = match key.code {
            KeyCode::Enter => self.submit_proof(),
            KeyCode::Backspace => {
                if let Mode::SignIn(dialog) = &mut self.mode {
                    dialog.proof.pop();
                }
                PageEffect::none()
            }
            KeyCode::Char(character) if !character.is_control() => {
                if let Mode::SignIn(dialog) = &mut self.mode
                    && dialog.flow != SubscriptionFlow::Device
                {
                    dialog.proof.push(character);
                }
                PageEffect::none()
            }
            _ => PageEffect::none(),
        };
        AccountsAction {
            effect,
            open_url: None,
        }
    }

    fn submit_proof(&mut self) -> PageEffect {
        let Mode::SignIn(dialog) = &mut self.mode else {
            return PageEffect::none();
        };
        if dialog.flow == SubscriptionFlow::Device {
            return PageEffect::flash("device sign-in completes by polling", false);
        }
        if dialog.state != SignInSessionState::NeedsProof {
            return PageEffect::flash("sign-in is not ready for pasted proof", false);
        }
        let proof = dialog.proof.trim().to_owned();
        if proof.is_empty() {
            return PageEffect::flash("paste a callback URL or token text", false);
        }
        let url = matches!(dialog.challenge, SignInChallenge::OpenUrl { .. });
        dialog.proof.clear();
        if looks_like_path(&proof) {
            return PageEffect::flash(
                "use i to import a file; paste a callback URL or token text here",
                false,
            );
        }
        PageEffect::mutation(
            "submitting sign-in proof",
            ControlIntent::SubmitSignInProof {
                id: dialog.subscription.clone(),
                session: dialog.session.clone(),
                token: self.token,
                proof: Secret::new(proof),
                url,
                generation: 0,
            },
        )
    }

    fn switch_selected(&self) -> PageEffect {
        let Some(row) = self.selected() else {
            return PageEffect::flash("no account selected", false);
        };
        let Some(account) = &row.account else {
            return PageEffect::flash("select an account to switch", false);
        };
        PageEffect::mutation(
            "switching account",
            ControlIntent::SwitchAccount {
                agent: row.subscription.clone(),
                target: account.id.clone(),
                generation: 0,
            },
        )
    }

    fn discover(&mut self) -> PageEffect {
        if self.discovered.is_some() {
            return PageEffect::mutation(
                "importing discovered accounts",
                ControlIntent::ImportDiscoveredAccounts { generation: 0 },
            );
        }
        let token = self.next_token();
        PageEffect::read(ControlIntent::DiscoverAccounts { token })
    }

    fn remove(&mut self) -> PageEffect {
        let Some(row) = self.selected() else {
            return PageEffect::flash("no account selected", false);
        };
        let id = row.subscription.clone();
        let Some(account) = row.account.as_ref().map(|account| account.id.clone()) else {
            self.confirm = false;
            return PageEffect::flash("select an account to remove", false);
        };
        if !self.confirm {
            self.confirm = true;
            return PageEffect::flash("press d again to remove this account", false);
        }
        self.confirm = false;
        PageEffect::mutation(
            "removing account",
            ControlIntent::RemoveAccount {
                id,
                account,
                generation: 0,
            },
        )
    }

    fn refresh(&self) -> PageEffect {
        let Some(row) = self.selected() else {
            return PageEffect::flash("no account selected", false);
        };
        PageEffect::mutation(
            "refreshing account",
            ControlIntent::RefreshSubscription {
                id: row.subscription.clone(),
                account: row.account.as_ref().map(|account| account.id.clone()),
                generation: 0,
            },
        )
    }

    fn with_subscription(&self, make: impl FnOnce(String) -> PageEffect) -> PageEffect {
        self.selected().map_or_else(
            || PageEffect::flash("no subscription selected", false),
            |row| make(row.subscription.clone()),
        )
    }

    fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    fn next_token(&mut self) -> u64 {
        self.token = self.token.saturating_add(1);
        self.token
    }

    fn session_matches(&self, subscription: &str, session: &str) -> bool {
        matches!(&self.mode, Mode::SignIn(dialog) if dialog.subscription == subscription && dialog.session == session)
    }

    fn apply_status(
        &mut self,
        subscription: &str,
        session: &str,
        status: SignInStatusResponse,
    ) -> Option<(String, bool, bool)> {
        if !self.session_matches(subscription, session) {
            return None;
        }
        let terminal = match status.state {
            SignInSessionState::Ready => Some(("sign-in complete".to_owned(), true, true)),
            SignInSessionState::Failed => Some((
                "sign-in failed or expired; start again".to_owned(),
                false,
                false,
            )),
            SignInSessionState::Cancelled => Some(("sign-in cancelled".to_owned(), false, false)),
            SignInSessionState::Pending | SignInSessionState::NeedsProof => None,
            _ => Some(("sign-in ended in an unknown state".to_owned(), false, false)),
        };
        if terminal.is_some() {
            self.close_sign_in();
            return terminal;
        }
        if let Mode::SignIn(dialog) = &mut self.mode {
            dialog.state = status.state;
            if let Some(challenge) = status.challenge {
                dialog.challenge = challenge;
            }
        }
        None
    }

    fn close_sign_in(&mut self) {
        self.mode = Mode::List;
        self.poll_in_flight = false;
        self.next_token();
    }

    fn list_lines(&self, height: u16, theme: &Theme) -> Vec<Line<'static>> {
        let discovery_rows = self.discovered.as_ref().map_or(0, |found| found.len() + 2);
        let window = list_window(
            height.saturating_sub(discovery_rows as u16),
            3,
            self.rows.len(),
            self.cursor,
        );
        let mut lines = Vec::new();
        if self.rows.is_empty() {
            let message = if self.loading {
                "Loading accounts…".to_owned()
            } else if let Some(error) = &self.load_error {
                format!("Could not load accounts: {error}")
            } else {
                "No subscriptions configured.".to_owned()
            };
            lines.push(Line::from(Span::styled(
                format!("  {message}"),
                theme.muted_style(),
            )));
        } else {
            if let Some(error) = &self.load_error {
                lines.push(Line::from(Span::styled(
                    format!("  Could not reload accounts: {error}"),
                    theme.muted_style(),
                )));
            }
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
                    Span::styled(row_text(row), style),
                ]));
            }
        }
        if let Some(found) = &self.discovered {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Discovered sign-ins (press f again to import new accounts):",
                theme.title_style(),
            )));
            if found.is_empty() {
                lines.push(Line::from("    none found"));
            } else {
                lines.extend(found.iter().map(|account| {
                    let stored = if account.already_stored {
                        " (already stored)"
                    } else {
                        ""
                    };
                    Line::from(format!(
                        "    {}: {}{}",
                        account.subscription, account.label, stored
                    ))
                }));
            }
        }
        lines
    }
}

impl Page for AccountsPage {
    fn id(&self) -> PageId {
        PageId::Accounts
    }

    fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        self.loading = true;
        self.load_error = None;
        self.discovered = None;
        if !self.sign_in_active() {
            self.next_token();
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> PageKeyOutcome {
        let _ = self.on_accounts_key(key);
        PageKeyOutcome::None
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.draw(area, buf, theme);
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

fn selection_key(row: &Row) -> (String, Option<String>) {
    (
        row.subscription.clone(),
        row.account.as_ref().map(|account| account.id.clone()),
    )
}

fn row_text(row: &Row) -> String {
    let Some(account) = &row.account else {
        return format!(
            "{} ({}) · {}",
            row.name,
            row.subscription,
            flow_name(row.flow)
        );
    };
    let status = if account.ready {
        "ready".to_owned()
    } else if let Some(reason) = &account.needs_sign_in {
        format!("sign-in needed: {reason}")
    } else {
        "not ready".to_owned()
    };
    format!("  {} ({}) · {}", account.label, account.id, status)
}

fn flow_name(flow: SubscriptionFlow) -> &'static str {
    match flow {
        SubscriptionFlow::Pkce => "PKCE",
        SubscriptionFlow::Device => "device",
        SubscriptionFlow::TokenFile => "token",
        SubscriptionFlow::Bridge => "bridge",
        _ => "unknown flow",
    }
}

fn sign_in_lines(dialog: &SignInDialog) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("  Sign in: {}", dialog.subscription)),
        Line::from(format!("  State: {}", state_name(dialog.state))),
        Line::from(""),
    ];
    match &dialog.challenge {
        SignInChallenge::OpenUrl { .. } => {
            lines.push(Line::from(
                "  Authorization page ready. Press o to ask the OS to open it.",
            ));
        }
        SignInChallenge::DeviceCode { .. } => {
            lines.push(Line::from(
                "  Device verification pending. Press o to open the verification page.",
            ));
            lines.push(Line::from(
                "  Polling completes this flow; no pasted proof is used.",
            ));
        }
        SignInChallenge::Instructions { .. } => {
            lines.push(Line::from(
                "  Follow the provider instructions and paste token text when requested.",
            ));
        }
        _ => {
            lines.push(Line::from("  Waiting for a supported sign-in challenge."));
        }
    }
    if dialog.flow != SubscriptionFlow::Device {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "  Hidden proof: {}",
            "*".repeat(dialog.proof.chars().count())
        )));
        lines.push(Line::from(
            "  Paste a callback URL or token text, then press enter when proof is requested.",
        ));
    }
    lines
}

fn state_name(state: SignInSessionState) -> &'static str {
    match state {
        SignInSessionState::Pending => "pending",
        SignInSessionState::NeedsProof => "proof requested",
        SignInSessionState::Ready => "ready",
        SignInSessionState::Failed => "failed or expired",
        SignInSessionState::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn title(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), theme.title_style()))
}

fn looks_like_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("~/")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("file://")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn move_cursor(cursor: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    *cursor = (*cursor as i32 + delta).rem_euclid(len as i32) as usize;
}
