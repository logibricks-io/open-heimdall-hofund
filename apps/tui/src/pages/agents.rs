//! Agents page: model, visibility, connection, profiles, and catalog sync.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    AgentAction, AgentState, AgentStatusEntry, AgentSummary, ProfileSummary,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::intent::{ControlIntent, PageEffect};
use crate::theme::Theme;
use crate::ui::list_window;

/// Interactive agents screen.
#[derive(Clone, Debug, Default)]
pub struct AgentPage {
    loaded: bool,
    cursor: usize,
    field: Field,
    rows: Vec<AgentRow>,
    mode: AgentMode,
    profiles: Vec<ProfileSummary>,
}

#[derive(Clone, Debug)]
struct AgentRow {
    summary: AgentSummary,
    state: AgentState,
    model: Option<String>,
    shown: Option<Vec<String>>,
    hidden: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Field {
    #[default]
    Model,
    Visibility,
    Connection,
}

#[derive(Clone, Debug, Default)]
enum AgentMode {
    #[default]
    List,
    ModelPicker {
        agent: String,
        cursor: usize,
        filter: String,
        options: Vec<String>,
    },
    Visibility {
        agent: String,
        cursor: usize,
        ids: Vec<String>,
        hidden: Vec<String>,
    },
    Confirm {
        agent: String,
        action: AgentAction,
        revision: Option<String>,
        lines: Vec<String>,
    },
    Profiles {
        cursor: usize,
        filter: String,
    },
    Ask {
        title: String,
        buffer: String,
        then: AgentAsk,
    },
}

#[derive(Clone, Debug)]
enum AgentAsk {
    ProfileName,
    Rename { from: String },
}

impl AgentPage {
    /// Whether a control load has been applied.
    #[must_use]
    pub fn active(&self) -> bool {
        self.loaded
    }

    /// Whether this page owns keys that would otherwise quit or change pages.
    #[must_use]
    pub fn captures(&self) -> bool {
        !matches!(self.mode, AgentMode::List)
    }

    /// Footer lines while this page is active.
    #[must_use]
    pub fn hint_lines(&self) -> (&'static str, &'static str) {
        match &self.mode {
            AgentMode::List => (
                "j/k agent   h/l field   enter change   S sync catalog   s save profile",
                "p profiles   r reload",
            ),
            AgentMode::ModelPicker { .. } => (
                "j/k model   type to filter   enter apply   esc back",
                "a typed name that is not listed is sent as a custom model",
            ),
            AgentMode::Visibility { .. } => (
                "j/k row   enter toggle   esc back",
                "the first two rows set picked-only",
            ),
            AgentMode::Confirm { .. } => ("enter apply the plan   esc cancel", ""),
            AgentMode::Profiles { .. } => (
                "j/k profile   enter switch   ctrl+e rename   ctrl+d delete   esc back",
                "",
            ),
            AgentMode::Ask { .. } => ("type a name   enter save   esc cancel", ""),
        }
    }

    /// Applies agents and status. Hidden and not-installed rows sort last.
    pub fn apply_load(&mut self, agents: Vec<AgentSummary>, status: Vec<AgentStatusEntry>) {
        let selected = self.rows.get(self.cursor).map(|row| row.summary.id.clone());
        let mut rows: Vec<AgentRow> = agents
            .into_iter()
            .map(|summary| {
                let found = status.iter().find(|entry| entry.id == summary.id);
                AgentRow {
                    summary,
                    state: found.map(|entry| entry.state).unwrap_or_default(),
                    model: found.and_then(|entry| entry.model.clone()),
                    shown: None,
                    hidden: None,
                }
            })
            .collect();
        rows.sort_by_key(|row| u8::from(dimmed(row)));
        self.cursor = selected
            .and_then(|id| rows.iter().position(|row| row.summary.id == id))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.loaded = true;
    }

    /// Stores route model ids and opens the picker when this agent asked.
    pub fn apply_route_models(&mut self, agent: &str, ids: Vec<String>) {
        if let AgentMode::ModelPicker {
            agent: current,
            options,
            cursor,
            ..
        } = &mut self.mode
            && current == agent
        {
            *options = ids;
            *cursor = 0;
        }
    }

