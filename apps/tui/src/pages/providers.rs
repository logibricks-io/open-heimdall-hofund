//! Providers page: list, model picks, preset add, and field edits.

use std::collections::BTreeMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    ModelEntry, PicksResponse, PresetEntry, PresetKind, ProviderKey, ProviderSummary,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::status::{address_is_editable, is_account_row, is_project_preset, provider_status_line};
use crate::intent::{ControlIntent, PageEffect, Secret};
use crate::theme::Theme;
use crate::ui::list_window;

const REVISION_LEN: usize = 64;

/// Interactive providers screen.
#[derive(Clone, Debug, Default)]
pub struct ProviderPage {
    loaded: bool,
    cursor: usize,
    rows: Vec<ProviderSummary>,
    presets: Vec<PresetEntry>,
    disk_revision: String,
    mode: Mode,
    balances: Vec<String>,
    confirm: bool,
    read_token: u64,
    models_token: Option<u64>,
    keys_token: Option<u64>,
}

#[derive(Clone, Debug, Default)]
enum Mode {
    #[default]
    List,
    Models(Models),
    Presets {
        cursor: usize,
        filter: String,
    },
    Keys {
        provider: String,
        cursor: usize,
        keys: Vec<ProviderKey>,
    },
    Ask(Ask),
}

#[derive(Clone, Debug)]
struct Models {
    provider: String,
    cursor: usize,
    filter: String,
    ids: Vec<String>,
    enabled: Vec<String>,
    committed: Vec<String>,
    undo: Option<Vec<String>>,
    revision: String,
    read_only: bool,
}

#[derive(Clone)]
struct Ask {
    title: String,
    buffer: String,
    secret: bool,
    then: AskThen,
}

#[derive(Clone, Debug)]
enum AskThen {
    Create(CreateDraft),
    Family { id: String },
    BaseUrl { id: String },
    Replace { id: String, key_id: String },
}

#[derive(Clone, Debug)]
struct CreateDraft {
    preset: String,
    #[allow(dead_code)]
    kind: PresetKind,
    needs_key: bool,
    needs_address: bool,
    needs_project: bool,
    option_keys: Vec<String>,
    api_key: Option<Secret>,
    base_url: Option<String>,
    project: Option<String>,
    location: Option<String>,
    step: CreateStep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateStep {
    Key,
    Project,
    Location,
    Address,
}

impl ProviderPage {
    /// Whether a control load has been applied.
    #[must_use]
    pub fn active(&self) -> bool {
        self.loaded
    }

    /// Whether this page owns keys that would otherwise quit or change pages.
    #[must_use]
    pub fn captures(&self) -> bool {
        !matches!(self.mode, Mode::List)
    }

    /// Footer lines while this page is active.
    #[must_use]
    pub fn hint_lines(&self) -> (&'static str, &'static str) {
        match &self.mode {
            Mode::List => (
                "j/k row   enter models   a add   e key   t test   m refresh   b balances",
                "o on/off   u listed   w address   f family   dd delete   r reload",
            ),
            Mode::Models(_) => (
                "j/k model   enter toggle   x reset   ctrl+r refresh   esc back",
                "the last enabled model stays on",
            ),
            Mode::Presets { .. } | Mode::Keys { .. } => {
                ("j/k row   enter select   type to filter   esc back", "")
            }
            Mode::Ask(ask) if ask.secret => ("type the key   enter save   esc cancel", ""),
            Mode::Ask(_) => ("type a value   enter save   esc cancel", ""),
        }
    }

    /// Stores a 64-character disk revision from a list or a successful write.
    pub fn note_revision(&mut self, revision: &str) {
        if revision.len() == REVISION_LEN {
            self.disk_revision = revision.to_owned();
        }
    }

    /// Forgets a pending `dd` confirmation.
    pub fn disarm_confirm(&mut self) {
        self.confirm = false;
    }

    /// Whether an open-models response may replace the current screen.
    #[must_use]
    pub fn accepts_open(&self, id: &str, token: u64) -> bool {
        if self.models_token != Some(token) {
            return false;
        }
        if self.selected().is_none_or(|row| row.id != id) {
            return false;
        }
        match &self.mode {
            Mode::List => true,
            Mode::Models(models) => models.provider == id,
            Mode::Presets { .. } | Mode::Keys { .. } | Mode::Ask(_) => false,
        }
    }

