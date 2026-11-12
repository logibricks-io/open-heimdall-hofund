//! Contract checks for the Providers and Agents fixes.
//! `refresh_event_before_refresh_result_releases_the_keyboard` is the refresh lock.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    AgentModelsResponse, AgentState, AgentStatusEntry, AgentSummary, EventPayload, ModelEntry,
    ModelsRefreshedEvent, PicksResponse, ProfileSummary, ProviderKey, ProviderSummary,
};
use hofund_tui::app::App;
use hofund_tui::events::apply_control_event;
use hofund_tui::intent::ControlIntent;
use hofund_tui::session::{AgentSnapshot, Finished, ModelView, ProviderSnapshot, SessionOutcome};
use hofund_tui::{PageId, buffer_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty())
}

fn revision(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn provider(id: &str) -> ProviderSummary {
    let mut summary = ProviderSummary::default();
    summary.id = id.to_owned();
    summary.enabled = true;
    summary.listed = true;
    summary.base_url = "https://example.test/v1".to_owned();
    summary
}

fn load_providers(app: &mut App, ids: &[&str], disk: &str) {
    let generation = app.generation();
    app.apply_session(SessionOutcome::Providers {
        generation,
        result: Ok(ProviderSnapshot {
            providers: ids.iter().copied().map(provider).collect(),
            presets: Vec::new(),
            disk_revision: disk.to_owned(),
            presets_error: None,
        }),
    });
}

fn agent_snapshot() -> AgentSnapshot {
    let mut summary = AgentSummary::default();
    summary.id = "claude".to_owned();
    summary.display_name = "claude".to_owned();
    summary.installed = true;
    let mut status = AgentStatusEntry::default();
    status.id = "claude".to_owned();
    status.state = AgentState::Disconnected;
    status.model = Some("route".to_owned());
    AgentSnapshot {
        agents: vec![summary],
        status: vec![status],
    }
}

fn load_agents(app: &mut App) {
    let generation = app.generation();
    app.apply_session(SessionOutcome::Agents {
        generation,
        result: Ok(agent_snapshot()),
    });
}

fn models_response(shown: &[&str], hidden: &[&str]) -> AgentModelsResponse {
    let mut models = AgentModelsResponse::default();
    models.agent = "claude".to_owned();
    models.shown = shown.iter().map(|id| (*id).to_owned()).collect();
    models.hidden = hidden.iter().map(|id| (*id).to_owned()).collect();
    models
}

fn model_view(
    ids: &[&str],
    picks: &[&str],
    default_exposed: &[&str],
    picks_revision: &str,
) -> ModelView {
    let mut response = PicksResponse::default();
    response.picks = picks.iter().map(|id| (*id).to_owned()).collect();
    response.default_exposed = default_exposed.iter().map(|id| (*id).to_owned()).collect();
    response.picks_revision = picks_revision.to_owned();
    ModelView {
        models: ids
            .iter()
            .map(|id| {
                let mut model = ModelEntry::default();
                model.id = (*id).to_owned();
                model
            })
            .collect(),
        picks: response,
    }
}

fn open_outcome(generation: u64, id: &str, token: u64, view: ModelView) -> SessionOutcome {
    SessionOutcome::Models {
        generation,
        id: id.to_owned(),
        open: true,
        token,
        owns_pending: false,
        result: Ok(view),
        flash: None,
    }
}

fn refresh_outcome(generation: u64, id: &str, view: ModelView) -> SessionOutcome {
    SessionOutcome::Models {
        generation,
        id: id.to_owned(),
        open: false,
        token: 0,
        owns_pending: true,
        result: Ok(view),
        flash: Some(("refreshed 2 models".to_owned(), true)),
    }
}

fn finished(mut edit: impl FnMut(&mut Finished)) -> SessionOutcome {
    let mut done = Finished {
        flash: ("saved".to_owned(), true),
        reload_providers: false,
        reload_agents: false,
        reload_profiles: false,
        disk_revision: None,
        picks: None,
        revert_picks: None,
        revert_hidden: None,
        commit_hidden: None,
        profiles: None,
    };
    edit(&mut done);
    SessionOutcome::Finished(done)
}

fn hidden_ids(intents: &[ControlIntent]) -> Vec<String> {
    intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::SetHiddenModels { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .expect("hidden write")
}

fn open_token(intents: &[ControlIntent], id: &str) -> u64 {
    intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::OpenModels {
                id: provider,
                token,
                ..
            } if provider == id => Some(*token),
            _ => None,
        })
        .expect("open")
}

