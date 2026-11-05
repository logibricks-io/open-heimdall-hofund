//! Application state and key handling.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::pages::{Page, Pages};
use crate::state::{FLASH_TTL, Flash, GatewayPresence, Generation, Mode, PageId, PendingOp};

/// Main TUI model (testable without the terminal loop).
pub struct App {
    /// Active page.
    pub page: PageId,
    /// Interaction mode.
    pub mode: Mode,
    /// Terminal width.
    pub width: u16,
    /// Terminal height.
    pub height: u16,
    /// Optional status flash.
    pub flash: Option<Flash>,
    /// Gateway handoff state line.
    pub gateway: GatewayPresence,
    /// Last known disk revision from events.
    pub disk_revision: String,
    /// In-flight requests from snapshot events.
    pub in_flight: u64,
    /// Listener generation from snapshot.
    pub listener_generation: u64,
    /// Optional mutating operation.
    pub pending: Option<PendingOp>,
    /// Catalog sync indicator (Agents `S` in R-14).
    pub catalog_syncing: bool,
    /// Page states.
    pub pages: Pages,
    /// Monotonic generation for in-flight reads.
    generation: Generation,
    /// Whether the account dialog needs a poll (R-15).
    sign_in_dirty: bool,
    /// Token file must be reread (token_rotated).
    token_reread: bool,
    /// Event stream must reopen without cursor.
    event_resync: bool,
    /// Test hook: reload count.
    #[cfg(test)]
    pub reloads: u32,
}

impl App {
    /// Builds the initial app state.
    #[must_use]
    pub fn new(page: PageId) -> Self {
        Self::for_test(page)
    }

    /// Builds an app for unit tests.
    #[must_use]
    pub fn for_test(page: PageId) -> Self {
        Self {
            page,
            mode: Mode::List,
            width: 100,
            height: 30,
            flash: None,
            gateway: GatewayPresence::Attached {
                line: "attached to daemon".to_owned(),
            },
            disk_revision: String::new(),
            in_flight: 0,
            listener_generation: 0,
            pending: None,
            catalog_syncing: false,
            pages: Pages::default(),
            generation: 1,
            sign_in_dirty: false,
            token_reread: false,
            event_resync: false,
            #[cfg(test)]
            reloads: 0,
        }
    }

    /// Records terminal size after resize.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Sets a flash, replacing any previous one.
    pub fn set_flash(&mut self, flash: Flash) {
        self.flash = Some(flash);
    }

    /// Marks sign-in dialog dirty.
    pub fn mark_sign_in_dirty(&mut self) {
        self.sign_in_dirty = true;
    }

    /// Whether sign-in needs polling.
    #[must_use]
    pub fn sign_in_dirty(&self) -> bool {
        self.sign_in_dirty
    }

    /// Requests bearer token reread after rotation.
    pub fn request_token_reread(&mut self) {
        self.token_reread = true;
    }

    /// Takes the token reread flag.
    #[must_use]
    pub fn take_token_reread(&mut self) -> bool {
        std::mem::take(&mut self.token_reread)
    }

    /// Requests event stream resync without cursor.
    pub fn request_event_resync(&mut self) {
        self.event_resync = true;
    }

    /// Takes the resync flag.
    #[must_use]
    pub fn take_event_resync(&mut self) -> bool {
        std::mem::take(&mut self.event_resync)
    }

    /// Schedules reload of the open page.
    pub fn reload_open_page(&mut self) {
        self.bump_generation();
        let read_gen = self.generation;
        self.pages.mut_page(self.page).reload(read_gen);
        #[cfg(test)]
        {
            self.reloads += 1;
        }
    }