    /// Re-reads models for the open picker and retires any older open.
    pub fn reopen_models(&mut self) -> Option<ControlIntent> {
        let id = self.picker_provider()?.to_owned();
        let token = self.next_token();
        self.models_token = Some(token);
        self.keys_token = None;
        Some(ControlIntent::OpenModels {
            id,
            generation: 0,
            token,
        })
    }

    /// Applies a provider list. Keeps the cursor when the same id is still present.
    pub fn apply_load(
        &mut self,
        providers: Vec<ProviderSummary>,
        presets: Vec<PresetEntry>,
        disk_revision: String,
    ) {
        let selected = self.rows.get(self.cursor).map(|row| row.id.clone());
        self.rows = providers;
        self.presets = presets;
        if disk_revision.len() == REVISION_LEN {
            self.disk_revision = disk_revision;
        }
        self.loaded = true;
        self.cursor = selected
            .and_then(|id| self.rows.iter().position(|row| row.id == id))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// Opens or refreshes the model picker.
    pub fn apply_models(
        &mut self,
        id: &str,
        models: &[ModelEntry],
        picks: &PicksResponse,
        open: bool,
    ) {
        let enabled = if picks.picks.is_empty() {
            picks.default_exposed.clone()
        } else {
            picks.picks.clone()
        };
        let read_only = picks.picks.is_empty() && picks.default_exposed.is_empty();
        let mut ids: Vec<String> = models.iter().map(|model| model.id.clone()).collect();
        for pick in &enabled {
            if !ids.iter().any(|existing| existing == pick) {
                ids.push(pick.clone());
            }
        }
        let same = matches!(&self.mode, Mode::Models(models) if models.provider == id);
        if open || same {
            let cursor = if same {
                match &self.mode {
                    Mode::Models(current) => current.cursor.min(ids.len().saturating_sub(1)),
                    _ => 0,
                }
            } else {
                0
            };
            let filter = if same {
                match &self.mode {
                    Mode::Models(current) => current.filter.clone(),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            self.mode = Mode::Models(Models {
                provider: id.to_owned(),
                cursor,
                filter,
                ids,
                enabled: enabled.clone(),
                committed: enabled,
                undo: None,
                revision: picks.picks_revision.clone(),
                read_only,
            });
        }
    }

    /// Updates the picker after a successful picks write.
    pub fn apply_picks(&mut self, id: &str, picks: &PicksResponse) {
        if let Mode::Models(models) = &mut self.mode
            && models.provider == id
        {
            models.revision = picks.picks_revision.clone();
            models.undo = None;
            let enabled = if picks.picks.is_empty() {
                picks.default_exposed.clone()
            } else {
                picks.picks.clone()
            };
            models.enabled = enabled.clone();
            models.committed = enabled;
            models.read_only = picks.picks.is_empty() && picks.default_exposed.is_empty();
        }
    }

    /// Restores the enabled set after a rejected picks write.
    pub fn revert_picks(&mut self, id: &str) {
        if let Mode::Models(models) = &mut self.mode
            && models.provider == id
            && let Some(undo) = models.undo.take()
        {
            models.enabled = undo.clone();
            models.committed = undo;
        }
    }

    /// Opens the key picker, or the password line when there is one key.
    pub fn apply_keys(&mut self, id: &str, token: u64, keys: Vec<ProviderKey>) -> PageEffect {
        if self.keys_token != Some(token) || !matches!(self.mode, Mode::List) {
            return PageEffect::none();
        }
        if self.selected().is_none_or(|row| row.id != id) {
            return PageEffect::none();
        }
        if keys.is_empty() {
            self.mode = Mode::List;
            return PageEffect::flash("no key to replace", false);
        }
        if keys.len() == 1 {
            let key_id = keys[0].id.clone();
            self.mode = Mode::Ask(secret_ask(
                "new key",
                AskThen::Replace {
                    id: id.to_owned(),
                    key_id,
                },
            ));
            return PageEffect::none();
        }
        self.mode = Mode::Keys {
            provider: id.to_owned(),
            cursor: 0,
            keys,
        };
        PageEffect::none()
    }

    /// Shows balance cards from a fresh quota read.
    pub fn apply_balances(&mut self, lines: Vec<String>) {
        self.balances = lines;
    }

    /// Provider id whose model picker is open.
    #[must_use]
    pub fn picker_provider(&self) -> Option<&str> {
        match &self.mode {
            Mode::Models(models) => Some(models.provider.as_str()),
            _ => None,
        }
    }

    /// Handles a key while this page is active.
    pub fn on_key(&mut self, key: KeyEvent) -> PageEffect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return PageEffect::none();
        }
        match &self.mode {
            Mode::List => self.on_list(key),
            Mode::Models(_) => self.on_models(key),
            Mode::Presets { .. } => self.on_presets(key),
            Mode::Keys { .. } => self.on_keys(key),
            Mode::Ask(_) => self.on_ask(key),
        }
    }

    /// Draws the page body.
    pub fn draw(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let lines = match &self.mode {
            Mode::List => self.list_lines(area.height, theme),
            Mode::Models(models) => model_lines(models, area.height, theme),
            Mode::Presets { cursor, filter } => picker_lines(
                "presets",
                &self.preset_labels(),
                *cursor,
                filter,
                area.height,
                theme,
            ),
            Mode::Keys { cursor, keys, .. } => {
                let labels: Vec<String> = keys
                    .iter()
                    .map(|key| format!("{}  {}", key.id, key.masked))
                    .collect();
                picker_lines("keys", &labels, *cursor, "", area.height, theme)
            }
            Mode::Ask(ask) => ask_lines(ask, theme),
        };
        Paragraph::new(lines).render(area, buf);
    }

    fn on_list(&mut self, key: KeyEvent) -> PageEffect {
        if !matches!(key.code, KeyCode::Char('d')) {
            self.confirm = false;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut self.cursor, self.rows.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut self.cursor, self.rows.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_models(),
            KeyCode::Char('m') => self.with_selected(|id| {
                PageEffect::mutation(
                    "refreshing models",
                    ControlIntent::RefreshModels { id, generation: 0 },
                )
            }),
            KeyCode::Char('t') => self.with_selected(|id| {
                PageEffect::mutation("testing provider", ControlIntent::TestProvider { id })
            }),
            KeyCode::Char('b') => PageEffect::read(ControlIntent::LoadBalances),
            KeyCode::Char('a') => {
                self.mode = Mode::Presets {
                    cursor: 0,
                    filter: String::new(),
                };
                PageEffect::none()
            }
            KeyCode::Char('e') => self.begin_replace(),
            KeyCode::Char('w') => self.begin_address(),
            KeyCode::Char('f') => self.begin_family(),
            KeyCode::Char('o') => self.patch_flag(true),
            KeyCode::Char('u') => self.patch_flag(false),
            KeyCode::Char('d') => self.delete_key(),
            _ => PageEffect::none(),
        }
    }

    fn on_models(&mut self, key: KeyEvent) -> PageEffect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
            let Some(id) = self.picker_provider().map(str::to_owned) else {
                return PageEffect::none();
            };
            return PageEffect::mutation(
                "refreshing models",
                ControlIntent::RefreshModels { id, generation: 0 },
            );
        }
        let Mode::Models(models) = &mut self.mode else {
            return PageEffect::none();
        };
        let visible = filtered(&models.ids, &models.filter);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut models.cursor, visible.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut models.cursor, visible.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_model(),
            KeyCode::Char('x') => self.reset_models(),
            KeyCode::Backspace => {
                models.filter.pop();
                models.cursor = 0;
                PageEffect::none()
            }
            KeyCode::Char(character) if key.modifiers.is_empty() && !character.is_control() => {
                models.filter.push(character);
                models.cursor = 0;
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn on_presets(&mut self, key: KeyEvent) -> PageEffect {
        let labels = self.preset_labels();
        let Mode::Presets { cursor, filter } = &mut self.mode else {
            return PageEffect::none();
        };
        let visible = filtered(&labels, filter);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(cursor, visible.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(cursor, visible.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Some(index) = visible.get(*cursor).copied() else {
                    return PageEffect::flash("no preset matches", false);
                };
                let preset = self.presets[index].clone();
                self.begin_create(preset)
            }
            KeyCode::Backspace => {
                filter.pop();
                *cursor = 0;
                PageEffect::none()
            }
            KeyCode::Char(character) if key.modifiers.is_empty() && !character.is_control() => {
                filter.push(character);
                *cursor = 0;
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn on_keys(&mut self, key: KeyEvent) -> PageEffect {
        let Mode::Keys {
            provider,
            cursor,
            keys,
        } = &mut self.mode
        else {
            return PageEffect::none();
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(cursor, keys.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(cursor, keys.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Some(key) = keys.get(*cursor) else {
                    return PageEffect::none();
                };
                let then = AskThen::Replace {
                    id: provider.clone(),
                    key_id: key.id.clone(),
                };
                self.mode = Mode::Ask(secret_ask("new key", then));
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn on_ask(&mut self, key: KeyEvent) -> PageEffect {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                PageEffect::flash("cancelled", false)
            }
            KeyCode::Backspace => {
                if let Mode::Ask(ask) = &mut self.mode {
                    ask.buffer.pop();
                }
                PageEffect::none()
            }
            KeyCode::Enter => self.submit_ask(),
            KeyCode::Char(character) if !character.is_control() => {
                if let Mode::Ask(ask) = &mut self.mode {
                    ask.buffer.push(character);
                }
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn open_models(&mut self) -> PageEffect {
        let Some(id) = self.selected().map(|row| row.id.clone()) else {
            return PageEffect::flash("no provider selected", false);
        };
        let token = self.next_token();
        self.models_token = Some(token);
        self.keys_token = None;
        PageEffect::read(ControlIntent::OpenModels {
            id,
            generation: 0,
            token,
        })
    }

    fn next_token(&mut self) -> u64 {
        self.read_token = self.read_token.saturating_add(1);
        self.read_token
    }

    fn begin_replace(&mut self) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no provider selected", false);
        };
        if is_account_row(&row) && !row.signed_out {
            return PageEffect::flash("signed-in accounts do not take an API key here", false);
        }
        if is_project_preset(&row, &self.presets) {
            return PageEffect::flash("set Vertex credentials with a, not a key", false);
        }
        let token = self.next_token();
        self.keys_token = Some(token);
        self.models_token = None;
        PageEffect::read(ControlIntent::LoadKeys { id: row.id, token })
    }

    fn begin_address(&mut self) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no provider selected", false);
        };
        if !address_is_editable(&row, &self.presets) {
            let text = if is_project_preset(&row, &self.presets) {
                "set the project with a"
            } else {
                "this provider uses a fixed address"
            };
            return PageEffect::flash(text, false);
        }
        self.mode = Mode::Ask(plain_ask(
            "address",
            row.base_url.clone(),
            AskThen::BaseUrl { id: row.id },
        ));
        PageEffect::none()
    }

    fn begin_family(&mut self) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no provider selected", false);
        };
        self.mode = Mode::Ask(plain_ask(
            "family",
            row.family.unwrap_or_default(),
            AskThen::Family { id: row.id },
        ));
        PageEffect::none()
    }

    fn begin_create(&mut self, preset: PresetEntry) -> PageEffect {
        let needs_address = preset.needs_address
            || matches!(
                preset.kind,
                PresetKind::Local | PresetKind::Remote | PresetKind::Custom
            );
        let mut draft = CreateDraft {
            preset: preset.id,
            kind: preset.kind,
            needs_key: preset.needs_key,
            needs_address,
            needs_project: preset.needs_project,
            option_keys: preset.option_keys,
            api_key: None,
            base_url: None,
            project: None,
            location: None,
            step: CreateStep::Key,
        };
        if draft.needs_key {
            draft.step = CreateStep::Key;
            self.mode = Mode::Ask(secret_ask("api key", AskThen::Create(draft)));
            return PageEffect::none();
        }
        self.advance_create(draft)
    }

    fn advance_create(&mut self, mut draft: CreateDraft) -> PageEffect {
        if draft.needs_project && draft.project.is_none() {
            draft.step = CreateStep::Project;
            self.mode = Mode::Ask(plain_ask("project", String::new(), AskThen::Create(draft)));
            return PageEffect::none();
        }
        if draft.needs_project && draft.location.is_none() {
            draft.step = CreateStep::Location;
            self.mode = Mode::Ask(plain_ask(
                "location (empty means global)",
                String::new(),
                AskThen::Create(draft),
            ));
            return PageEffect::none();
        }
        if draft.needs_address && draft.base_url.is_none() {
            draft.step = CreateStep::Address;
            self.mode = Mode::Ask(plain_ask("address", String::new(), AskThen::Create(draft)));
            return PageEffect::none();
        }
        self.submit_create(draft)
    }

    fn submit_create(&mut self, draft: CreateDraft) -> PageEffect {
        let Some(revision) = self.revision() else {
            self.mode = Mode::List;
            return PageEffect::flash("configuration revision is unknown; press r", false);
        };
        let mut options = BTreeMap::new();
        if let Some(project) = draft.project.clone() {
            options.insert("project".to_owned(), project);
        }
        if let Some(location) = draft.location.clone().filter(|value| !value.is_empty()) {
            options.insert("location".to_owned(), location);
        }
        options.retain(|key, _| draft.option_keys.is_empty() || draft.option_keys.contains(key));
        self.mode = Mode::List;
        PageEffect::mutation(
            "adding provider",
            ControlIntent::CreateProvider {
                preset: draft.preset,
                revision,
                name: None,
                base_url: draft.base_url,
                api_key: draft.api_key,
                options,
            },
        )
    }

    fn submit_ask(&mut self) -> PageEffect {
        let Mode::Ask(ask) = &self.mode else {
            return PageEffect::none();
        };
        let buffer = ask.buffer.clone();
        let then = ask.then.clone();
        match then {
            AskThen::Create(mut draft) => {
                match draft.step {
                    CreateStep::Key => {
                        if buffer.trim().is_empty() {
                            return PageEffect::flash("a key is required", false);
                        }
                        draft.api_key = Some(Secret::new(buffer));
                    }
                    CreateStep::Project => {
                        if buffer.trim().is_empty() {
                            return PageEffect::flash("a project is required", false);
                        }
                        draft.project = Some(buffer);
                    }
                    CreateStep::Location => {
                        draft.location = Some(buffer);
                    }
                    CreateStep::Address => {
                        if buffer.trim().is_empty() {
                            return PageEffect::flash("an address is required", false);
                        }
                        draft.base_url = Some(buffer);
                    }
                }
                self.advance_create(draft)
            }
            AskThen::Family { id } => {
                let Some(revision) = self.revision() else {
                    return PageEffect::flash("configuration revision is unknown; press r", false);
                };
                self.mode = Mode::List;
                PageEffect::mutation(
                    "saving family",
                    ControlIntent::PatchProvider {
                        id,
                        revision,
                        enabled: None,
                        listed: None,
                        family: Some(buffer),
                        base_url: None,
                    },
                )
            }
            AskThen::BaseUrl { id } => {
                if buffer.trim().is_empty() {
                    return PageEffect::flash("an address is required", false);
                }
                let Some(revision) = self.revision() else {
                    return PageEffect::flash("configuration revision is unknown; press r", false);
                };
                self.mode = Mode::List;
                PageEffect::mutation(
                    "saving address",
                    ControlIntent::PatchProvider {
                        id,
                        revision,
                        enabled: None,
                        listed: None,
                        family: None,
                        base_url: Some(buffer),
                    },
                )
            }
            AskThen::Replace { id, key_id } => {
                if buffer.trim().is_empty() {
                    return PageEffect::flash("a key is required", false);
                }
                self.mode = Mode::List;
                PageEffect::mutation(
                    "replacing key",
                    ControlIntent::ReplaceKey {
                        id,
                        key_id,
                        secret: Secret::new(buffer),
                    },
                )
            }
        }
    }

    fn patch_flag(&mut self, enabled_flag: bool) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no provider selected", false);
        };
        let Some(revision) = self.revision() else {
            return PageEffect::flash("configuration revision is unknown; press r", false);
        };
        if enabled_flag {
            PageEffect::mutation(
                "saving provider",
                ControlIntent::PatchProvider {
                    id: row.id,
                    revision,
                    enabled: Some(!row.enabled),
                    listed: None,
                    family: None,
                    base_url: None,
                },
            )
        } else {
            PageEffect::mutation(
                "saving provider",
                ControlIntent::PatchProvider {
                    id: row.id,
                    revision,
                    enabled: None,
                    listed: Some(!row.listed),
                    family: None,
                    base_url: None,
                },
            )
        }
    }

    fn delete_key(&mut self) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no provider selected", false);
        };
        if is_account_row(&row) {
            self.confirm = false;
            return PageEffect::flash(
                "sign out from Accounts; this page does not remove accounts",
                false,
            );
        }
        if !self.confirm {
            self.confirm = true;
            return PageEffect::flash(format!("press d again to delete {}", row.id), false);
        }
        self.confirm = false;
        let Some(revision) = self.revision() else {
            return PageEffect::flash("configuration revision is unknown; press r", false);
        };
        PageEffect::mutation(
            "deleting provider",
            ControlIntent::DeleteProvider {
                id: row.id,
                revision,
            },
        )
    }

