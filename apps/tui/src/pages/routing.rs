//! Routing groups, group editing, member pickers, and route traces.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_proto::{
    API_VERSION, Group, GroupCreateRequest, GroupListResponse, GroupMember, GroupMemberCandidate,
    GroupMemberCandidatesResponse, GroupMemberWire, GroupPatchRequest,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::{Page, PageKeyOutcome};
use crate::intent::{ControlIntent, PageEffect};
use crate::session::RouteSnapshot;
use crate::state::PageId;
use crate::theme::Theme;
use crate::ui::list_window;

const REVISION_LEN: usize = 64;
const ROUTINGS: &[&str] = &[
    "smart", "order", "rotate", "usage", "pace", "weight", "free", "fast", "cheap", "drain",
];
const STAYS: &[&str] = &["auto", "session", "turn", "off", "strict"];
const EFFORTS: &[&str] = &["", "low", "medium", "high", "xhigh", "max"];
const RULE_HINT: &str = "use=<model> tokens=200k images effort=high agents=<id> intent=\"a quick question\" · the daemon parses the line";

/// Interactive Routing page.
#[derive(Clone, Debug)]
pub struct RoutingPage {
    loaded: bool,
    loading: bool,
    load_error: Option<String>,
    groups: Vec<Group>,
    disk_revision: String,
    cursor: usize,
    group_cursor: usize,
    confirm: Option<String>,
    read_generation: u64,
    candidate_token: u64,
    candidate_request: Option<u64>,
    mode: RoutingMode,
}

#[derive(Clone, Debug, Default)]
enum RoutingMode {
    #[default]
    List,
    Group {
        id: String,
    },
    Picker(Picker),
    Ask(Ask),
    Route(Box<RouteState>),
}

#[derive(Clone, Debug)]
struct Picker {
    target: PickerTarget,
    items: Vec<GroupMemberCandidate>,
    cursor: usize,
    filter: String,
}

#[derive(Clone, Debug)]
enum PickerTarget {
    Create { name: String },
    AddMember { id: String },
    Classifier { id: String },
}

#[derive(Clone, Debug)]
struct Ask {
    title: String,
    buffer: String,
    then: AskThen,
}

#[derive(Clone, Debug)]
enum AskThen {
    Create,
    Rule { id: String, index: Option<usize> },
    Family { id: String },
    Context { id: String },
    Levels { id: String },
    Rename { id: String },
}

#[derive(Clone, Debug, Default)]
struct RouteState {
    snapshot: Option<RouteSnapshot>,
    cursor: usize,
}

impl Default for RoutingPage {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingPage {
    /// Empty page before the first group read.
    #[must_use]
    pub fn new() -> Self {
        Self {
            loaded: false,
            loading: true,
            load_error: None,
            groups: Vec::new(),
            disk_revision: String::new(),
            cursor: 0,
            group_cursor: 0,
            confirm: None,
            read_generation: 0,
            candidate_token: 0,
            candidate_request: None,
            mode: RoutingMode::List,
        }
    }

    /// Whether this page owns keys that would otherwise quit or reload.
    #[must_use]
    pub fn captures(&self) -> bool {
        !matches!(self.mode, RoutingMode::List)
    }

    /// Whether an existing group is open.
    #[must_use]
    pub fn group_open(&self) -> bool {
        matches!(self.mode, RoutingMode::Group { .. })
    }

    /// Loaded group rows, visible groups first.
    #[must_use]
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Footer lines for the current Routing mode.
    #[must_use]
    pub fn hint_lines(&self) -> (&'static str, &'static str) {
        match &self.mode {
            RoutingMode::List => (
                "j/k group   enter open   n new   o strategy   dd delete   u restore",
                "v route view   r reload   1-6 pages   ? help   q quit",
            ),
            RoutingMode::Group { .. } => (
                "j/k member/rule   J/K move   a add   n/e rule   dd remove   o/s strategy/stays",
                "E effort   F fast   c classifier   f/x/l fields   R rename   esc/q back",
            ),
            RoutingMode::Picker(_) => ("j/k row   type to filter   enter select   esc back", ""),
            RoutingMode::Ask(_) => ("type a value   enter save   esc cancel", ""),
            RoutingMode::Route(_) => ("j/k decision   esc/q back", ""),
        }
    }

    /// Last fetch generation recorded.
    #[must_use]
    pub fn fetch_gen(&self) -> u64 {
        self.read_generation
    }

    /// Marks a new group read.
    pub fn reload(&mut self, generation: u64) {
        self.read_generation = generation;
        self.loading = true;
        self.load_error = None;
    }

    /// Applies the group list and keeps the selected ids stable.
    pub fn apply_load(&mut self, response: GroupListResponse) {
        let selected = self.selected().map(|group| group.id.clone());
        let open = self.open_id().map(str::to_owned);
        let mut groups = response.groups;
        groups.sort_by_key(|group| group.hidden);
        self.groups = groups;
        self.disk_revision = response.disk_revision;
        self.loaded = true;
        self.loading = false;
        self.load_error = None;
        self.cursor = selected
            .and_then(|id| self.groups.iter().position(|group| group.id == id))
            .unwrap_or(0)
            .min(self.groups.len().saturating_sub(1));
        if let Some(id) = open {
            let visible = self
                .groups
                .iter()
                .any(|group| group.id == id && !group.hidden);
            if !visible {
                self.mode = RoutingMode::List;
            }
        }
        self.clamp_group_cursor();
    }