    /// Stores visibility and opens the picker when this agent asked.
    pub fn apply_visibility(&mut self, agent: &str, shown: Vec<String>, hidden: Vec<String>) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.summary.id == agent) {
            row.shown = Some(shown.clone());
            row.hidden = Some(hidden.clone());
        }
        let waiting =
            matches!(&self.mode, AgentMode::Visibility { agent: current, .. } if current == agent);
        if waiting {
            let mut ids = shown;
            for id in &hidden {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            self.mode = AgentMode::Visibility {
                agent: agent.to_owned(),
                cursor: 0,
                ids,
                hidden,
            };
        }
    }

    /// Opens the plan confirmation.
    pub fn apply_plan(
        &mut self,
        agent: &str,
        action: AgentAction,
        revision: String,
        lines: Vec<String>,
    ) {
        if self.selected().is_some_and(|row| row.summary.id == agent) {
            self.mode = AgentMode::Confirm {
                agent: agent.to_owned(),
                action,
                revision: (!revision.is_empty()).then_some(revision),
                lines,
            };
        }
    }

    /// Stores profiles and opens the picker.
    pub fn apply_profiles(&mut self, profiles: Vec<ProfileSummary>, open: bool) {
        self.profiles = profiles;
        if open {
            self.mode = AgentMode::Profiles {
                cursor: 0,
                filter: String::new(),
            };
        }
    }