    fn toggle_model(&mut self) -> PageEffect {
        let Mode::Models(models) = &mut self.mode else {
            return PageEffect::none();
        };
        if models.read_only {
            return PageEffect::flash(
                "model list is read-only until default exposure is known",
                false,
            );
        }
        let visible = filtered(&models.ids, &models.filter);
        let Some(index) = visible.get(models.cursor).copied() else {
            return PageEffect::none();
        };
        let id = models.ids[index].clone();
        let on = models.enabled.iter().any(|enabled| enabled == &id);
        if on && models.enabled.len() == 1 {
            return PageEffect::flash("keep at least one enabled model", false);
        }
        if models.revision.len() != REVISION_LEN {
            return PageEffect::flash("picks revision is unknown; press r", false);
        }
        models.undo = Some(models.committed.clone());
        if on {
            models.enabled.retain(|enabled| enabled != &id);
        } else {
            models.enabled.push(id);
        }
        models.committed = models.enabled.clone();
        let provider = models.provider.clone();
        let revision = models.revision.clone();
        let ids = models.enabled.clone();
        PageEffect::mutation(
            "saving picks",
            ControlIntent::ReplacePicks {
                id: provider,
                revision,
                ids,
            },
        )
    }

    fn reset_models(&mut self) -> PageEffect {
        let Mode::Models(models) = &self.mode else {
            return PageEffect::none();
        };
        if models.read_only {
            return PageEffect::flash(
                "model list is read-only until default exposure is known",
                false,
            );
        }
        if models.revision.len() != REVISION_LEN {
            return PageEffect::flash("picks revision is unknown; press r", false);
        }
        PageEffect::mutation(
            "resetting picks",
            ControlIntent::ResetPicks {
                id: models.provider.clone(),
                revision: models.revision.clone(),
            },
        )
    }