    /// Records a failed group read.
    pub fn apply_load_error(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Stores a candidate response if it belongs to the open picker.
    pub fn apply_candidates(&mut self, token: u64, response: GroupMemberCandidatesResponse) {
        if self.candidate_request != Some(token) {
            return;
        }
        let RoutingMode::Picker(picker) = &self.mode else {
            return;
        };
        let items = match &picker.target {
            PickerTarget::Create { .. } => served_then_deciders(&response, &[], None),
            PickerTarget::AddMember { id } => {
                let group = self.group(id);
                let skip = group
                    .map(|group| {
                        group
                            .members
                            .iter()
                            .map(|member| member.reference.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let decides = group.is_some_and(|group| {
                    !group.members.is_empty()
                        && group.members.iter().all(|member| {
                            response.deciders.iter().any(|candidate| {
                                candidate.decision && candidate.reference == member.reference
                            })
                        })
                });
                if decides {
                    response
                        .deciders
                        .into_iter()
                        .filter(|candidate| !skip.contains(&candidate.reference))
                        .collect()
                } else {
                    response
                        .served
                        .into_iter()
                        .filter(|candidate| {
                            !skip.contains(&candidate.reference)
                                && candidate.reference != format!("group/{id}")
                        })
                        .collect()
                }
            }
            PickerTarget::Classifier { id } => {
                served_then_deciders(&response, &[], Some(id.as_str()))
            }
        };
        let RoutingMode::Picker(picker) = &mut self.mode else {
            return;
        };
        picker.items = items;
        picker.cursor = 0;
    }

    /// Applies a successful create, patch, or restore.
    pub fn apply_group_write(&mut self, previous_id: &str, group: Group, revision: String) {
        if revision.len() == REVISION_LEN {
            self.disk_revision = revision;
        }
        let id = group.id.clone();
        if let Some(existing) = self
            .groups
            .iter_mut()
            .find(|existing| existing.id == previous_id || existing.id == id)
        {
            *existing = group;
        } else {
            self.groups.push(group);
        }
        self.groups.sort_by_key(|group| group.hidden);
        if matches!(&self.mode, RoutingMode::Group { id: open } if open == previous_id) {
            self.mode = RoutingMode::Group { id };
        }
        self.clamp_group_cursor();
    }

    /// Applies a successful delete.
    pub fn apply_group_delete(&mut self, id: &str, revision: String) {
        if revision.len() == REVISION_LEN {
            self.disk_revision = revision;
        }
        self.groups.retain(|group| group.id != id);
        self.cursor = self.cursor.min(self.groups.len().saturating_sub(1));
        if matches!(&self.mode, RoutingMode::Group { id: open } if open == id) {
            self.mode = RoutingMode::List;
        }
    }

    /// Applies route decisions, the selected session trace, and unrouted holds.
    pub fn apply_route_view(&mut self, snapshot: RouteSnapshot) {
        let RoutingMode::Route(state) = &mut self.mode else {
            return;
        };
        state.cursor = state
            .cursor
            .min(snapshot.routes.decisions.len().saturating_sub(1));
        state.snapshot = Some(snapshot);
    }

    /// Clears delete confirmation and returns to the list on a page change.
    pub fn leave(&mut self) {
        self.confirm = None;
        self.candidate_request = None;
        self.mode = RoutingMode::List;
    }

    /// Handles a key while Routing is active.
    pub fn on_key(&mut self, key: KeyEvent) -> PageEffect {
        match &self.mode {
            RoutingMode::List => self.on_list(key),
            RoutingMode::Group { .. } => self.on_group(key),
            RoutingMode::Picker(_) => self.on_picker(key),
            RoutingMode::Ask(_) => self.on_ask(key),
            RoutingMode::Route(_) => self.on_route(key),
        }
    }

    /// Draws the page body.
    pub fn draw(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let lines = match &self.mode {
            RoutingMode::List => self.list_lines(area.height, theme),
            RoutingMode::Group { id } => self.group_lines(id, area.height, theme),
            RoutingMode::Picker(picker) => picker_lines(picker, area.height, theme),
            RoutingMode::Ask(ask) => ask_lines(ask, theme),
            RoutingMode::Route(state) => route_lines(state, area.height, theme),
        };
        Paragraph::new(lines).render(area, buf);
    }

    fn on_list(&mut self, key: KeyEvent) -> PageEffect {
        if !matches!(key.code, KeyCode::Char('d')) {
            self.confirm = None;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut self.cursor, self.groups.len(), 1);
                return PageEffect::none();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut self.cursor, self.groups.len(), -1);
                return PageEffect::none();
            }
            KeyCode::Char('n') => {
                self.mode = RoutingMode::Ask(Ask {
                    title: "new group name".to_owned(),
                    buffer: String::new(),
                    then: AskThen::Create,
                });
                return PageEffect::none();
            }
            KeyCode::Char('v') => {
                self.mode = RoutingMode::Route(Box::default());
                return PageEffect::read(ControlIntent::LoadRouteView { generation: 0 });
            }
            _ => {}
        }
        let Some(group) = self.selected().cloned() else {
            return PageEffect::none();
        };
        if group.hidden {
            if matches!(key.code, KeyCode::Char('u')) && group.found {
                return self.restore(group);
            }
            return PageEffect::flash(format!("{} was removed; u restores it", group.id), false);
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.group_cursor = 0;
                self.mode = RoutingMode::Group { id: group.id };
                PageEffect::none()
            }
            KeyCode::Char('o') => self.change_strategy(&group),
            KeyCode::Char('d') => self.delete_group(&group),
            _ => PageEffect::none(),
        }
    }