fn agent_lines(app: &App) -> String {
    let area = Rect::new(0, 0, 80, 8);
    let mut buf = Buffer::empty(area);
    app.agents
        .draw(area, &mut buf, &hofund_tui::Theme::monochrome());
    buffer_text(&buf, area.width, area.height)
}

#[test]
fn page_change_disarms_provider_delete() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first", "second"], &revision(0x11));
    app.handle_key(key('d'));
    app.handle_key(key('1'));
    app.handle_key(key('2'));
    app.handle_key(key('d'));
    let intents = app.take_intents();
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent, ControlIntent::DeleteProvider { .. })),
        "leaving the page must disarm dd, got {intents:?}"
    );
}

#[test]
fn same_list_reload_keeps_delete_armed() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('d'));
    app.handle_key(key('r'));
    app.handle_key(key('d'));
    let intents = app.take_intents();
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            ControlIntent::DeleteProvider { id, revision: expected }
                if id == "first" && expected == &revision(0x11)
        )),
        "r in list mode must leave dd armed, got {intents:?}"
    );
}

#[test]
fn successful_write_revision_is_used_for_the_next_delete() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.apply_session(finished(|done| {
        done.disk_revision = Some(revision(0x22));
    }));
    app.handle_key(key('d'));
    app.handle_key(key('d'));
    let intents = app.take_intents();
    assert!(
        intents.iter().any(|intent| matches!(
            intent,
            ControlIntent::DeleteProvider { revision: expected, .. } if expected == &revision(0x22)
        )),
        "delete must send the revision from the write, got {intents:?}"
    );
}

#[test]
fn late_model_open_for_another_provider_does_not_capture_toggle() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first", "second"], &revision(0x11));
    let generation = app.generation();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let first = open_token(&app.take_intents(), "first");
    app.handle_key(key('j'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let second = open_token(&app.take_intents(), "second");
    app.apply_session(open_outcome(
        generation,
        "first",
        first,
        model_view(&["a"], &["a"], &[], &revision(0x33)),
    ));
    app.apply_session(open_outcome(
        generation,
        "second",
        second,
        model_view(&["b"], &["b"], &[], &revision(0x33)),
    ));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let intents = app.take_intents();
    assert!(
        !intents.iter().any(|intent| matches!(
            intent,
            ControlIntent::ReplacePicks { id, .. } if id == "first"
        )),
        "a late open must not leave the first provider's picker under the cursor, got {intents:?}"
    );
    assert!(
        !format!("{:?}", app.providers).contains("provider: \"first\""),
        "picker must stay on the provider that was current when its open returned"
    );
}

#[test]
fn model_timeout_does_not_clear_another_mutations_pending_flag() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    assert!(app.try_start_pending("testing provider"));
    let outcome = ControlIntent::OpenModels {
        id: "first".to_owned(),
        generation: app.generation(),
        token: 1,
    }
    .timed_out();
    app.apply_session(outcome);
    assert!(
        app.pending.is_some(),
        "an open-models timeout must not release a different mutation"
    );
}