    fn with_selected(&self, build: impl FnOnce(String) -> PageEffect) -> PageEffect {
        match self.selected() {
            Some(row) => build(row.id.clone()),
            None => PageEffect::flash("no provider selected", false),
        }
    }

    fn selected(&self) -> Option<&ProviderSummary> {
        self.rows.get(self.cursor)
    }

    fn revision(&self) -> Option<String> {
        (self.disk_revision.len() == REVISION_LEN).then(|| self.disk_revision.clone())
    }

    fn preset_labels(&self) -> Vec<String> {
        self.presets
            .iter()
            .map(|preset| format!("{}  {}", preset.id, kind_label(preset.kind)))
            .collect()
    }

    fn list_lines(&self, height: u16, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if !self.balances.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  balances: {}", self.balances.join("  ")),
                theme.muted_style(),
            )));
        }
        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No providers yet. Press a to add one.".to_owned(),
                theme.muted_style(),
            )));
            return lines;
        }
        let window = list_window(height, 3, self.rows.len(), self.cursor);
        for (index, row) in self
            .rows
            .iter()
            .enumerate()
            .skip(window.start)
            .take(window.visible)
        {
            let marker = if index == self.cursor { "> " } else { "  " };
            let status = provider_status_line(row, &self.presets);
            let error = if row.list_error.is_empty() {
                String::new()
            } else {
                format!("  {}", row.list_error)
            };
            let text = format!(
                "{marker}{}  {status}  {} models{error}",
                row.id, row.model_count
            );
            let style = if index == self.cursor {
                theme.cursor_style()
            } else {
                theme.text_style()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        lines
    }
}