    /// Returns the next read generation without consuming.
    #[must_use]
    pub fn next_generation(&self) -> Generation {
        self.generation + 1
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Current read generation (tests).
    #[cfg(test)]
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Applies a worker response if `gen` is still current.
    pub fn apply_read_result(&mut self, read_gen: Generation, apply: impl FnOnce(&mut Self)) {
        if read_gen == self.generation {
            apply(self);
        }
    }

    /// Navigates to `page`, clearing confirm state and flash per spec.
    pub fn go_to(&mut self, page: PageId) {
        self.flash = None;
        self.page = page;
        self.mode = Mode::List;
        self.reload_open_page();
    }

    /// Handles a terminal key when not in a pending mutation (except quit).
    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }
        if self.pending.is_some() {
            return AppAction::None;
        }
        match self.mode {
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc) {
                    self.mode = Mode::List;
                }
                AppAction::None
            }
            Mode::List => self.handle_list_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return AppAction::Quit,
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return AppAction::None;
            }
            KeyCode::Char('r') => {
                self.reload_open_page();
                return AppAction::Reload;
            }
            KeyCode::Char(c @ '1'..='6') => {
                let index = c as usize - '1' as usize;
                if let Some(page) = PageId::from_index(index) {
                    self.go_to(page);
                }
                return AppAction::None;
            }
            KeyCode::Char(']') => {
                let next = next_page(self.page, 1);
                self.go_to(next);
                return AppAction::None;
            }
            KeyCode::Char('[') => {
                let prev = next_page(self.page, -1);
                self.go_to(prev);
                return AppAction::None;
            }
            _ => {}
        }
        let _ = self.pages.mut_page(self.page).on_key(key);
        AppAction::None
    }

    /// Starts a pending mutation if none is active.
    pub fn try_start_pending(&mut self, label: impl Into<String>) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(PendingOp {
            label: label.into(),
            started: Instant::now(),
        });
        true
    }

    /// Clears pending mutation.
    pub fn finish_pending(&mut self) {
        self.pending = None;
    }

    /// Sets flash with TTL for tests.
    pub fn set_flash_ttl(&mut self, text: impl Into<String>, ok: bool) {
        self.flash = Some(Flash {
            text: text.into(),
            ok,
            until: Some(Instant::now() + FLASH_TTL),
        });
    }
}

fn next_page(current: PageId, delta: i32) -> PageId {
    let pos = PageId::CYCLE
        .iter()
        .position(|p| *p == current)
        .unwrap_or(0) as i32;
    let len = PageId::CYCLE.len() as i32;
    let next = (pos + delta).rem_euclid(len) as usize;
    PageId::CYCLE[next]
}

/// Outcome of [`App::handle_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppAction {
    /// Redraw only.
    None,
    /// Reload was requested.
    Reload,
    /// User requested quit.
    Quit,
}

#[cfg(test)]
mod tests {
    use super::{App, next_page};
    use crate::state::PageId;

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::empty())
    }

    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn navigation_cycles_six_pages() {
        assert_eq!(next_page(PageId::Library, 1), PageId::Agents);
        assert_eq!(next_page(PageId::Agents, -1), PageId::Library);
    }

    #[test]
    fn digit_two_opens_providers() {
        let mut app = App::for_test(PageId::Agents);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.page, PageId::Providers);
    }

    #[test]
    fn page_key_clears_flash() {
        let mut app = App::for_test(PageId::Agents);
        app.set_flash(crate::state::Flash::new("x", true));
        app.handle_key(key(KeyCode::Char('3')));
        assert!(app.flash.is_none());
    }

    #[test]
    fn question_opens_help_and_esc_closes() {
        let mut app = App::for_test(PageId::Usage);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, crate::state::Mode::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, crate::state::Mode::List);
    }

    #[test]
    fn stale_generation_drops_result() {
        let mut app = App::for_test(PageId::Agents);
        let stale = app.generation();
        app.reload_open_page();
        let mut applied = false;
        app.apply_read_result(stale, |_| applied = true);
        assert!(!applied);
    }

    #[test]
    fn flash_replaces_previous() {
        let mut app = App::for_test(PageId::Agents);
        app.set_flash(crate::state::Flash::new("first", true));
        app.set_flash(crate::state::Flash::new("second", false));
        assert_eq!(app.flash.as_ref().map(|f| f.text.as_str()), Some("second"));
    }

    #[test]
    fn reload_increments_page_generation() {
        let mut app = App::for_test(PageId::Sessions);
        app.reload_open_page();
        assert_eq!(app.pages.page(PageId::Sessions).fetch_gen(), 2);
    }
}