    /// Whether the loaded list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.loaded && self.rows.is_empty()
    }

    /// Handles a key while this page is active.
    pub fn on_key(&mut self, key: KeyEvent) -> PageEffect {
        match &self.mode {
            AgentMode::List => self.on_list(key),
            AgentMode::ModelPicker { .. } => self.on_model_picker(key),
            AgentMode::Visibility { .. } => self.on_visibility(key),
            AgentMode::Confirm { .. } => self.on_confirm(key),
            AgentMode::Profiles { .. } => self.on_profiles(key),
            AgentMode::Ask { .. } => self.on_ask(key),
        }
    }

    /// Draws the page body.
    pub fn draw(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let lines = match &self.mode {
            AgentMode::List => self.list_lines(area.height, theme),
            AgentMode::ModelPicker {
                agent,
                cursor,
                filter,
                options,
            } => model_lines(agent, options, *cursor, filter, area.height, theme),
            AgentMode::Visibility {
                agent,
                cursor,
                ids,
                hidden,
            } => visibility_lines(agent, ids, hidden, *cursor, area.height, theme),
            AgentMode::Confirm {
                agent,
                action,
                lines,
                ..
            } => confirm_lines(agent, *action, lines, theme),
            AgentMode::Profiles { cursor, filter } => {
                profile_lines(&self.profiles, *cursor, filter, area.height, theme)
            }
            AgentMode::Ask { title, buffer, .. } => vec![
                Line::from(Span::styled(format!("  {title}"), theme.title_style())),
                Line::from(Span::styled(format!("  {buffer}_"), theme.cursor_style())),
            ],
        };
        Paragraph::new(lines).render(area, buf);
    }

    fn on_list(&mut self, key: KeyEvent) -> PageEffect {
        let catalog = matches!(key.code, KeyCode::Char('S'))
            || (matches!(key.code, KeyCode::Char('s'))
                && key.modifiers.contains(KeyModifiers::SHIFT));
        if catalog {
            let mut effect = PageEffect::read(ControlIntent::CatalogSync { force: true });
            effect.catalog_sync = true;
            effect.pending = None;
            return effect;
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
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab
                if !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.field = self.field.next();
                PageEffect::none()
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                self.field = self.field.prev();
                PageEffect::none()
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_field(),
            KeyCode::Char('s') => {
                self.mode = AgentMode::Ask {
                    title: "profile name".to_owned(),
                    buffer: String::new(),
                    then: AgentAsk::ProfileName,
                };
                PageEffect::none()
            }
            KeyCode::Char('p') => {
                self.mode = AgentMode::Profiles {
                    cursor: 0,
                    filter: String::new(),
                };
                PageEffect::read(ControlIntent::LoadProfiles)
            }
            _ => PageEffect::none(),
        }
    }

    fn activate_field(&mut self) -> PageEffect {
        let Some(row) = self.selected().cloned() else {
            return PageEffect::flash("no agent selected", false);
        };
        match self.field {
            Field::Model => {
                self.mode = AgentMode::ModelPicker {
                    agent: row.summary.id.clone(),
                    cursor: 0,
                    filter: String::new(),
                    options: Vec::new(),
                };
                PageEffect::read(ControlIntent::LoadRouteModels {
                    agent: row.summary.id,
                    generation: 0,
                })
            }
            Field::Visibility => {
                self.mode = AgentMode::Visibility {
                    agent: row.summary.id.clone(),
                    cursor: 0,
                    ids: Vec::new(),
                    hidden: row.hidden.clone().unwrap_or_default(),
                };
                PageEffect::read(ControlIntent::LoadAgentModels {
                    agent: row.summary.id,
                    generation: 0,
                })
            }
            Field::Connection => {
                let action = if row.state == AgentState::Connected {
                    AgentAction::Disconnect
                } else {
                    AgentAction::Connect
                };
                PageEffect::mutation(
                    "planning",
                    ControlIntent::PlanAgent {
                        agent: row.summary.id,
                        action,
                    },
                )
            }
        }
    }

    fn on_model_picker(&mut self, key: KeyEvent) -> PageEffect {
        let AgentMode::ModelPicker {
            agent,
            cursor,
            filter,
            options,
        } = &mut self.mode
        else {
            return PageEffect::none();
        };
        let choices = model_choices(options, filter);
        match key.code {
            KeyCode::Esc => {
                self.mode = AgentMode::List;
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(cursor, choices.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(cursor, choices.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Some(model) = choices.get(*cursor).cloned() else {
                    return PageEffect::flash("no model selected", false);
                };
                let agent = agent.clone();
                self.mode = AgentMode::List;
                PageEffect::mutation(
                    "saving model",
                    ControlIntent::SetAgentModel {
                        agent,
                        model: Some(model),
                    },
                )
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

    fn on_visibility(&mut self, key: KeyEvent) -> PageEffect {
        let AgentMode::Visibility {
            agent,
            cursor,
            ids,
            hidden,
        } = &mut self.mode
        else {
            return PageEffect::none();
        };
        let len = ids.len() + 2;
        match key.code {
            KeyCode::Esc => {
                self.mode = AgentMode::List;
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(cursor, len, 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(cursor, len, -1);
                PageEffect::none()
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let agent_id = agent.clone();
                if *cursor == 0 {
                    self.mode = AgentMode::List;
                    return PageEffect::mutation(
                        "saving visibility",
                        ControlIntent::SetPickedOnly {
                            agent: agent_id,
                            only: true,
                        },
                    );
                }
                if *cursor == 1 {
                    self.mode = AgentMode::List;
                    return PageEffect::mutation(
                        "saving visibility",
                        ControlIntent::SetPickedOnly {
                            agent: agent_id,
                            only: false,
                        },
                    );
                }
                let Some(id) = ids.get(*cursor - 2).cloned() else {
                    return PageEffect::none();
                };
                if hidden.iter().any(|hidden_id| hidden_id == &id) {
                    hidden.retain(|hidden_id| hidden_id != &id);
                } else {
                    hidden.push(id);
                }
                let ids = hidden.clone();
                PageEffect::mutation(
                    "saving visibility",
                    ControlIntent::SetHiddenModels {
                        agent: agent_id,
                        ids,
                    },
                )
            }
            _ => PageEffect::none(),
        }
    }

    fn on_confirm(&mut self, key: KeyEvent) -> PageEffect {
        match key.code {
            KeyCode::Esc => {
                self.mode = AgentMode::List;
                PageEffect::flash("cancelled", false)
            }
            KeyCode::Enter => {
                let AgentMode::Confirm {
                    agent,
                    action,
                    revision,
                    ..
                } = &self.mode
                else {
                    return PageEffect::none();
                };
                let agent = agent.clone();
                let revision = revision.clone();
                let action = *action;
                self.mode = AgentMode::List;
                let intent = if action == AgentAction::Disconnect {
                    ControlIntent::DisconnectAgent { agent, revision }
                } else {
                    ControlIntent::ConnectAgent { agent, revision }
                };
                PageEffect::mutation("applying", intent)
            }
            _ => PageEffect::none(),
        }
    }

    fn on_profiles(&mut self, key: KeyEvent) -> PageEffect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('d')) {
            let Some(name) = self.profile_at_cursor() else {
                return PageEffect::flash("no profile selected", false);
            };
            return PageEffect::mutation("deleting profile", ControlIntent::DeleteProfile { name });
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('e')) {
            let Some(from) = self.profile_at_cursor() else {
                return PageEffect::flash("no profile selected", false);
            };
            self.mode = AgentMode::Ask {
                title: "new profile name".to_owned(),
                buffer: String::new(),
                then: AgentAsk::Rename { from },
            };
            return PageEffect::none();
        }
        let labels = self.profile_labels();
        let names: Vec<String> = self
            .profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect();
        let AgentMode::Profiles { cursor, filter } = &self.mode else {
            return PageEffect::none();
        };
        let mut cursor = *cursor;
        let mut filter = filter.clone();
        let visible = filtered(&labels, &filter);
        let effect = match key.code {
            KeyCode::Esc => {
                self.mode = AgentMode::List;
                return PageEffect::none();
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('n')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || !matches!(key.code, KeyCode::Char('n')) =>
            {
                move_cursor(&mut cursor, visible.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || !matches!(key.code, KeyCode::Char('p')) =>
            {
                move_cursor(&mut cursor, visible.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Some(name) = visible
                    .get(cursor)
                    .and_then(|index| names.get(*index))
                    .cloned()
                else {
                    return PageEffect::flash("no profile selected", false);
                };
                self.mode = AgentMode::List;
                return PageEffect::mutation(
                    "switching profile",
                    ControlIntent::SwitchProfile { name },
                );
            }
            KeyCode::Backspace => {
                filter.pop();
                cursor = 0;
                PageEffect::none()
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty()
                    && !character.is_control()
                    && character != 'j'
                    && character != 'k' =>
            {
                filter.push(character);
                cursor = 0;
                PageEffect::none()
            }
            _ => PageEffect::none(),
        };
        if let AgentMode::Profiles {
            cursor: slot,
            filter: text,
        } = &mut self.mode
        {
            *slot = cursor;
            *text = filter;
        }
        effect
    }

    fn on_ask(&mut self, key: KeyEvent) -> PageEffect {
        match key.code {
            KeyCode::Esc => {
                self.mode = AgentMode::List;
                PageEffect::flash("cancelled", false)
            }
            KeyCode::Backspace => {
                if let AgentMode::Ask { buffer, .. } = &mut self.mode {
                    buffer.pop();
                }
                PageEffect::none()
            }
            KeyCode::Enter => {
                let AgentMode::Ask { buffer, then, .. } = &self.mode else {
                    return PageEffect::none();
                };
                let name = buffer.trim().to_owned();
                if name.is_empty() {
                    return PageEffect::flash("a name is required", false);
                }
                let then = then.clone();
                self.mode = AgentMode::List;
                match then {
                    AgentAsk::ProfileName => {
                        PageEffect::mutation("saving profile", ControlIntent::SaveProfile { name })
                    }
                    AgentAsk::Rename { from } => PageEffect::mutation(
                        "renaming profile",
                        ControlIntent::RenameProfile { from, to: name },
                    ),
                }
            }
            KeyCode::Char(character) if !character.is_control() => {
                if let AgentMode::Ask { buffer, .. } = &mut self.mode {
                    buffer.push(character);
                }
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn selected(&self) -> Option<&AgentRow> {
        self.rows.get(self.cursor)
    }

    fn profile_labels(&self) -> Vec<String> {
        self.profiles
            .iter()
            .map(|profile| {
                if profile.in_use {
                    format!("{}  in use", profile.name)
                } else {
                    profile.name.clone()
                }
            })
            .collect()
    }

    fn profile_at_cursor(&self) -> Option<String> {
        let AgentMode::Profiles { cursor, filter } = &self.mode else {
            return None;
        };
        let labels = self.profile_labels();
        let visible = filtered(&labels, filter);
        visible
            .get(*cursor)
            .and_then(|index| self.profiles.get(*index))
            .map(|profile| profile.name.clone())
    }

    fn list_lines(&self, height: u16, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::from(Span::styled(
                "  No agents were detected.".to_owned(),
                theme.muted_style(),
            ))];
        }
        let window = list_window(height, 3, self.rows.len(), self.cursor);
        self.rows
            .iter()
            .enumerate()
            .skip(window.start)
            .take(window.visible)
            .map(|(index, row)| {
                let marker = if index == self.cursor { "> " } else { "  " };
                let style = if index == self.cursor {
                    theme.cursor_style()
                } else if dimmed(row) {
                    theme.muted_style()
                } else {
                    theme.text_style()
                };
                Line::from(Span::styled(
                    format!("{marker}{}", row_text(row, self.field)),
                    style,
                ))
            })
            .collect()
    }
}

impl Field {
    fn next(self) -> Self {
        match self {
            Self::Model => Self::Visibility,
            Self::Visibility => Self::Connection,
            Self::Connection => Self::Model,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Model => Self::Connection,
            Self::Visibility => Self::Model,
            Self::Connection => Self::Visibility,
        }
    }
}

fn dimmed(row: &AgentRow) -> bool {
    !row.summary.installed
        || matches!(&row.shown, Some(shown) if shown.is_empty() && row.hidden.as_ref().is_some_and(|hidden| !hidden.is_empty()))
}

fn row_text(row: &AgentRow, field: Field) -> String {
    let model = row.model.as_deref().unwrap_or("default");
    let visibility = match (&row.shown, &row.hidden) {
        (Some(shown), Some(hidden)) if hidden.is_empty() && !shown.is_empty() => "shown",
        (Some(_), Some(hidden)) if !hidden.is_empty() => "hidden",
        _ => "…",
    };
    let state = match row.state {
        AgentState::Connected => "connected",
        AgentState::Disconnected => "disconnected",
        AgentState::Drifted => "drifted",
        _ => "unknown",
    };
    let (model, visibility, state) = (
        mark(field == Field::Model, model),
        mark(field == Field::Visibility, visibility),
        mark(field == Field::Connection, state),
    );
    format!(
        "{}  model:{model}  visibility:{visibility}  connection:{state}",
        row.summary.display_name
    )
}

fn mark(selected: bool, value: &str) -> String {
    if selected {
        format!("[{value}]")
    } else {
        value.to_owned()
    }
}

fn model_choices(options: &[String], filter: &str) -> Vec<String> {
    let filter_lower = filter.to_ascii_lowercase();
    let mut choices: Vec<String> = options
        .iter()
        .filter(|option| {
            filter_lower.is_empty() || option.to_ascii_lowercase().contains(&filter_lower)
        })
        .cloned()
        .collect();
    if !filter.is_empty() && !choices.iter().any(|option| option == filter) {
        choices.push(filter.to_owned());
    }
    choices
}

fn model_lines(
    agent: &str,
    options: &[String],
    cursor: usize,
    filter: &str,
    height: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let choices = model_choices(options, filter);
    let mut lines = vec![Line::from(Span::styled(
        format!("  model for {agent}"),
        theme.title_style(),
    ))];
    if choices.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Loading models…".to_owned(),
            theme.muted_style(),
        )));
        return lines;
    }
    push_window(&mut lines, &choices, cursor, height, theme);
    lines
}

fn visibility_lines(
    agent: &str,
    ids: &[String],
    hidden: &[String],
    cursor: usize,
    height: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut labels = vec![
        "show picked models only".to_owned(),
        "show every listed model".to_owned(),
    ];
    for id in ids {
        let mark = if hidden.iter().any(|hidden_id| hidden_id == id) {
            "[ ]"
        } else {
            "[x]"
        };
        labels.push(format!("{mark} {id}"));
    }
    let mut lines = vec![Line::from(Span::styled(
        format!("  visibility for {agent}"),
        theme.title_style(),
    ))];
    push_window(&mut lines, &labels, cursor, height, theme);
    lines
}

fn confirm_lines(
    agent: &str,
    action: AgentAction,
    body: &[String],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let verb = if action == AgentAction::Disconnect {
        "disconnect"
    } else {
        "connect"
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("  {verb} {agent}?"),
        theme.title_style(),
    ))];
    if body.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No file changes in the plan.".to_owned(),
            theme.muted_style(),
        )));
    }
    for line in body.iter().take(12) {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            theme.text_style(),
        )));
    }
    lines
}