    fn on_group(&mut self, key: KeyEvent) -> PageEffect {
        if !matches!(key.code, KeyCode::Char('d')) {
            self.confirm = None;
        }
        let Some(id) = self.open_id().map(str::to_owned) else {
            self.mode = RoutingMode::List;
            return PageEffect::none();
        };
        let Some(group) = self.group(&id).cloned() else {
            self.mode = RoutingMode::List;
            return PageEffect::none();
        };
        let members = group.members.len();
        let rules = group.rules.len();
        let total = members + rules;
        let on_rule = self.group_cursor >= members;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = RoutingMode::List;
                PageEffect::none()
            }
            KeyCode::Char('J')
                if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_group_row(&group, 1)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_group_row(&group, 1)
            }
            KeyCode::Char('K')
                if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_group_row(&group, -1)
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_group_row(&group, -1)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut self.group_cursor, total, 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut self.group_cursor, total, -1);
                PageEffect::none()
            }
            KeyCode::Char('a') => self.open_picker(PickerTarget::AddMember { id }),
            KeyCode::Char('n') => {
                self.open_rule(group, None);
                PageEffect::none()
            }
            KeyCode::Enter | KeyCode::Char('e') if on_rule => {
                self.open_rule(group, Some(self.group_cursor - members));
                PageEffect::none()
            }
            KeyCode::Char('c') => {
                if group.rules.iter().any(|rule| rule_has_intent(&rule.line)) {
                    self.open_picker(PickerTarget::Classifier { id })
                } else {
                    PageEffect::flash(
                        format!("{} has no rule with an intent; n adds one", group.name),
                        false,
                    )
                }
            }
            KeyCode::Char('E') | KeyCode::Char('F') if !on_rule => {
                self.change_member(&group, matches!(key.code, KeyCode::Char('F')))
            }
            KeyCode::Char('o') => self.change_strategy(&group),
            KeyCode::Char('s') => self.change_stays(&group),
            KeyCode::Char('f') => {
                self.mode = RoutingMode::Ask(Ask {
                    title: "family (empty clears it)".to_owned(),
                    buffer: group.family.unwrap_or_default(),
                    then: AskThen::Family { id },
                });
                PageEffect::none()
            }
            KeyCode::Char('x') => {
                self.mode = RoutingMode::Ask(Ask {
                    title: "context tokens, e.g. 200k (empty clears it)".to_owned(),
                    buffer: group
                        .context
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    then: AskThen::Context { id },
                });
                PageEffect::none()
            }
            KeyCode::Char('l') => {
                self.mode = RoutingMode::Ask(Ask {
                    title: "reasoning levels, comma separated (empty clears them)".to_owned(),
                    buffer: group.levels.join(","),
                    then: AskThen::Levels { id },
                });
                PageEffect::none()
            }
            KeyCode::Char('R') => {
                self.mode = RoutingMode::Ask(Ask {
                    title: "new group id".to_owned(),
                    buffer: group.id,
                    then: AskThen::Rename { id },
                });
                PageEffect::none()
            }
            KeyCode::Char('d') => self.delete_group_row(&group),
            _ => PageEffect::none(),
        }
    }

    fn on_picker(&mut self, key: KeyEvent) -> PageEffect {
        let RoutingMode::Picker(picker) = &mut self.mode else {
            return PageEffect::none();
        };
        let visible = filtered(&picker.items, &picker.filter);
        match key.code {
            KeyCode::Esc => {
                self.close_picker();
                PageEffect::none()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                move_cursor(&mut picker.cursor, visible.len(), 1);
                PageEffect::none()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_cursor(&mut picker.cursor, visible.len(), -1);
                PageEffect::none()
            }
            KeyCode::Enter => {
                let Some(candidate) = visible
                    .get(picker.cursor)
                    .and_then(|index| picker.items.get(*index))
                    .cloned()
                else {
                    return PageEffect::flash("no member candidate selected", false);
                };
                self.pick_candidate(candidate.reference)
            }
            KeyCode::Backspace => {
                picker.filter.pop();
                picker.cursor = 0;
                PageEffect::none()
            }
            KeyCode::Char(character) if key.modifiers.is_empty() && !character.is_control() => {
                picker.filter.push(character);
                picker.cursor = 0;
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn on_ask(&mut self, key: KeyEvent) -> PageEffect {
        match key.code {
            KeyCode::Esc => {
                self.close_ask();
                PageEffect::none()
            }
            KeyCode::Backspace => {
                if let RoutingMode::Ask(ask) = &mut self.mode {
                    ask.buffer.pop();
                }
                PageEffect::none()
            }
            KeyCode::Enter => self.submit_ask(),
            KeyCode::Char(character) if !character.is_control() => {
                if let RoutingMode::Ask(ask) = &mut self.mode {
                    ask.buffer.push(character);
                }
                PageEffect::none()
            }
            _ => PageEffect::none(),
        }
    }

    fn on_route(&mut self, key: KeyEvent) -> PageEffect {
        let RoutingMode::Route(state) = &mut self.mode else {
            return PageEffect::none();
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => {
                self.mode = RoutingMode::List;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let len = state
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.routes.decisions.len())
                    .unwrap_or(0);
                move_cursor(&mut state.cursor, len, 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let len = state
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.routes.decisions.len())
                    .unwrap_or(0);
                move_cursor(&mut state.cursor, len, -1);
            }
            _ => {}
        }
        PageEffect::none()
    }

    fn selected(&self) -> Option<&Group> {
        self.groups.get(self.cursor)
    }

    fn open_id(&self) -> Option<&str> {
        match &self.mode {
            RoutingMode::Group { id } => Some(id),
            _ => None,
        }
    }

    fn group(&self, id: &str) -> Option<&Group> {
        self.groups
            .iter()
            .find(|group| group.id == id && !group.hidden)
    }

    fn revision(&self) -> Option<String> {
        (self.disk_revision.len() == REVISION_LEN).then(|| self.disk_revision.clone())
    }

    fn change_strategy(&self, group: &Group) -> PageEffect {
        let next = next_name(ROUTINGS, &group.routing);
        self.patch(group.id.clone(), |request| {
            request.routing = Some(next.to_owned());
        })
    }

    fn change_stays(&self, group: &Group) -> PageEffect {
        let next = next_name(STAYS, &group.stays);
        self.patch(group.id.clone(), |request| {
            request.stays = Some(next.to_owned());
        })
    }

    fn change_member(&self, group: &Group, fast: bool) -> PageEffect {
        let Some(member) = group.members.get(self.group_cursor) else {
            return PageEffect::none();
        };
        if member.reference.starts_with("group/") {
            return PageEffect::none();
        }
        let mut members = member_wires(&group.members);
        if fast {
            members[self.group_cursor].fast = !members[self.group_cursor].fast;
        } else {
            members[self.group_cursor].effort = next_name(EFFORTS, &member.effort).to_owned();
        }
        self.patch(group.id.clone(), |request| {
            request.members = Some(members);
        })
    }

    fn move_group_row(&mut self, group: &Group, delta: i32) -> PageEffect {
        let members = group.members.len();
        let total = members + group.rules.len();
        let from = self.group_cursor;
        let Ok(to) = i32::try_from(from).map(|from| from + delta) else {
            return PageEffect::none();
        };
        if to < 0 || to >= total as i32 || (to >= members as i32) != (from >= members) {
            return PageEffect::none();
        }
        let to = to as usize;
        self.group_cursor = to;
        if from < members {
            let mut wires = member_wires(&group.members);
            wires.swap(from, to);
            self.patch(group.id.clone(), |request| {
                request.members = Some(wires);
            })
        } else {
            let mut lines = rule_lines(group);
            lines.swap(from - members, to - members);
            self.patch(group.id.clone(), |request| {
                request.rules = Some(lines);
            })
        }
    }

    fn delete_group(&mut self, group: &Group) -> PageEffect {
        let token = format!("group/{}", group.id);
        if self.confirm.as_deref() != Some(token.as_str()) {
            self.confirm = Some(token);
            return PageEffect::flash(format!("press d again to delete {}", group.name), false);
        }
        self.confirm = None;
        let Some(revision) = self.revision() else {
            return unknown_revision();
        };
        PageEffect::mutation(
            "deleting group",
            ControlIntent::DeleteGroup {
                id: group.id.clone(),
                revision,
            },
        )
    }

    fn delete_group_row(&mut self, group: &Group) -> PageEffect {
        let members = group.members.len();
        if self.group_cursor >= members {
            let index = self.group_cursor - members;
            let token = format!("rule/{index}");
            if self.confirm.as_deref() != Some(token.as_str()) {
                self.confirm = Some(token);
                return PageEffect::flash(
                    format!("press d again to remove rule {}", index + 1),
                    false,
                );
            }
            self.confirm = None;
            let mut lines = rule_lines(group);
            lines.remove(index);
            return self.patch(group.id.clone(), |request| {
                request.rules = Some(lines);
            });
        }
        if group.members.len() == 1 {
            self.confirm = None;
            return PageEffect::flash(
                "a group needs a member; dd on the Routing list deletes the group",
                false,
            );
        }
        let index = self.group_cursor;
        let token = format!("member/{index}");
        if self.confirm.as_deref() != Some(token.as_str()) {
            self.confirm = Some(token);
            return PageEffect::flash(
                format!(
                    "press d again to take {} out; rules that target only it are dropped",
                    group.members[index].reference
                ),
                false,
            );
        }
        self.confirm = None;
        let mut wires = member_wires(&group.members);
        wires.remove(index);
        self.patch(group.id.clone(), |request| {
            request.members = Some(wires);
        })
    }

    fn restore(&self, group: Group) -> PageEffect {
        let Some(revision) = self.revision() else {
            return unknown_revision();
        };
        PageEffect::mutation(
            "restoring group",
            ControlIntent::RestoreGroup {
                id: group.id,
                revision,
            },
        )
    }

    fn open_picker(&mut self, target: PickerTarget) -> PageEffect {
        self.candidate_token = self.candidate_token.saturating_add(1);
        let token = self.candidate_token;
        self.candidate_request = Some(token);
        self.mode = RoutingMode::Picker(Picker {
            target,
            items: Vec::new(),
            cursor: 0,
            filter: String::new(),
        });
        PageEffect::read(ControlIntent::LoadGroupCandidates { token })
    }

    fn close_picker(&mut self) {
        let RoutingMode::Picker(picker) = &self.mode else {
            return;
        };
        self.mode = match &picker.target {
            PickerTarget::Create { .. } => RoutingMode::List,
            PickerTarget::AddMember { id } | PickerTarget::Classifier { id } => {
                RoutingMode::Group { id: id.clone() }
            }
        };
        self.candidate_request = None;
    }

    fn pick_candidate(&mut self, reference: String) -> PageEffect {
        let RoutingMode::Picker(picker) = &self.mode else {
            return PageEffect::none();
        };
        let target = picker.target.clone();
        self.candidate_request = None;
        match target {
            PickerTarget::Create { name } => {
                self.mode = RoutingMode::List;
                let Some(revision) = self.revision() else {
                    return unknown_revision();
                };
                let request = GroupCreateRequest::new(revision, name, reference);
                PageEffect::mutation("creating group", ControlIntent::CreateGroup { request })
            }
            PickerTarget::AddMember { id } => {
                self.mode = RoutingMode::Group { id: id.clone() };
                let Some(group) = self.group(&id) else {
                    return PageEffect::none();
                };
                let mut wires = member_wires(&group.members);
                wires.push(GroupMemberWire::new(reference));
                self.patch(id, |request| {
                    request.members = Some(wires);
                })
            }
            PickerTarget::Classifier { id } => {
                self.mode = RoutingMode::Group { id: id.clone() };
                self.patch(id, |request| {
                    request.classifier = Some(reference);
                })
            }
        }
    }

    fn open_rule(&mut self, group: Group, index: Option<usize>) {
        let buffer = index
            .and_then(|index| group.rules.get(index))
            .map(|rule| rule.line.clone())
            .unwrap_or_default();
        self.mode = RoutingMode::Ask(Ask {
            title: match index {
                Some(index) => format!("rule {}", index + 1),
                None => "new rule".to_owned(),
            },
            buffer,
            then: AskThen::Rule {
                id: group.id,
                index,
            },
        });
    }

    fn submit_ask(&mut self) -> PageEffect {
        let RoutingMode::Ask(ask) = &self.mode else {
            return PageEffect::none();
        };
        let ask = ask.clone();
        match ask.then {
            AskThen::Create => {
                let name = ask.buffer.trim().to_owned();
                if name.is_empty() {
                    return PageEffect::flash("a group name is required", false);
                }
                self.open_picker(PickerTarget::Create { name })
            }
            AskThen::Rule { id, index } => {
                self.mode = RoutingMode::Group { id: id.clone() };
                let Some(group) = self.group(&id) else {
                    return PageEffect::none();
                };
                let mut lines = rule_lines(group);
                if let Some(index) = index {
                    if index < lines.len() {
                        lines[index] = ask.buffer;
                    }
                } else {
                    lines.push(ask.buffer);
                }
                self.patch(id, |request| {
                    request.rules = Some(lines);
                })
            }
            AskThen::Family { id } => {
                self.mode = RoutingMode::Group { id: id.clone() };
                self.patch(id, |request| {
                    request.family = Some((!ask.buffer.trim().is_empty()).then_some(ask.buffer));
                })
            }
            AskThen::Context { id } => {
                let context = match parse_context(&ask.buffer) {
                    Ok(context) => context,
                    Err(message) => return PageEffect::flash(message, false),
                };
                self.mode = RoutingMode::Group { id: id.clone() };
                self.patch(id, |request| {
                    request.context = Some(context);
                })
            }
            AskThen::Levels { id } => {
                let levels = ask
                    .buffer
                    .split(|character: char| character == ',' || character.is_whitespace())
                    .filter(|level| !level.is_empty())
                    .map(str::to_owned)
                    .collect();
                self.mode = RoutingMode::Group { id: id.clone() };
                self.patch(id, |request| {
                    request.levels = Some(levels);
                })
            }
            AskThen::Rename { id } => {
                let to = ask.buffer.trim().to_owned();
                if to.is_empty() {
                    return PageEffect::flash("a group id is required", false);
                }
                self.mode = RoutingMode::Group { id: id.clone() };
                self.patch(id, |request| {
                    request.rename_to = Some(to);
                })
            }
        }
    }

    fn close_ask(&mut self) {
        let RoutingMode::Ask(ask) = &self.mode else {
            return;
        };
        self.mode = match &ask.then {
            AskThen::Create => RoutingMode::List,
            AskThen::Rule { id, .. }
            | AskThen::Family { id }
            | AskThen::Context { id }
            | AskThen::Levels { id }
            | AskThen::Rename { id } => RoutingMode::Group { id: id.clone() },
        };
    }

    fn patch(&self, id: String, change: impl FnOnce(&mut GroupPatchRequest)) -> PageEffect {
        let Some(revision) = self.revision() else {
            return unknown_revision();
        };
        let mut request = GroupPatchRequest::default();
        request.version = API_VERSION;
        request.expected_revision = revision;
        change(&mut request);
        PageEffect::mutation("saving group", ControlIntent::PatchGroup { id, request })
    }

    fn clamp_group_cursor(&mut self) {
        let total = self
            .open_id()
            .and_then(|id| self.group(id))
            .map(|group| group.members.len() + group.rules.len())
            .unwrap_or(0);
        self.group_cursor = self.group_cursor.min(total.saturating_sub(1));
    }

    fn list_lines(&self, height: u16, theme: &Theme) -> Vec<Line<'static>> {
        if self.groups.is_empty() {
            let message = if let Some(error) = &self.load_error {
                format!("Could not load routing groups: {error}")
            } else if self.loading || !self.loaded {
                "Loading routing groups…".to_owned()
            } else {
                "No routing groups yet. Press n to make one.".to_owned()
            };
            return vec![Line::from(Span::styled(
                format!("  {message}"),
                theme.muted_style(),
            ))];
        }
        let window = list_window(height, 3, self.groups.len(), self.cursor);
        let mut lines = Vec::new();
        for (index, group) in self
            .groups
            .iter()
            .enumerate()
            .skip(window.start)
            .take(window.visible)
        {
            if group.hidden && (index == 0 || !self.groups[index - 1].hidden) {
                lines.push(Line::from(Span::styled(
                    "  removed found groups · u restores the selected group",
                    theme.muted_style(),
                )));
            }
            let marker = if index == self.cursor { "> " } else { "  " };
            let name = if group.name.is_empty() {
                group.id.as_str()
            } else {
                group.name.as_str()
            };
            let text = if group.hidden {
                format!("{marker}{name}  group/{}  hidden found group", group.id)
            } else {
                let mut notes = vec![
                    format!(
                        "{} member{}",
                        group.members.len(),
                        plural(group.members.len())
                    ),
                    group.routing.clone(),
                ];
                if group.stays != "auto" && !group.stays.is_empty() {
                    notes.push(format!("stays {}", group.stays));
                }
                if !group.rules.is_empty() {
                    notes.push(format!(
                        "{} rule{}",
                        group.rules.len(),
                        plural(group.rules.len())
                    ));
                }
                if let Some(family) = &group.family
                    && !family.is_empty()
                {
                    notes.push(format!("family {family}"));
                }
                if group.found {
                    notes.push("found".to_owned());
                }
                let members = group
                    .members
                    .iter()
                    .map(|member| member.reference.as_str())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                format!(
                    "{marker}{name}  group/{}  {}  {members}",
                    group.id,
                    notes.join(" · ")
                )
            };
            let style = if index == self.cursor {
                theme.cursor_style()
            } else if group.hidden {
                theme.muted_style()
            } else {
                theme.text_style()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        lines
    }

    fn group_lines(&self, id: &str, height: u16, theme: &Theme) -> Vec<Line<'static>> {
        let Some(group) = self.group(id) else {
            return vec![Line::from(Span::styled(
                "  The group is no longer visible.",
                theme.muted_style(),
            ))];
        };
        let mut head = vec![
            format!("group/{}", group.id),
            group.routing.clone(),
            format!("stays {}", group.stays),
        ];
        if let Some(context) = group.context {
            head.push(format!("context {context}"));
        }
        if !group.levels.is_empty() {
            head.push(format!("levels {}", group.levels.join("/")));
        }
        if let Some(family) = &group.family
            && !family.is_empty()
        {
            head.push(format!("family {family}"));
        }
        let has_intent = group.rules.iter().any(|rule| rule_has_intent(&rule.line));
        let fixed = 6 + u16::from(group.rules.is_empty()) + u16::from(has_intent);
        let rows = usize::from(height.saturating_sub(fixed).max(3));
        let window = group_window(
            group.members.len(),
            group.rules.len(),
            self.group_cursor,
            rows,
        );
        let mut lines = vec![
            Line::from(Span::styled(
                format!("  routing · {}", group.name),
                theme.title_style(),
            )),
            Line::from(Span::styled(
                format!("  {}", head.join(" · ")),
                theme.muted_style(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  members · fallback follows this order",
                theme.muted_style(),
            )),
        ];
        for index in window.member_start..window.member_end {
            let member = &group.members[index];
            let marker = if index == self.group_cursor {
                "> "
            } else {
                "  "
            };
            let mut notes = Vec::new();
            if !member.effort.is_empty() {
                notes.push(format!("effort {}", member.effort));
            }
            if member.fast {
                notes.push("fast".to_owned());
            }
            if !member.enabled {
                notes.push("disabled".to_owned());
            }
            let suffix = if notes.is_empty() {
                String::new()
            } else {
                format!(" · {}", notes.join(" · "))
            };
            lines.push(selected_line(
                format!("{marker}{}  {}{suffix}", index + 1, member.reference),
                index == self.group_cursor,
                theme,
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  rules · the daemon parses each typed line",
            theme.muted_style(),
        )));
        if group.rules.is_empty() {
            lines.push(Line::from(Span::styled(
                "  none · n adds one",
                theme.muted_style(),
            )));
        }
        for index in window.rule_start..window.rule_end {
            let selected = group.members.len() + index == self.group_cursor;
            let marker = if selected { "> " } else { "  " };
            lines.push(selected_line(
                format!("{marker}{}  {}", index + 1, group.rules[index].line),
                selected,
                theme,
            ));
        }
        if has_intent {
            lines.push(Line::from(Span::styled(
                format!(
                    "  classifier {} · c changes it",
                    if group.classifier.is_empty() {
                        "not set"
                    } else {
                        &group.classifier
                    }
                ),
                theme.muted_style(),
            )));
        }
        lines
    }
}

impl Page for RoutingPage {
    fn id(&self) -> PageId {
        PageId::Routing
    }

    fn reload(&mut self, generation: u64) {
        Self::reload(self, generation);
    }

    fn on_key(&mut self, _key: KeyEvent) -> PageKeyOutcome {
        PageKeyOutcome::None
    }

    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.draw(area, buf, theme);
    }

    fn fetch_gen(&self) -> u64 {
        self.read_generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GroupWindow {
    member_start: usize,
    member_end: usize,
    rule_start: usize,
    rule_end: usize,
}

fn group_window(members: usize, rules: usize, selected: usize, rows: usize) -> GroupWindow {
    if members + rules <= rows {
        return GroupWindow {
            member_start: 0,
            member_end: members,
            rule_start: 0,
            rule_end: rules,
        };
    }
    if selected < members {
        let member_start = selected.saturating_add(1).saturating_sub(rows);
        let member_end = members.min(member_start + rows);
        return GroupWindow {
            member_start,
            member_end,
            rule_start: 0,
            rule_end: rules.min(rows.saturating_sub(member_end - member_start)),
        };
    }
    let rule_end = rules.min(selected.saturating_sub(members) + 1);
    let rule_start = rule_end.saturating_sub(rows);
    GroupWindow {
        member_start: members.saturating_sub(rows.saturating_sub(rule_end - rule_start)),
        member_end: members,
        rule_start,
        rule_end,
    }
}

fn served_then_deciders(
    response: &GroupMemberCandidatesResponse,
    skip: &[String],
    self_id: Option<&str>,
) -> Vec<GroupMemberCandidate> {
    response
        .served
        .iter()
        .chain(&response.deciders)
        .filter(|candidate| {
            !skip.contains(&candidate.reference)
                && self_id.is_none_or(|id| candidate.reference != format!("group/{id}"))
        })
        .cloned()
        .collect()
}

fn filtered(candidates: &[GroupMemberCandidate], filter: &str) -> Vec<usize> {
    let filter = filter.to_ascii_lowercase();
    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            filter.is_empty() || candidate.reference.to_ascii_lowercase().contains(&filter)
        })
        .map(|(index, _)| index)
        .collect()
}

fn member_wires(members: &[GroupMember]) -> Vec<GroupMemberWire> {
    members
        .iter()
        .map(|member| {
            let mut wire = GroupMemberWire::new(member.reference.clone());
            wire.effort = member.effort.clone();
            wire.fast = member.fast;
            wire.enabled = member.enabled;
            wire
        })
        .collect()
}

fn rule_lines(group: &Group) -> Vec<String> {
    group.rules.iter().map(|rule| rule.line.clone()).collect()
}

fn next_name<'a>(names: &'a [&str], current: &str) -> &'a str {
    names
        .iter()
        .position(|name| *name == current)
        .map(|index| names[(index + 1) % names.len()])
        .unwrap_or(names[0])
}