fn kind_label(kind: PresetKind) -> &'static str {
    match kind {
        PresetKind::Preset => "preset",
        PresetKind::Local => "local",
        PresetKind::Remote => "remote",
        PresetKind::Custom => "custom",
        _ => "preset",
    }
}

fn secret_ask(title: &str, then: AskThen) -> Ask {
    Ask {
        title: title.to_owned(),
        buffer: String::new(),
        secret: true,
        then,
    }
}

fn plain_ask(title: &str, buffer: String, then: AskThen) -> Ask {
    Ask {
        title: title.to_owned(),
        buffer,
        secret: false,
        then,
    }
}

fn move_cursor(cursor: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        *cursor = 0;
        return;
    }
    let next = *cursor as i32 + delta;
    *cursor = next.rem_euclid(len as i32) as usize;
}

fn filtered(labels: &[String], filter: &str) -> Vec<usize> {
    let filter = filter.to_ascii_lowercase();
    labels
        .iter()
        .enumerate()
        .filter(|(_, label)| filter.is_empty() || label.to_ascii_lowercase().contains(&filter))
        .map(|(index, _)| index)
        .collect()
}

fn model_lines(models: &Models, height: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "  models {}{}",
            models.provider,
            if models.read_only {
                "  (read-only)"
            } else {
                ""
            }
        ),
        theme.title_style(),
    ))];
    let visible = filtered(&models.ids, &models.filter);
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No models.".to_owned(),
            theme.muted_style(),
        )));
        return lines;
    }
    let window = list_window(height.saturating_sub(1), 3, visible.len(), models.cursor);
    for (offset, index) in visible
        .iter()
        .copied()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let id = &models.ids[index];
        let mark = if models.enabled.iter().any(|enabled| enabled == id) {
            "[x]"
        } else {
            "[ ]"
        };
        let marker = if offset == models.cursor { ">" } else { " " };
        let style = if offset == models.cursor {
            theme.cursor_style()
        } else {
            theme.text_style()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {mark} {id}"),
            style,
        )));
    }
    lines
}