fn profile_lines(
    profiles: &[ProfileSummary],
    cursor: usize,
    filter: &str,
    height: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let labels: Vec<String> = profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let visible = filtered(&labels, filter);
    let shown: Vec<String> = visible
        .iter()
        .filter_map(|index| labels.get(*index).cloned())
        .collect();
    let mut lines = vec![Line::from(Span::styled(
        "  profiles".to_owned(),
        theme.title_style(),
    ))];
    if shown.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No profiles.".to_owned(),
            theme.muted_style(),
        )));
        return lines;
    }
    push_window(&mut lines, &shown, cursor, height, theme);
    lines
}

fn push_window(
    lines: &mut Vec<Line<'static>>,
    labels: &[String],
    cursor: usize,
    height: u16,
    theme: &Theme,
) {
    let window = list_window(height.saturating_sub(1), 3, labels.len(), cursor);
    for (offset, label) in labels
        .iter()
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
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }
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

fn move_cursor(cursor: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        *cursor = 0;
        return;
    }
    let next = *cursor as i32 + delta;
    *cursor = next.rem_euclid(len as i32) as usize;
}

#[cfg(test)]
mod tests {
    use super::AgentPage;
    use crate::intent::ControlIntent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use hofund_control_client::hofund_control_proto::{
        AgentAction, AgentState, AgentStatusEntry, AgentSummary,
    };