fn rule_has_intent(line: &str) -> bool {
    line.split_whitespace().any(|word| {
        ["intent=", "asks=", "about="]
            .iter()
            .any(|prefix| word.starts_with(prefix))
    })
}

fn parse_context(value: &str) -> Result<Option<i64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let lower = value.to_ascii_lowercase();
    let (number, multiplier) = match lower.as_bytes().last() {
        Some(b'k') => (&lower[..lower.len() - 1], 1_000_i64),
        Some(b'm') => (&lower[..lower.len() - 1], 1_000_000_i64),
        Some(b'g') => (&lower[..lower.len() - 1], 1_000_000_000_i64),
        _ => (lower.as_str(), 1_i64),
    };
    let number = number
        .parse::<i64>()
        .map_err(|_| "context must be tokens, for example 200000 or 200k".to_owned())?;
    number
        .checked_mul(multiplier)
        .map(Some)
        .ok_or_else(|| "context is too large".to_owned())
}

fn unknown_revision() -> PageEffect {
    PageEffect::flash("configuration revision is unknown; press r", false)
}

fn move_cursor(cursor: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        *cursor = 0;
        return;
    }
    *cursor = (*cursor as i32 + delta).rem_euclid(len as i32) as usize;
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn selected_line(text: String, selected: bool, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text,
        if selected {
            theme.cursor_style()
        } else {
            theme.text_style()
        },
    ))
}

