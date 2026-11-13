//! Application state and key handling.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hofund_control_client::hofund_control_proto::{
    AgentAction, AgentModelsResponse, GroupDeleteResponse, GroupListResponse,
    GroupMemberCandidatesResponse, GroupWriteResponse, ProfileSummary, ProviderKey,
};

use crate::intent::{ControlIntent, PageEffect};
use crate::pages::{AgentPage, Page, PageKeyOutcome, Pages, ProviderPage, UsageAction};
use crate::session::{
    AgentSnapshot, Finished, ModelView, PlanView, ProviderSnapshot, RouteSnapshot, SessionOutcome,
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
        let Some(mut intent) = self.providers.reopen_models() else {
            return;
        };
        intent.stamp_generation(self.generation);
        self.queue(intent);
    }

    /// Schedules reload of the open page and returns the new read generation.
    #[must_use]
    pub fn reload_open_page(&mut self) -> Generation {
        self.bump_generation();
        let read_gen = self.generation;
        match self.page {
            PageId::Sessions => self.pages.sessions_mut().reload(read_gen),
            PageId::Routing => self.pages.routing_mut().reload(read_gen),
            PageId::Usage => self.pages.usage_mut().reload(read_gen),
            page => self.pages.mut_page(page).reload(read_gen),
        }
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
            PageId::Routing => self.queue(ControlIntent::LoadGroups { generation }),
            PageId::Usage => {
                self.queue(ControlIntent::LoadUsage {
                    generation,
                    period: self.pages.usage().period(),
                });
                self.queue(ControlIntent::LoadUsageSettings { generation });
                self.queue(ControlIntent::LoadQuotaAlerts { generation });
            }
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
        if self.page == PageId::Usage {
            let intent = self.pages.usage_mut().open_quotas(Instant::now());
            self.queue(intent);
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
        self.providers.disarm_confirm();
        self.agents.cancel_profile_request();
        self.pages.routing_mut().leave();
        self.page = page;
        self.mode = Mode::List;
        let _ = self.reload_open_page();
        if page == PageId::Usage {
            let intent = self.pages.usage_mut().open_quotas(Instant::now());
            self.queue(intent);
        }
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
            return self.dispatch_active_page(key);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return AppAction::Quit,
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return AppAction::None;
            }
            KeyCode::Char('r') => {
                let _ = self.reload_open_page();
                if self.page == PageId::Usage {
                    let intent = self.pages.usage_mut().refresh_quotas(Instant::now());
                    self.queue(intent);
                }
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
        self.dispatch_active_page(key)
    }

    fn page_captures(&self) -> bool {
        match self.page {
            PageId::Providers => self.providers.captures(),
            PageId::Agents => self.agents.captures(),
            PageId::Routing => self.pages.routing().captures(),
            PageId::Sessions => self.pages.sessions().captures(),
            PageId::Usage => self.pages.usage().captures(),
            _ => false,
        }
    }

    fn dispatch_active_page(&mut self, key: KeyEvent) -> AppAction {
        if self.page == PageId::Sessions {
            let outcome = self.pages.sessions_mut().on_key(key);
            return self.apply_sessions_outcome(outcome);
        }
        if self.page == PageId::Routing {
            let effect = self.pages.routing_mut().on_key(key);
            self.apply_effect(effect);
            return AppAction::None;
        }
        if self.page == PageId::Usage {
            let outcome = self.pages.usage_mut().on_key(key);
            return self.apply_usage_outcome(outcome);
        }
        if self.page == PageId::Providers && (self.providers.active() || self.providers.captures())
        {
            let effect = self.providers.on_key(key);
            self.apply_effect(effect);
            return AppAction::None;
        }
        if self.page == PageId::Agents && (self.agents.active() || self.agents.captures()) {
            let effect = self.agents.on_key(key);
            self.apply_effect(effect);
            return AppAction::None;
        }
        let _ = self.pages.mut_page(self.page).on_key(key);
        AppAction::None
    }

    fn apply_sessions_outcome(&mut self, outcome: PageKeyOutcome) -> AppAction {
        match outcome {
            PageKeyOutcome::None | PageKeyOutcome::CursorUp | PageKeyOutcome::CursorDown => {
                AppAction::None
            }
            PageKeyOutcome::Reload => {
                let _ = self.reload_open_page();
                AppAction::Reload
            }
            PageKeyOutcome::Resume(resume) => AppAction::Resume(resume),
            PageKeyOutcome::FlashResumeUnavailable => AppAction::Flash {
                text: "this session can't be resumed from here".to_owned(),
                ok: false,
            },
            PageKeyOutcome::ClosePicker => {
                self.pages.sessions_mut().close_picker();
                AppAction::None
            }
            PageKeyOutcome::PickFilter(value) => {
                self.pages.sessions_mut().apply_filter_pick(value);
                let _ = self.reload_open_page();
                AppAction::Reload
            }
            PageKeyOutcome::Usage(_) => AppAction::None,
        }
    }

    fn apply_usage_outcome(&mut self, outcome: PageKeyOutcome) -> AppAction {
        match outcome {
            PageKeyOutcome::Reload => {
                let _ = self.reload_open_page();
                AppAction::Reload
            }
            PageKeyOutcome::Usage(UsageAction::ToggleQuotaLeft) => {
                if let Some(intent) = self.pages.usage_mut().toggle_quota_left() {
                    self.apply_effect(PageEffect::mutation("saving quota display", intent));
                }
                AppAction::None
            }
            PageKeyOutcome::Usage(UsageAction::CheckIn) => {
                self.apply_effect(PageEffect::mutation(
                    "checking in every account",
                    ControlIntent::RunCheckins,
                ));
                AppAction::None
            }
            PageKeyOutcome::Usage(UsageAction::SetUsageAlert(value)) => {
                self.apply_effect(PageEffect::mutation(
                    "saving usage alert",
                    ControlIntent::SetQuotaAlert {
                        usage: Some(value),
                        balance: None,
                    },
                ));
                AppAction::None
            }
            PageKeyOutcome::Usage(UsageAction::SetBalanceAlert(value)) => {
                self.apply_effect(PageEffect::mutation(
                    "saving balance alert",
                    ControlIntent::SetQuotaAlert {
                        usage: None,
                        balance: Some(value),
                    },
                ));
                AppAction::None
            }
            PageKeyOutcome::None
            | PageKeyOutcome::CursorUp
            | PageKeyOutcome::CursorDown
            | PageKeyOutcome::Resume(_)
            | PageKeyOutcome::FlashResumeUnavailable
            | PageKeyOutcome::ClosePicker
            | PageKeyOutcome::PickFilter(_) => AppAction::None,
        }
    }

    /// Starts a pending mutation if none is active.
    pub fn try_start_pending(&mut self, label: impl Into<String>) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(PendingOp {
            label: label.into(),
            started: Instant::now(),
            generation: self.generation,
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
                token,
                owns_pending,
                result,
                flash,
            } => self.apply_models(ModelApply {
                generation,
                id,
                open,
                token,
                owns_pending,
                result,
                flash,
            }),
            SessionOutcome::Keys { id, token, result } => self.apply_keys(id, token, result),
            SessionOutcome::Balances(result) => self.apply_balances(result),
            SessionOutcome::Usage { generation, result } => {
                self.apply_read_result(generation, |app| match result {
                    Ok(response) => app.pages.usage_mut().apply_usage(response),
                    Err(message) => app.pages.usage_mut().apply_usage_error(message),
                });
            }
            SessionOutcome::UsageSettings { generation, result } => {
                self.apply_read_result(generation, |app| {
                    if let Ok(settings) = result {
                        if settings.disk_revision.len() == 64 {
                            app.disk_revision.clone_from(&settings.disk_revision);
                        }
                        app.pages.usage_mut().apply_settings(settings);
                    }
                });
            }
            SessionOutcome::Quotas { token, result } => match result {
                Ok(response) => {
                    if let Some(intent) = self.pages.usage_mut().apply_quotas(
                        token,
                        response,
                        self.page == PageId::Usage,
                    ) {
                        self.queue(intent);
                    }
                }
                Err(message) => self.pages.usage_mut().apply_quotas_error(token, message),
            },
            SessionOutcome::Checkins(result) => {
                self.finish_pending();
                match result {
                    Ok((text, ok)) => self.set_flash_ttl(text, ok),
                    Err(message) => self.set_flash_ttl(message, false),
                }
            }
            SessionOutcome::QuotaAlerts { generation, result } => {
                self.apply_read_result(generation, |app| match result {
                    Ok(alerts) => app.pages.usage_mut().apply_alerts(alerts),
                    Err(message) => app.pages.usage_mut().apply_alerts_error(message),
                });
            }
            SessionOutcome::QuotaAlertSaved(result) => {
                self.finish_pending();
                match result {
                    Ok(response) => {
                        let alerts =
                            hofund_control_client::hofund_control_proto::QuotaAlertsResponse {
                                version: response.version,
                                usage_alert: response.usage_alert,
                                balance_alert: response.balance_alert,
                            };
                        self.pages.usage_mut().apply_alerts(alerts);
                        self.set_flash_ttl("saved quota alerts", true);
                    }
                    Err(message) => self.set_flash_ttl(message, false),
                }
            }
            SessionOutcome::QuotaLeftSaved { result, previous } => {
                self.finish_pending();
                match result {
                    Ok(settings) => {
                        self.disk_revision.clone_from(&settings.disk_revision);
                        self.pages.usage_mut().apply_settings(settings);
                        self.set_flash_ttl("saved quota display", true);
                    }
                    Err(message) => {
                        self.pages.usage_mut().rollback_quota_left(previous);
                        self.set_flash_ttl(message, false);
                    }
                }
            }
            SessionOutcome::Profiles { token, result } => self.apply_profiles(token, result),
            SessionOutcome::RouteModels {
                generation,
                agent,
                result,
            } => self.apply_route_models(generation, agent, result),
            SessionOutcome::AgentModels {
                generation,
                agent,
                open,
                result,
            } => self.apply_agent_models(generation, agent, open, result),
            SessionOutcome::Plan {
                agent,
                action,
                result,
            } => self.apply_plan(agent, action, result),
            SessionOutcome::Groups { generation, result } => {
                self.apply_groups(generation, result);
            }
            SessionOutcome::GroupCandidates { token, result } => {
                self.apply_group_candidates(token, result);
            }
            SessionOutcome::RouteView { generation, result } => {
                self.apply_route_view(generation, result);
            }
            SessionOutcome::GroupWrite {
                id,
                success,
                result,
            } => self.apply_group_write(id, success, result),
            SessionOutcome::GroupDelete { id, result } => self.apply_group_delete(id, result),
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
                    return;
                }
                self.queue_agent_visibility();
            }
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_models(&mut self, apply: ModelApply) {
        let ModelApply {
            generation,
            id,
            open,
            token,
            owns_pending,
            result,
            flash,
        } = apply;
        // models_refreshed and a config reload bump the read generation while
        // this refresh still owns the keyboard. The stale list stays unused.
        // The release is limited to the generation that took the slot, so an
        // older completion cannot clear a mutation that started later.
        if generation == self.generation {
            match result {
                Ok(view) => {
                    let allow = !open || self.providers.accepts_open(&id, token);
                    if allow {
                        self.providers
                            .apply_models(&id, &view.models, &view.picks, open);
                    }
                }
                Err(message) => self.set_flash_ttl(message, false),
            }
            if let Some((text, ok)) = flash {
                self.set_flash_ttl(text, ok);
            }
        } else if owns_pending && self.pending_generation_is(generation) {
            if let Some((text, ok)) = flash {
                self.set_flash_ttl(text, ok);
            } else if let Err(message) = result {
                self.set_flash_ttl(message, false);
            }
        }
        if owns_pending && self.pending_generation_is(generation) {
            self.finish_pending();
        }
    }

    fn pending_generation_is(&self, generation: Generation) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
    }

    fn apply_keys(&mut self, id: String, token: u64, result: Result<Vec<ProviderKey>, String>) {
        match result {
            Ok(keys) => {
                let effect = self.providers.apply_keys(&id, token, keys);
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

    fn apply_profiles(&mut self, token: u64, result: Result<Vec<ProfileSummary>, String>) {
        match result {
            Ok(profiles) => self.agents.accept_profiles(profiles, token),
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
        open: bool,
        result: Result<AgentModelsResponse, String>,
    ) {
        self.apply_read_result(generation, |app| match result {
            Ok(models) => {
                if open {
                    app.agents
                        .apply_visibility(&agent, models.shown, models.hidden);
                } else {
                    app.agents
                        .remember_visibility(&agent, models.shown, models.hidden);
                }
            }
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

    fn apply_groups(&mut self, generation: Generation, result: Result<GroupListResponse, String>) {
        self.apply_read_result(generation, |app| match result {
            Ok(response) => {
                app.disk_revision.clone_from(&response.disk_revision);
                app.pages.routing_mut().apply_load(response);
            }
            Err(message) => {
                app.pages.routing_mut().apply_load_error(message.clone());
                app.set_flash_ttl(message, false);
            }
        });
    }

    fn apply_group_candidates(
        &mut self,
        token: u64,
        result: Result<GroupMemberCandidatesResponse, String>,
    ) {
        match result {
            Ok(response) => self.pages.routing_mut().apply_candidates(token, response),
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_route_view(&mut self, generation: Generation, result: Result<RouteSnapshot, String>) {
        self.apply_read_result(generation, |app| match result {
            Ok(snapshot) => app.pages.routing_mut().apply_route_view(snapshot),
            Err(message) => app.set_flash_ttl(message, false),
        });
    }

    fn apply_group_write(
        &mut self,
        id: String,
        success: String,
        result: Result<GroupWriteResponse, String>,
    ) {
        self.finish_pending();
        match result {
            Ok(response) => {
                self.disk_revision.clone_from(&response.disk_revision);
                self.pages.routing_mut().apply_group_write(
                    &id,
                    response.group,
                    response.disk_revision,
                );
                self.set_flash_ttl(success, true);
            }
            Err(message) => self.set_flash_ttl(message, false),
        }
    }

    fn apply_group_delete(&mut self, id: String, result: Result<GroupDeleteResponse, String>) {
        self.finish_pending();
        match result {
            Ok(response) => {
                self.disk_revision.clone_from(&response.disk_revision);
                self.pages
                    .routing_mut()
                    .apply_group_delete(&id, response.disk_revision);
                self.set_flash_ttl(format!("deleted {id}"), true);
            }
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
        if let Some(agent) = &finished.commit_hidden {
            self.agents.commit_hidden(agent);
        }
        if let Some(agent) = &finished.revert_hidden {
            self.agents.revert_hidden(agent);
        }
        if let Some(profiles) = finished.profiles.clone() {
            self.agents.replace_profiles(profiles);
        }
        if finished.reload_providers && self.page == PageId::Providers {
            let _ = self.reload_open_page();
        }
        if finished.reload_agents && self.page == PageId::Agents {
            let _ = self.reload_open_page();
        }
        if finished.reload_profiles {
            let token = self.agents.reopen_profiles();
            self.queue(ControlIntent::LoadProfiles { token });
        }
    }

    fn queue_agent_visibility(&mut self) {
        let generation = self.generation;
        for agent in self.agents.agent_ids() {
            self.queue(ControlIntent::LoadAgentModels {
                agent,
                generation,
                open: false,
            });
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

struct ModelApply {
    generation: Generation,
    id: String,
    open: bool,
    token: u64,
    owns_pending: bool,
    result: Result<ModelView, String>,
    flash: Option<(String, bool)>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppAction {
    /// Redraw only.
    None,
    /// Reload was requested.
    Reload,
    /// User requested quit.
    Quit,
    /// Resume an agent in the foreground from argv.
    Resume(hofund_control_proto::SessionResume),
    /// Show a flash without a reload.
    Flash { text: String, ok: bool },
}

#[cfg(test)]
mod tests {
    use super::{App, next_page};
    use crate::intent::ControlIntent;
    use crate::session::{Finished, ModelView, ProviderSnapshot, SessionOutcome};
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
        assert_eq!(app.pages.sessions().fetch_gen(), 2);
    }

    #[test]
    fn page_change_disarms_provider_delete_and_reload_does_not() {
        let mut app = loaded_providers("11");
        app.handle_key(key(KeyCode::Char('d')));
        assert!(
            !app.take_intents()
                .iter()
                .any(|intent| matches!(intent, ControlIntent::DeleteProvider { .. }))
        );
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(
            !app.take_intents()
                .iter()
                .any(|intent| matches!(intent, ControlIntent::DeleteProvider { .. })),
            "leaving the page must disarm delete"
        );

        let mut app = loaded_providers("11");
        app.handle_key(key(KeyCode::Char('d')));
        let _ = app.take_intents();
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(
            app.take_intents().iter().any(
                |intent| matches!(intent, ControlIntent::DeleteProvider { id, .. } if id == "first")
            ),
            "reload on the list must leave the second d armed"
        );
    }

    #[test]
    fn successful_write_revision_is_used_for_the_next_delete() {
        let mut app = loaded_providers("11");
        app.apply_session(SessionOutcome::Finished(done(
            Some("22".repeat(32)),
            false,
            false,
        )));
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('d')));
        let revision = app
            .take_intents()
            .into_iter()
            .find_map(|intent| match intent {
                ControlIntent::DeleteProvider { revision, .. } => Some(revision),
                _ => None,
            });
        assert_eq!(revision.as_deref(), Some("22".repeat(32).as_str()));
    }

    #[test]
    fn late_model_open_for_another_provider_does_not_capture_toggle() {
        let mut app = loaded_providers("ab");
        app.handle_key(key(KeyCode::Enter));
        let first = open_token(&app.take_intents());
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        let second = open_token(&app.take_intents());
        let generation = app.generation();
        app.apply_session(open_models(generation, "second", second));
        app.apply_session(open_models(generation, "first", first));
        app.handle_key(key(KeyCode::Enter));
        let intents = app.take_intents();
        assert!(
            matches!(
                intents.as_slice(),
                [ControlIntent::ReplacePicks { id, .. }] if id == "second"
            ),
            "late open of first must not take the toggle: {intents:?}"
        );
    }

    #[test]
    fn late_keys_do_not_replace_an_address_ask() {
        let mut app = App::for_test(PageId::Providers);
        let mut row = provider("first");
        row.preset = Some("remote-hofund".to_owned());
        row.base_url = "http://127.0.0.1:9/v1".to_owned();
        let mut other = provider("second");
        other.base_url = "http://127.0.0.1:9/v1".to_owned();
        let mut preset = hofund_control_client::hofund_control_proto::PresetEntry::default();
        preset.id = "remote-hofund".to_owned();
        preset.needs_address = true;
        preset.kind = hofund_control_client::hofund_control_proto::PresetKind::Remote;
        app.apply_session(SessionOutcome::Providers {
            generation: app.generation(),
            result: Ok(ProviderSnapshot {
                providers: vec![row, other],
                presets: vec![preset],
                disk_revision: "ab".repeat(32),
                presets_error: None,
            }),
        });
        let _ = app.take_intents();
        app.handle_key(key(KeyCode::Char('w')));
        app.apply_session(SessionOutcome::Keys {
            id: "first".to_owned(),
            token: 1,
            result: Ok(vec![one_key()]),
        });
        let frame = provider_frame(&app);
        assert!(
            frame.contains("address"),
            "key list must not cover the address line: {frame}"
        );
        assert!(!frame.contains("new key"), "{frame}");
    }

    #[test]
    fn model_timeout_does_not_clear_another_mutations_pending_flag() {
        let mut app = loaded_providers("ab");
        assert!(app.try_start_pending("testing provider"));
        let timed_out = ControlIntent::OpenModels {
            id: "first".to_owned(),
            generation: app.generation(),
            token: 1,
        }
        .timed_out();
        app.apply_session(timed_out);
        assert!(
            app.pending.is_some(),
            "an open-models timeout must not release a test that is still in flight"
        );

        let mut app = loaded_providers("ab");
        app.handle_key(key(KeyCode::Char('m')));
        assert!(app.pending.is_some());
        let timed_out = ControlIntent::RefreshModels {
            id: "first".to_owned(),
            generation: app.generation(),
        }
        .timed_out();
        app.apply_session(timed_out);
        assert!(
            app.pending.is_none(),
            "a refresh timeout still ends that refresh"
        );
    }

    #[test]
    fn profile_load_after_escape_does_not_reopen_the_picker() {
        let mut app = loaded_agents();
        app.handle_key(key(KeyCode::Char('p')));
        let token = profile_token(&app.take_intents());
        app.handle_key(key(KeyCode::Esc));
        app.apply_session(SessionOutcome::Profiles {
            token,
            result: Ok(vec![profile("work")]),
        });
        app.handle_key(key(KeyCode::Enter));
        assert!(
            !app.take_intents()
                .iter()
                .any(|intent| matches!(intent, ControlIntent::SwitchProfile { .. })),
            "escape cancels the profile picker"
        );
    }

    #[test]
    fn profile_load_still_switches_when_the_picker_stays_open() {
        let mut app = loaded_agents();
        app.handle_key(key(KeyCode::Char('p')));
        let token = profile_token(&app.take_intents());
        app.apply_session(SessionOutcome::Profiles {
            token,
            result: Ok(vec![profile("work")]),
        });
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_intents().iter().any(
            |intent| matches!(intent, ControlIntent::SwitchProfile { name } if name == "work")
        ));
    }

    #[test]
    fn failed_hidden_write_does_not_stay_in_the_next_request() {
        let mut app = loaded_agents();
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Enter));
        let _ = app.take_intents();
        app.apply_session(SessionOutcome::AgentModels {
            generation: app.generation(),
            agent: "claude".to_owned(),
            open: true,
            result: Ok(models(&["a", "b"], &[])),
        });
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        let first = app.take_intents();
        assert!(
            matches!(
                first.as_slice(),
                [ControlIntent::SetHiddenModels { ids, .. }] if ids == &["a".to_owned()]
            ),
            "{first:?}"
        );
        app.apply_session(
            ControlIntent::SetHiddenModels {
                agent: "claude".to_owned(),
                ids: vec!["a".to_owned()],
            }
            .timed_out(),
        );
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        let second = app.take_intents();
        assert!(
            matches!(
                second.as_slice(),
                [ControlIntent::SetHiddenModels { ids, .. }] if ids == &["b".to_owned()]
            ),
            "failed hide of a must not remain in the next write: {second:?}"
        );
    }

    #[test]
    fn agent_list_shows_hidden_from_the_models_read_after_reload() {
        let mut app = loaded_agents();
        let reads = app.take_intents();
        assert!(
            reads.iter().any(
                |intent| matches!(intent, ControlIntent::LoadAgentModels { agent, .. } if agent == "claude")
            ),
            "the list must read shown/hidden: {reads:?}"
        );
        app.apply_session(SessionOutcome::AgentModels {
            generation: app.generation(),
            agent: "claude".to_owned(),
            open: false,
            result: Ok(models(&["route-b"], &["route-a"])),
        });
        assert!(
            agent_frame(&app).contains("hidden"),
            "{}",
            agent_frame(&app)
        );

        app.apply_session(SessionOutcome::Finished(done(None, false, true)));
        let reload = app.take_intents();
        let generation = reload.iter().find_map(|intent| match intent {
            ControlIntent::LoadAgents { generation } => Some(*generation),
            _ => None,
        });
        let generation = generation.expect("reload");
        app.apply_session(SessionOutcome::Agents {
            generation,
            result: Ok(agent_snapshot()),
        });
        let again = app.take_intents();
        assert!(
            again.iter().any(
                |intent| matches!(intent, ControlIntent::LoadAgentModels { agent, .. } if agent == "claude")
            ),
            "reload must read visibility again: {again:?}"
        );
        app.apply_session(SessionOutcome::AgentModels {
            generation: app.generation(),
            agent: "claude".to_owned(),
            open: false,
            result: Ok(models(&["route-b"], &["route-a"])),
        });
        assert!(
            agent_frame(&app).contains("hidden"),
            "{}",
            agent_frame(&app)
        );
    }

    fn loaded_providers(nibble: &str) -> App {
        let mut app = App::for_test(PageId::Providers);
        app.apply_session(SessionOutcome::Providers {
            generation: app.generation(),
            result: Ok(ProviderSnapshot {
                providers: vec![provider("first"), provider("second")],
                presets: Vec::new(),
                disk_revision: nibble.repeat(32),
                presets_error: None,
            }),
        });
        let _ = app.take_intents();
        app
    }

    fn loaded_agents() -> App {
        let mut app = App::for_test(PageId::Agents);
        app.apply_session(SessionOutcome::Agents {
            generation: app.generation(),
            result: Ok(agent_snapshot()),
        });
        app
    }

    fn agent_snapshot() -> crate::session::AgentSnapshot {
        use hofund_control_client::hofund_control_proto::{
            AgentState, AgentStatusEntry, AgentSummary,
        };
        let mut summary = AgentSummary::default();
        summary.id = "claude".to_owned();
        summary.display_name = "claude".to_owned();
        summary.installed = true;
        let mut status = AgentStatusEntry::default();
        status.id = "claude".to_owned();
        status.state = AgentState::Disconnected;
        status.model = Some("route".to_owned());
        crate::session::AgentSnapshot {
            agents: vec![summary],
            status: vec![status],
        }
    }

    fn provider(id: &str) -> hofund_control_client::hofund_control_proto::ProviderSummary {
        let mut summary = hofund_control_client::hofund_control_proto::ProviderSummary::default();
        summary.id = id.to_owned();
        summary.enabled = true;
        summary.listed = true;
        summary.base_url = "https://example.test/v1".to_owned();
        summary
    }

    fn one_key() -> hofund_control_client::hofund_control_proto::ProviderKey {
        let mut key = hofund_control_client::hofund_control_proto::ProviderKey::default();
        key.id = "k1".to_owned();
        key.masked = "ab…yz".to_owned();
        key
    }

    fn profile(name: &str) -> hofund_control_client::hofund_control_proto::ProfileSummary {
        let mut profile = hofund_control_client::hofund_control_proto::ProfileSummary::default();
        profile.name = name.to_owned();
        profile
    }

    fn models(
        shown: &[&str],
        hidden: &[&str],
    ) -> hofund_control_client::hofund_control_proto::AgentModelsResponse {
        let mut models =
            hofund_control_client::hofund_control_proto::AgentModelsResponse::default();
        models.shown = shown.iter().map(|id| (*id).to_owned()).collect();
        models.hidden = hidden.iter().map(|id| (*id).to_owned()).collect();
        models
    }

    fn open_token(intents: &[ControlIntent]) -> u64 {
        intents
            .iter()
            .find_map(|intent| match intent {
                ControlIntent::OpenModels { token, .. } => Some(*token),
                _ => None,
            })
            .expect("open")
    }

    fn profile_token(intents: &[ControlIntent]) -> u64 {
        intents
            .iter()
            .find_map(|intent| match intent {
                ControlIntent::LoadProfiles { token } => Some(*token),
                _ => None,
            })
            .expect("profiles")
    }

    fn open_models(generation: u64, id: &str, token: u64) -> SessionOutcome {
        use hofund_control_client::hofund_control_proto::{ModelEntry, PicksResponse};
        let mut picks = PicksResponse::default();
        picks.picks = vec!["a".to_owned(), "b".to_owned()];
        picks.picks_revision = "ab".repeat(32);
        let models = ["a", "b"].into_iter().map(|name| {
            let mut model = ModelEntry::default();
            model.id = name.to_owned();
            model
        });
        SessionOutcome::Models {
            generation,
            id: id.to_owned(),
            open: true,
            token,
            owns_pending: false,
            result: Ok(ModelView {
                models: models.collect(),
                picks,
            }),
            flash: None,
        }
    }

    fn done(revision: Option<String>, reload_providers: bool, reload_agents: bool) -> Finished {
        Finished {
            flash: ("saved".to_owned(), true),
            reload_providers,
            reload_agents,
            reload_profiles: false,
            disk_revision: revision,
            picks: None,
            revert_picks: None,
            revert_hidden: None,
            commit_hidden: None,
            profiles: None,
        }
    }

    fn provider_frame(app: &App) -> String {
        frame(|area, buf| {
            app.providers
                .draw(area, buf, &crate::theme::Theme::monochrome());
        })
    }

    fn agent_frame(app: &App) -> String {
        frame(|area, buf| {
            app.agents
                .draw(area, buf, &crate::theme::Theme::monochrome());
        })
    }

    fn frame(draw: impl FnOnce(ratatui::layout::Rect, &mut ratatui::buffer::Buffer)) -> String {
        let area = ratatui::layout::Rect::new(0, 0, 80, 12);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        draw(area, &mut buf);
        buf.content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }
}