    fn character(value: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(value), KeyModifiers::empty())
    }

    fn agent(id: &str, installed: bool) -> AgentSummary {
        let mut summary = AgentSummary::default();
        summary.id = id.to_owned();
        summary.display_name = id.to_owned();
        summary.installed = installed;
        summary
    }

    fn status(id: &str, state: AgentState, model: &str) -> AgentStatusEntry {
        let mut entry = AgentStatusEntry::default();
        entry.id = id.to_owned();
        entry.state = state;
        entry.model = Some(model.to_owned());
        entry
    }

    fn loaded() -> AgentPage {
        let mut page = AgentPage::default();
        page.apply_load(
            vec![agent("claude", true), agent("missing", false)],
            vec![status("claude", AgentState::Disconnected, "route")],
        );
        page
    }

    #[test]
    fn connection_field_plans_before_connect() {
        let mut page = loaded();
        page.on_key(character('l'));
        page.on_key(character('l'));
        let effect = page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::PlanAgent { agent, action }]
                if agent == "claude" && *action == AgentAction::Connect
        ));
        page.apply_plan(
            "claude",
            AgentAction::Connect,
            "rev".to_owned(),
            vec!["settings.json".to_owned()],
        );
        let apply = page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(
            apply.intents.as_slice(),
            [ControlIntent::ConnectAgent { revision: Some(revision), .. }] if revision == "rev"
        ));
    }

    #[test]
    fn shift_s_syncs_and_plain_s_saves_a_profile() {
        let mut page = loaded();
        let sync = page.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert!(sync.catalog_sync);
        assert!(matches!(
            sync.intents.as_slice(),
            [ControlIntent::CatalogSync { force: true }]
        ));
        page.on_key(character('s'));
        page.on_key(character('w'));
        let save = page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(
            save.intents.as_slice(),
            [ControlIntent::SaveProfile { name }] if name == "w"
        ));
    }

    #[test]
    fn custom_model_text_is_sent() {
        let mut page = loaded();
        page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        page.apply_route_models("claude", vec!["alpha".to_owned()]);
        page.on_key(character('z'));
        page.on_key(character('e'));
        page.on_key(character('d'));
        let effect = page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::SetAgentModel { model: Some(model), .. }] if model == "zed"
        ));
    }

    #[test]
    fn not_installed_agents_sort_last() {
        let mut page = AgentPage::default();
        page.apply_load(
            vec![agent("missing", false), agent("claude", true)],
            Vec::new(),
        );
        page.on_key(character('t'));
        let effect = page.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(
            effect.intents.as_slice(),
            [ControlIntent::LoadRouteModels { agent, .. }] if agent == "claude"
        ));
    }
}