fn picker_lines(picker: &Picker, height: u16, theme: &Theme) -> Vec<Line<'static>> {
    let title = match &picker.target {
        PickerTarget::Create { name } => format!("first member for {name}"),
        PickerTarget::AddMember { id } => format!("add a member to {id}"),
        PickerTarget::Classifier { id } => format!("classifier for {id}"),
    };
    let visible = filtered(&picker.items, &picker.filter);
    let mut lines = vec![Line::from(Span::styled(
        format!("  {title}"),
        theme.title_style(),
    ))];
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Loading candidates, or none are available.",
            theme.muted_style(),
        )));
        return lines;
    }
    let window = list_window(height.saturating_sub(1), 3, visible.len(), picker.cursor);
    for (offset, index) in visible
        .iter()
        .copied()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let candidate = &picker.items[index];
        let marker = if offset == picker.cursor { "> " } else { "  " };
        let note = if candidate.decision {
            " · decision model"
        } else {
            ""
        };
        lines.push(selected_line(
            format!("{marker}{}{note}", candidate.reference),
            offset == picker.cursor,
            theme,
        ));
    }
    lines
}

fn ask_lines(ask: &Ask, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {}", ask.title),
            theme.title_style(),
        )),
        Line::from(Span::styled(
            format!("  {}_", ask.buffer),
            theme.cursor_style(),
        )),
    ];
    if matches!(ask.then, AskThen::Rule { .. }) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {RULE_HINT}"),
            theme.muted_style(),
        )));
    }
    lines
}