#[test]
fn refresh_event_before_refresh_result_releases_the_keyboard() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('m'));
    let generation = app
        .take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::RefreshModels { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("refresh");
    assert!(app.pending.is_some());
    let mut event = ModelsRefreshedEvent::default();
    event.provider = "first".to_owned();
    event.ok = true;
    event.model_count = 2;
    apply_control_event(&mut app, &EventPayload::ModelsRefreshed(event));
    app.apply_session(refresh_outcome(
        generation,
        "first",
        model_view(&["a", "b"], &["a"], &[], &revision(0x33)),
    ));
    assert!(
        app.pending.is_none(),
        "the refresh that emitted models_refreshed must still release pending"
    );
    app.handle_key(key('t'));
    assert!(
        app.take_intents()
            .iter()
            .any(|intent| matches!(intent, ControlIntent::TestProvider { .. })),
        "keys after refresh must reach the page"
    );
}

#[test]
fn refresh_result_without_an_event_releases_pending() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('m'));
    let generation = app
        .take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::RefreshModels { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("refresh");
    app.apply_session(refresh_outcome(
        generation,
        "first",
        model_view(&["a"], &["a"], &[], &revision(0x33)),
    ));
    assert!(app.pending.is_none());
}

#[test]
fn profile_load_after_escape_does_not_reopen_the_picker() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    app.handle_key(key('p'));
    let token = app
        .take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadProfiles { token } => Some(token),
            _ => None,
        })
        .expect("profiles");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    let mut profile = ProfileSummary::default();
    profile.name = "work".to_owned();
    app.apply_session(SessionOutcome::Profiles {
        token,
        result: Ok(vec![profile]),
    });
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let intents = app.take_intents();
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent, ControlIntent::SwitchProfile { .. })),
        "a canceled profile read must not reopen the picker, got {intents:?}"
    );
}

fn hide_then_fail(app: &mut App, fail: SessionOutcome) -> Vec<String> {
    app.handle_key(key('l'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let generation = app
        .take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadAgentModels {
                generation, open, ..
            } if open => Some(generation),
            _ => None,
        })
        .expect("visibility open");
    app.apply_session(SessionOutcome::AgentModels {
        generation,
        agent: "claude".to_owned(),
        open: true,
        result: Ok(models_response(&["a", "b"], &[])),
    });
    app.handle_key(key('j'));
    app.handle_key(key('j'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let first = hidden_ids(&app.take_intents());
    assert_eq!(first, vec!["a".to_owned()]);
    app.apply_session(fail);
    app.handle_key(key('j'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    hidden_ids(&app.take_intents())
}

#[test]
fn failed_hidden_write_does_not_stay_in_the_next_request() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    let _ = app.take_intents();
    let next = hide_then_fail(
        &mut app,
        finished(|done| {
            done.flash = ("rejected".to_owned(), false);
            done.revert_hidden = Some("claude".to_owned());
        }),
    );
    assert_eq!(
        next,
        vec!["b".to_owned()],
        "http failure must roll the hidden list back"
    );
}

#[test]
fn timed_out_hidden_write_does_not_stay_in_the_next_request() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    let _ = app.take_intents();
    let next = hide_then_fail(
        &mut app,
        ControlIntent::SetHiddenModels {
            agent: "claude".to_owned(),
            ids: vec!["a".to_owned()],
        }
        .timed_out(),
    );
    assert_eq!(
        next,
        vec!["b".to_owned()],
        "timeout must roll the hidden list back"
    );
}

#[test]
fn list_visibility_survives_reload_when_the_new_read_returns() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    let first = app.take_intents();
    let generation = first
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadAgentModels {
                generation,
                open: false,
                ..
            } => Some(*generation),
            _ => None,
        })
        .expect("list visibility read");
    app.apply_session(SessionOutcome::AgentModels {
        generation,
        agent: "claude".to_owned(),
        open: false,
        result: Ok(models_response(&[], &["secret"])),
    });
    let shown = agent_lines(&app);
    assert!(
        shown.contains("visibility:hidden"),
        "the list column must show the models read, got {shown}"
    );
    app.handle_key(key('r'));
    let reloaded = app.take_intents();
    let next_generation = reloaded
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadAgents { generation } => Some(*generation),
            _ => None,
        })
        .expect("agent reload");
    app.apply_session(SessionOutcome::Agents {
        generation: next_generation,
        result: Ok(agent_snapshot()),
    });
    app.apply_session(SessionOutcome::AgentModels {
        generation,
        agent: "claude".to_owned(),
        open: false,
        result: Ok(models_response(&[], &["secret"])),
    });
    let stale = agent_lines(&app);
    assert!(
        !stale.contains("visibility:hidden"),
        "a visibility read from the previous load must not fill the new list, got {stale}"
    );
    let queued = app.take_intents();
    let fresh = queued
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadAgentModels {
                generation,
                open: false,
                ..
            } => Some(*generation),
            _ => None,
        })
        .expect("reload must queue a new visibility read");
    app.apply_session(SessionOutcome::AgentModels {
        generation: fresh,
        agent: "claude".to_owned(),
        open: false,
        result: Ok(models_response(&["route"], &[])),
    });
    let after = agent_lines(&app);
    assert!(
        after.contains("visibility:shown"),
        "the new read must restore the column, got {after}"
    );
}