fn picker_lines(
    title: &str,
    labels: &[String],
    cursor: usize,
    filter: &str,
    height: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("  {title}"),
        theme.title_style(),
    ))];
    let visible = filtered(labels, filter);
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Nothing matches.".to_owned(),
            theme.muted_style(),
        )));
        return lines;
    }
    let window = list_window(height.saturating_sub(1), 3, visible.len(), cursor);
    for (offset, index) in visible
        .iter()
        .copied()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let marker = if offset == cursor { "> " } else { "  " };
        let style = if offset == cursor {
            theme.cursor_style()
        } else {
            theme.text_style()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", labels[index]),
            style,
        )));
    }
    lines
}

impl fmt::Debug for Ask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buffer = if self.secret {
            format!("[redacted len={}]", self.buffer.chars().count())
        } else {
            self.buffer.clone()
        };
        formatter
            .debug_struct("Ask")
            .field("title", &self.title)
            .field("buffer", &buffer)
            .field("secret", &self.secret)
            .field("then", &self.then)
            .finish()
    }
}

fn ask_lines(ask: &Ask, theme: &Theme) -> Vec<Line<'static>> {
    let value = if ask.secret {
        "•".repeat(ask.buffer.chars().count())
    } else {
        ask.buffer.clone()
    };
    vec![
        Line::from(Span::styled(
            format!("  {}", ask.title),
            theme.title_style(),
        )),
        Line::from(Span::styled(format!("  {value}_"), theme.cursor_style())),
    ]
}