fn route_lines(state: &RouteState, height: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "  route decisions · v returns to groups",
        theme.title_style(),
    ))];
    let Some(snapshot) = &state.snapshot else {
        lines.push(Line::from(Span::styled(
            "  Loading route view…",
            theme.muted_style(),
        )));
        return lines;
    };
    lines.push(Line::from(Span::styled(
        format!(
            "  {} unrouted hold{}",
            snapshot.unrouted.holds.len(),
            plural(snapshot.unrouted.holds.len())
        ),
        theme.muted_style(),
    )));
    for hold in &snapshot.unrouted.holds {
        lines.push(Line::from(Span::styled(
            format!("  waiting  {}  {}", hold.asked, hold.phase),
            theme.muted_style(),
        )));
    }
    if snapshot.routes.decisions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No routing decisions yet.",
            theme.muted_style(),
        )));
        return lines;
    }
    let remaining = height.saturating_sub(lines.len() as u16 + 2);
    let window = list_window(remaining, 3, snapshot.routes.decisions.len(), state.cursor);
    for (index, decision) in snapshot
        .routes
        .decisions
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.visible)
    {
        let marker = if index == state.cursor { "> " } else { "  " };
        let served = if !decision.served_member.is_empty() {
            decision.served_member.as_str()
        } else if !decision.error.is_empty() {
            decision.error.as_str()
        } else if decision.unrouted {
            "unrouted"
        } else {
            "pending"
        };
        lines.push(selected_line(
            format!(
                "{marker}{} -> {}  {}{}",
                decision.asked,
                served,
                decision.routing,
                decision
                    .status
                    .map(|status| format!("  {status}"))
                    .unwrap_or_default()
            ),
            index == state.cursor,
            theme,
        ));
    }
    if let Some(session) = &snapshot.session {
        lines.push(Line::from(""));
        let detail = session
            .decision
            .as_ref()
            .map(|decision| {
                format!(
                    "{} -> {}",
                    decision.asked,
                    if decision.served_member.is_empty() {
                        "pending"
                    } else {
                        &decision.served_member
                    }
                )
            })
            .unwrap_or_else(|| "no decision for this session".to_owned());
        lines.push(Line::from(Span::styled(
            format!("  session {} · {detail}", session.session),
            theme.muted_style(),
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{group_window, next_name, parse_context, rule_has_intent};

    #[test]
    fn routing_cycle_skips_unsupported_names() {
        assert_eq!(next_name(super::ROUTINGS, "smart"), "order");
        assert_eq!(next_name(super::ROUTINGS, "drain"), "smart");
        assert_eq!(next_name(super::ROUTINGS, "manual"), "smart");
    }

    #[test]
    fn member_effort_wraps_to_agent_default() {
        assert_eq!(next_name(super::EFFORTS, ""), "low");
        assert_eq!(next_name(super::EFFORTS, "max"), "");
    }

    #[test]
    fn context_accepts_reference_suffixes() {
        assert_eq!(parse_context("200k"), Ok(Some(200_000)));
        assert_eq!(parse_context(""), Ok(None));
        assert!(parse_context("many").is_err());
    }

    #[test]
    fn intent_detection_accepts_quoted_text() {
        assert!(rule_has_intent(r#"use=a intent="a quick question""#));
        assert!(!rule_has_intent("use=a tokens=200k"));
    }

    #[test]
    fn narrow_group_window_keeps_rule_selected() {
        let window = group_window(4, 5, 8, 3);
        assert!(window.rule_start <= 4 && 4 < window.rule_end);
    }
}
