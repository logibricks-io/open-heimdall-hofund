//! Application state and key handling.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hofund_control_client::hofund_control_proto::{
    AgentAction, AgentModelsResponse, ProfileSummary, ProviderKey,
};

use crate::intent::{ControlIntent, PageEffect};
use crate::pages::{AgentPage, Page, Pages, ProviderPage};
use crate::session::{
    AgentSnapshot, Finished, ModelView, PlanView, ProviderSnapshot, SessionOutcome,
};
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
    /// Effective model listener from the snapshot event.
    pub effective_listen: String,
    /// Optional mutating operation.
    pub pending: Option<PendingOp>,
    /// Catalog sync indicator (Agents `S` in R-14).
    pub catalog_syncing: bool,
    /// Page states.
    pub pages: Pages,
    /// Providers screen. The shell stays in charge until the first load.
    pub providers: ProviderPage,
    /// Agents screen. The shell stays in charge until the first load.
    pub agents: AgentPage,
    /// Monotonic generation for in-flight reads.
    generation: Generation,
    /// Control calls waiting for a client.
    intents: Vec<ControlIntent>,
    /// Balances are requested once, on the first Providers visit.
    balances_once: bool,
    /// Startup catalog sync runs once per process.
    catalog_once: bool,
    /// The empty-agents redirect to Providers happens once.
    agents_empty_routed: bool,
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
            effective_listen: String::new(),
            pending: None,
            catalog_syncing: false,
            pages: Pages::default(),
            providers: ProviderPage::default(),
            agents: AgentPage::default(),
            generation: 1,
            intents: Vec::new(),
            balances_once: false,
            catalog_once: false,
            agents_empty_routed: false,
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

    /// Marks sign-in progress (Accounts polling is R-15).
    pub fn mark_sign_in_dirty(&mut self) {
        let _ = ();
    }

    /// Whether sign-in needs polling (R-15).
    #[must_use]
    pub fn sign_in_dirty(&self) -> bool {
        false
    }

    /// Removes the token reread flag.
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

    /// Reloads an open model picker from the cached list after a refresh event.
    pub fn refresh_open_picker(&mut self) {
        let Some(id) = self.providers.picker_provider().map(str::to_owned) else {
            return;
        };
        let generation = self.generation;
        self.queue(ControlIntent::OpenModels { id, generation });
    }

    /// Schedules reload of the open page and returns the new read generation.
    #[must_use]
    pub fn reload_open_page(&mut self) -> Generation {
        self.bump_generation();
        let read_gen = self.generation;
        self.pages.mut_page(self.page).reload(read_gen);
        self.queue_page_load(read_gen);
        #[cfg(test)]
        {
            self.reloads += 1;
        }
        read_gen
    }

    fn queue_page_load(&mut self, generation: Generation) {
        match self.page {
            PageId::Providers => {
                self.queue(ControlIntent::LoadProviders { generation });
                if !self.balances_once {
                    self.balances_once = true;
                    self.queue(ControlIntent::LoadBalances);
                }
            }
            PageId::Agents => self.queue(ControlIntent::LoadAgents { generation }),
            _ => {}
        }
    }

    fn queue(&mut self, intent: ControlIntent) {
        self.intents.push(intent);
    }

    /// Control calls queued since the last take.
    pub fn take_intents(&mut self) -> Vec<ControlIntent> {
        std::mem::take(&mut self.intents)
    }

    /// Starts the startup catalog sync and loads the open page.
    pub fn on_control_attached(&mut self) {
        if !self.catalog_once {
            self.catalog_once = true;
            self.catalog_syncing = true;
            self.queue(ControlIntent::CatalogSync { force: false });
        }
        let _ = self.reload_open_page();
    }

    /// Returns the next read generation without consuming.
    #[must_use]
    pub fn next_generation(&self) -> Generation {
        self.generation + 1
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Current read generation.
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
        let _ = self.reload_open_page();
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
        if self.page_captures() {
            self.dispatch_active_page(key);
            return AppAction::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return AppAction::Quit,
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return AppAction::None;
            }
            KeyCode::Char('r') => {
                let _ = self.reload_open_page();
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
        self.dispatch_active_page(key);
        AppAction::None
    }

    fn page_captures(&self) -> bool {
        match self.page {
            PageId::Providers => self.providers.captures(),
            PageId::Agents => self.agents.captures(),
            _ => false,
        }
    }

    fn dispatch_active_page(&mut self, key: KeyEvent) {
        if self.page == PageId::Providers && (self.providers.active() || self.providers.captures())
        {
            let effect = self.providers.on_key(key);
            self.apply_effect(effect);
            return;
        }
        if self.page == PageId::Agents && (self.agents.active() || self.agents.captures()) {
            let effect = self.agents.on_key(key);
            self.apply_effect(effect);
            return;
        }
        let _ = self.pages.mut_page(self.page).on_key(key);
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

    fn apply_effect(&mut self, mut effect: PageEffect) {
        if let Some((text, ok)) = effect.flash.take() {
            self.set_flash_ttl(text, ok);
        }
        if effect.catalog_sync {
            self.catalog_syncing = true;
        }
        if let Some(label) = effect.pending
            && !self.try_start_pending(label)
        {
            return;
        }
        let generation = self.generation;
        for intent in &mut effect.intents {
            intent.stamp_generation(generation);
        }
        self.intents.extend(effect.intents);
    }

    /// Applies one finished control call.
    pub fn apply_session(&mut self, outcome: SessionOutcome) {
        match outcome {
            SessionOutcome::Providers { generation, result } => {
                self.apply_providers(generation, result);
            }
            SessionOutcome::Agents { generation, result } => self.apply_agents(generation, result),
            SessionOutcome::Models {
                generation,
                id,
                open,
                result,
                flash,
            } => self.apply_models(generation, id, open, result, flash),
            SessionOutcome::Keys { id, result } => self.apply_keys(id, result),
            SessionOutcome::Balances(result) => self.apply_balances(result),
            SessionOutcome::Profiles(result) => self.apply_profiles(result),
            SessionOutcome::RouteModels {
                generation,
                agent,
                result,
            } => self.apply_route_models(generation, agent, result),
            SessionOutcome::AgentModels {
                generation,
                agent,
                result,
            } => self.apply_agent_models(generation, agent, result),
            SessionOutcome::Plan {
                agent,
                action,
                result,
            } => self.apply_plan(agent, action, result),
            SessionOutcome::Finished(finished) => self.apply_finished(finished),
            SessionOutcome::Catalog(result) => self.apply_catalog(result),
        }
    }

    fn apply_providers(
        &mut self,
        generation: Generation,
        result: Result<ProviderSnapshot, String>,
    ) {
        self.apply_read_result(generation, |app| match result {
            Ok(snapshot) => {
                if let Some(error) = snapshot.presets_error.clone() {
                    app.set_flash_ttl(error, false);
                }
                if snapshot.disk_revision.len() == 64 {
                    app.disk_revision.clone_from(&snapshot.disk_revision);
                }
                app.providers.apply_load(
                    snapshot.providers,
                    snapshot.presets,
                    snapshot.disk_revision,
                );
            }
            Err(message) => app.set_flash_ttl(message, false),
        });
    }

    fn apply_agents(&mut self, generation: Generation, result: Result<AgentSnapshot, String>) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(snapshot) => {
                self.agents.apply_load(snapshot.agents, snapshot.status);
                if self.agents.is_empty()
                    && self.page == PageId::Agents
                    && !self.agents_empty_routed
                {
                    self.agents_empty_routed = true;
                    self.go_to(PageId::Providers);
                    self.set_flash_ttl("no agents were detected", false);
                }
            }
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_models(
        &mut self,
        generation: Generation,
        id: String,
        open: bool,
        result: Result<ModelView, String>,
        flash: Option<(String, bool)>,
    ) {
        let refresh = flash.is_some();
        self.apply_read_result(generation, |app| match result {
            Ok(view) => app
                .providers
                .apply_models(&id, &view.models, &view.picks, open),
            Err(message) => app.set_flash_ttl(message, false),
        });
        if let Some((text, ok)) = flash
            && generation == self.generation
        {
            self.set_flash_ttl(text, ok);
        }
        if refresh {
            self.finish_pending();
        }
    }

    fn apply_keys(&mut self, id: String, result: Result<Vec<ProviderKey>, String>) {
        match result {
            Ok(keys) => {
                let effect = self.providers.apply_keys(&id, keys);
                self.apply_effect(effect);
            }
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_balances(&mut self, result: Result<Vec<String>, String>) {
        match result {
            Ok(lines) => self.providers.apply_balances(lines),
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_profiles(&mut self, result: Result<Vec<ProfileSummary>, String>) {
        match result {
            Ok(profiles) => self.agents.apply_profiles(profiles, true),
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_route_models(
        &mut self,
        generation: Generation,
        agent: String,
        result: Result<Vec<String>, String>,
    ) {
        self.apply_read_result(generation, |app| match result {
            Ok(ids) => app.agents.apply_route_models(&agent, ids),
            Err(message) => app.set_flash_ttl(message, false),
        });
    }

    fn apply_agent_models(
        &mut self,
        generation: Generation,
        agent: String,
        result: Result<AgentModelsResponse, String>,
    ) {
        self.apply_read_result(generation, |app| match result {
            Ok(models) => app
                .agents
                .apply_visibility(&agent, models.shown, models.hidden),
            Err(message) => app.set_flash_ttl(message, false),
        });
    }

    fn apply_plan(&mut self, agent: String, action: AgentAction, result: Result<PlanView, String>) {
        self.finish_pending();
        match result {
            Ok(plan) => self
                .agents
                .apply_plan(&agent, action, plan.revision, plan.lines),
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_finished(&mut self, finished: Finished) {
        self.finish_pending();
        self.set_flash_ttl(finished.flash.0.clone(), finished.flash.1);
        if let Some(revision) = finished.disk_revision.clone() {
            self.providers.note_revision(&revision);
            self.disk_revision = revision;
        }
        if let Some((id, picks)) = &finished.picks {
            self.providers.apply_picks(id, picks);
        }
        if let Some(id) = &finished.revert_picks {
            self.providers.revert_picks(id);
        }
        if let Some(profiles) = finished.profiles.clone() {
            self.agents.apply_profiles(profiles, true);
        }
        if finished.reload_providers && self.page == PageId::Providers {
            let _ = self.reload_open_page();
        }
        if finished.reload_agents && self.page == PageId::Agents {
            let _ = self.reload_open_page();
        }
        if finished.reload_profiles {
            self.queue(ControlIntent::LoadProfiles);
        }
    }

    fn apply_catalog(&mut self, result: Result<(), String>) {
        self.catalog_syncing = false;
        match result {
            Ok(()) => self.set_flash_ttl("catalog synced", true),
            Err(message) => self.set_flash_ttl(message, false),
        }
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
        let _ = app.reload_open_page();
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
        let _ = app.reload_open_page();
        assert_eq!(app.pages.page(PageId::Sessions).fetch_gen(), 2);
    }
}