#[test]
fn read_only_model_list_does_not_send_reset() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    let generation = app.generation();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let token = open_token(&app.take_intents(), "first");
    app.apply_session(open_outcome(
        generation,
        "first",
        token,
        model_view(&["image"], &[], &[], &revision(0x33)),
    ));
    app.handle_key(key('x'));
    let intents = app.take_intents();
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent, ControlIntent::ResetPicks { .. })),
        "x on an empty default exposure must not send Reset, got {intents:?}"
    );
}

#[test]
fn key_read_does_not_replace_an_address_prompt() {
    let mut app = App::for_test(PageId::Providers);
    let mut local = provider("first");
    local.base_url = "http://127.0.0.1:9/v1".to_owned();
    let generation = app.generation();
    app.apply_session(SessionOutcome::Providers {
        generation,
        result: Ok(ProviderSnapshot {
            providers: vec![local],
            presets: Vec::new(),
            disk_revision: revision(0x11),
            presets_error: None,
        }),
    });
    app.handle_key(key('e'));
    let token = app
        .take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadKeys { token, .. } => Some(token),
            _ => None,
        })
        .expect("keys");
    app.handle_key(key('w'));
    let mut key = ProviderKey::default();
    key.id = "k1".to_owned();
    app.apply_session(SessionOutcome::Keys {
        id: "first".to_owned(),
        token,
        result: Ok(vec![key]),
    });
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("address"),
        "the address prompt must stay, got {rendered}"
    );
    assert!(
        !rendered.contains("new key"),
        "a late key read must not replace the address prompt, got {rendered}"
    );
}

#[test]
fn superseded_open_does_not_replace_a_newer_picker() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    let generation = app.generation();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let first = open_token(&app.take_intents(), "first");
    app.apply_session(open_outcome(
        generation,
        "first",
        first,
        model_view(&["current"], &["current"], &[], &revision(0x33)),
    ));
    app.refresh_open_picker();
    let second = open_token(&app.take_intents(), "first");
    assert_ne!(first, second);
    app.apply_session(open_outcome(
        generation,
        "first",
        second,
        model_view(&["fresh"], &["fresh"], &[], &revision(0x44)),
    ));
    app.apply_session(open_outcome(
        generation,
        "first",
        first,
        model_view(&["stale"], &["stale"], &[], &revision(0x33)),
    ));
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("fresh") && !rendered.contains("stale"),
        "an older open for the same provider must not replace the picker, got {rendered}"
    );
}

fn models_refreshed(provider: &str) -> EventPayload {
    let mut event = ModelsRefreshedEvent::default();
    event.provider = provider.to_owned();
    event.ok = true;
    event.model_count = 2;
    EventPayload::ModelsRefreshed(event)
}