#[cfg(test)]
mod tests {
    use super::ProviderPage;
    use crate::intent::ControlIntent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use hofund_control_client::hofund_control_proto::{
        ModelEntry, PicksResponse, PresetEntry, PresetKind, ProviderSummary,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn character(value: char) -> KeyEvent {
        key(KeyCode::Char(value))
    }

    fn provider(id: &str) -> ProviderSummary {
        let mut summary = ProviderSummary::default();
        summary.id = id.to_owned();
        summary.enabled = true;
        summary.listed = true;
        summary.base_url = "https://example.test/v1".to_owned();
        summary
    }

    fn revision() -> String {
        "ab".repeat(32)
    }

    fn picks(ids: &[&str], exposed: &[&str]) -> PicksResponse {
        let mut picks = PicksResponse::default();
        picks.picks = ids.iter().map(|id| (*id).to_owned()).collect();
        picks.default_exposed = exposed.iter().map(|id| (*id).to_owned()).collect();
        picks.picks_revision = revision();
        picks
    }

    fn loaded() -> ProviderPage {
        let mut page = ProviderPage::default();
        page.apply_load(
            vec![provider("first"), provider("second")],
            vec![remote_preset()],
            revision(),
        );
        page
    }

    #[test]
    fn j_then_t_targets_the_second_provider() {
        let mut page = loaded();
        page.on_key(character('j'));
        let effect = page.on_key(character('t'));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::TestProvider { id }] if id == "second"
        ));
    }

    #[test]
    fn one_d_does_not_delete_and_the_second_sends_the_revision() {
        let mut page = loaded();
        let once = page.on_key(character('d'));
        assert!(once.intents.is_empty());
        assert!(once.flash.unwrap().0.contains("press d again"));
        let twice = page.on_key(character('d'));
        assert!(matches!(
            twice.intents.as_slice(),
            [ControlIntent::DeleteProvider { id, revision }]
                if id == "first" && revision.len() == 64
        ));
    }

    #[test]
    fn other_keys_clear_a_pending_delete() {
        let mut page = loaded();
        page.on_key(character('d'));
        page.on_key(character('j'));
        let effect = page.on_key(character('d'));
        assert!(effect.intents.is_empty());
    }

    #[test]
    fn empty_picks_without_default_exposure_do_not_replace() {
        let mut page = loaded();
        page.apply_models("first", &[model("a"), model("b")], &picks(&[], &[]), true);
        let effect = page.on_key(key(KeyCode::Enter));
        assert!(effect.intents.is_empty());
        assert!(effect.flash.unwrap().0.contains("read-only"));
    }

    #[test]
    fn read_only_picker_does_not_reset_picks() {
        let mut page = loaded();
        page.apply_models("first", &[model("a")], &picks(&[], &[]), true);
        let effect = page.on_key(character('x'));
        assert!(effect.intents.is_empty(), "{effect:?}");
        assert!(effect.flash.unwrap().0.contains("read-only"));
    }

    #[test]
    fn debug_output_omits_a_typed_key() {
        let mut page = loaded();
        page.on_key(character('a'));
        page.on_key(key(KeyCode::Enter));
        for character in ['s', 'e', 'c', 'r', 'e', 't'] {
            page.on_key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::empty(),
            ));
        }
        let while_typing = format!("{page:?}");
        assert!(
            !while_typing.contains("buffer: \"secret\""),
            "{while_typing}"
        );
        assert!(while_typing.contains("[redacted len=6]"), "{while_typing}");
        page.on_key(key(KeyCode::Enter));
        let while_address = format!("{page:?}");
        assert!(
            !while_address.contains("buffer: \"secret\"")
                && !while_address.contains("api_key: Some(\"secret\")"),
            "{while_address}"
        );
        assert!(
            while_address.contains("[redacted len=6]"),
            "{while_address}"
        );
    }

    #[test]
    fn toggle_sends_the_full_enabled_list_and_keeps_the_last_model() {
        let mut page = loaded();
        page.apply_models(
            "first",
            &[model("a"), model("b")],
            &picks(&["a", "b"], &[]),
            true,
        );
        let effect = page.on_key(key(KeyCode::Enter));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::ReplacePicks { ids, .. }] if ids == &["b".to_owned()]
        ));
        page.on_key(character('j'));
        let blocked = page.on_key(key(KeyCode::Enter));
        assert!(blocked.intents.is_empty());
        assert!(blocked.flash.unwrap().0.contains("at least one"));
    }

    #[test]
    fn default_exposure_can_be_narrowed() {
        let mut page = loaded();
        page.apply_models(
            "first",
            &[model("a"), model("b")],
            &picks(&[], &["a", "b"]),
            true,
        );
        page.on_key(character('j'));
        let effect = page.on_key(key(KeyCode::Enter));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::ReplacePicks { ids, .. }] if ids == &["a".to_owned()]
        ));
    }

    #[test]
    fn remote_preset_asks_for_a_key_then_an_address() {
        let mut page = loaded();
        page.on_key(character('a'));
        page.on_key(key(KeyCode::Enter));
        page.on_key(character('s'));
        page.on_key(character('e'));
        page.on_key(character('c'));
        page.on_key(character('r'));
        page.on_key(character('e'));
        page.on_key(character('t'));
        assert!(page.on_key(key(KeyCode::Enter)).intents.is_empty());
        for character in [
            'h', 't', 't', 'p', ':', '/', '/', 'g', 'w', '.', 't', 'e', 's', 't',
        ] {
            page.on_key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::empty(),
            ));
        }
        let effect = page.on_key(key(KeyCode::Enter));
        let ControlIntent::CreateProvider {
            preset,
            base_url,
            api_key,
            ..
        } = &effect.intents[0]
        else {
            panic!("expected create");
        };
        assert_eq!(preset, "remote-hofund");
        assert_eq!(base_url.as_deref(), Some("http://gw.test"));
        assert_eq!(
            api_key.as_ref().map(|secret| secret.expose()),
            Some("secret")
        );
        assert!(!format!("{effect:?}").contains("secret"));
    }

    #[test]
    fn vertex_and_signed_in_accounts_refuse_key_replace() {
        let mut vertex = provider("vertex");
        vertex.preset = Some("google-vertex".to_owned());
        let mut account = provider("acct");
        account.kind = "account".to_owned();
        account.account_label = "ada".to_owned();
        let mut page = ProviderPage::default();
        page.apply_load(vec![vertex, account], Vec::new(), revision());
        let blocked = page.on_key(character('e'));
        assert!(blocked.intents.is_empty());
        page.on_key(character('j'));
        let account_blocked = page.on_key(character('e'));
        assert!(account_blocked.flash.unwrap().0.contains("signed-in"));
    }

    fn remote_preset() -> PresetEntry {
        let mut preset = PresetEntry::default();
        preset.id = "remote-hofund".to_owned();
        preset.name = "Remote hofund".to_owned();
        preset.kind = PresetKind::Remote;
        preset.needs_key = true;
        preset.needs_address = true;
        preset
    }

    fn model(id: &str) -> ModelEntry {
        let mut model = ModelEntry::default();
        model.id = id.to_owned();
        model
    }
}