fn refresh_generation(app: &mut App) -> u64 {
    app.take_intents()
        .into_iter()
        .find_map(|intent| match intent {
            ControlIntent::RefreshModels { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("refresh")
}

#[test]
fn refresh_timeout_after_models_refreshed_releases_the_keyboard() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('m'));
    let generation = refresh_generation(&mut app);
    apply_control_event(&mut app, &models_refreshed("first"));
    app.apply_session(
        ControlIntent::RefreshModels {
            id: "first".to_owned(),
            generation,
        }
        .timed_out(),
    );
    assert!(
        app.pending.is_none(),
        "a refresh timeout still owns the slot after models_refreshed"
    );
    assert_eq!(
        app.flash.as_ref().map(|flash| flash.text.as_str()),
        Some("timed out")
    );
    app.handle_key(key('t'));
    assert!(
        app.take_intents()
            .iter()
            .any(|intent| matches!(intent, ControlIntent::TestProvider { .. })),
        "the timeout must unlock the page"
    );
}

#[test]
fn refresh_error_after_config_reload_releases_the_keyboard() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('m'));
    let generation = refresh_generation(&mut app);
    apply_control_event(&mut app, &EventPayload::ConfigReloaded(Default::default()));
    app.apply_session(SessionOutcome::Models {
        generation,
        id: "first".to_owned(),
        open: false,
        token: 0,
        owns_pending: true,
        result: Err("upstream refused".to_owned()),
        flash: Some(("upstream refused".to_owned(), false)),
    });
    assert!(
        app.pending.is_none(),
        "the refresh error still owns the slot"
    );
    assert_eq!(
        app.flash.as_ref().map(|flash| flash.text.as_str()),
        Some("upstream refused")
    );
    app.handle_key(key('t'));
    assert!(
        app.take_intents()
            .iter()
            .any(|intent| matches!(intent, ControlIntent::TestProvider { .. }))
    );
}

#[test]
fn picker_refresh_event_keeps_the_open_list_and_releases_pending() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    let generation = app.generation();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let token = open_token(&app.take_intents(), "first");
    app.apply_session(open_outcome(
        generation,
        "first",
        token,
        model_view(&["current"], &["current"], &[], &revision(0x33)),
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    let refresh = refresh_generation(&mut app);
    apply_control_event(&mut app, &models_refreshed("first"));
    app.apply_session(refresh_outcome(
        refresh,
        "first",
        model_view(&["stale-refresh"], &["stale-refresh"], &[], &revision(0x44)),
    ));
    assert!(
        app.pending.is_none(),
        "ctrl+r is the same owning refresh as m"
    );
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("current") && !rendered.contains("stale-refresh"),
        "the stale refresh body must not replace the open picker, got {rendered}"
    );
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    app.handle_key(key('t'));
    assert!(
        app.take_intents()
            .iter()
            .any(|intent| matches!(intent, ControlIntent::TestProvider { .. })),
        "escape and the next key must work after the owning refresh completes"
    );
}

#[test]
fn stale_refresh_does_not_release_a_later_mutation() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"], &revision(0x11));
    app.handle_key(key('m'));
    let generation = refresh_generation(&mut app);
    app.apply_session(refresh_outcome(
        generation,
        "first",
        model_view(&["a"], &["a"], &[], &revision(0x33)),
    ));
    assert!(app.pending.is_none());
    apply_control_event(&mut app, &models_refreshed("first"));
    assert!(app.try_start_pending("testing provider"));
    app.apply_session(SessionOutcome::Models {
        generation,
        id: "first".to_owned(),
        open: false,
        token: 0,
        owns_pending: true,
        result: Ok(model_view(&["stale"], &["stale"], &[], &revision(0x33))),
        flash: Some(("obsolete refresh".to_owned(), true)),
    });
    assert!(
        app.pending
            .as_ref()
            .is_some_and(|pending| pending.label == "testing provider"),
        "an older refresh completion must not clear a mutation started after the reload"
    );
    assert_ne!(
        app.flash.as_ref().map(|flash| flash.text.as_str()),
        Some("obsolete refresh"),
        "an older refresh must not replace the flash either"
    );
    let rendered = format!("{:?}", app.providers);
    assert!(
        !rendered.contains("stale"),
        "the older body must stay discarded, got {rendered}"
    );
}
