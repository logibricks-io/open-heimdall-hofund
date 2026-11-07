//! Regressions for the reviewed TUI gaps: a matching key read, read timeouts
//! that do not own the pending slot, picks timeout rollback, a saved hidden
//! list, profile reopen, and optional-network boundaries (the last is in
//! `pages::status`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hofund_control_client::hofund_control_proto::{
    AgentState, AgentStatusEntry, AgentSummary, ModelEntry, PicksResponse, ProfileSummary,
    ProviderKey, ProviderSummary,
};
use hofund_tui::PageId;
use hofund_tui::app::App;
use hofund_tui::intent::ControlIntent;
use hofund_tui::session::{AgentSnapshot, Finished, ModelView, ProviderSnapshot, SessionOutcome};

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty())
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
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

fn load_providers(app: &mut App, ids: &[&str]) {
    let generation = app.generation();
    app.apply_session(SessionOutcome::Providers {
        generation,
        result: Ok(ProviderSnapshot {
            providers: ids.iter().copied().map(provider).collect(),
            presets: Vec::new(),
            disk_revision: revision(0x11),
            presets_error: None,
        }),
    });
}

fn one_key() -> ProviderKey {
    let mut key = ProviderKey::default();
    key.id = "k1".to_owned();
    key
}

fn model_view(ids: &[&str], picks: &[&str], picks_revision: &str) -> ModelView {
    let mut response = PicksResponse::default();
    response.picks = picks.iter().map(|id| (*id).to_owned()).collect();
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

fn open_token(intents: &[ControlIntent]) -> u64 {
    intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::OpenModels { token, .. } => Some(*token),
            _ => None,
        })
        .expect("open")
}

fn keys_token(intents: &[ControlIntent]) -> u64 {
    intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadKeys { token, .. } => Some(*token),
            _ => None,
        })
        .expect("keys")
}

fn open_picker(app: &mut App, ids: &[&str], picks: &[&str], picks_revision: &str) -> u64 {
    let generation = app.generation();
    app.handle_key(enter());
    let token = open_token(&app.take_intents());
    app.apply_session(SessionOutcome::Models {
        generation,
        id: "first".to_owned(),
        open: true,
        token,
        owns_pending: false,
        result: Ok(model_view(ids, picks, picks_revision)),
        flash: None,
    });
    token
}

fn finished(edit: impl FnOnce(&mut Finished)) -> SessionOutcome {
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

fn agent_snapshot() -> AgentSnapshot {
    let mut summary = AgentSummary::default();
    summary.id = "claude".to_owned();
    summary.display_name = "claude".to_owned();
    summary.installed = true;
    let mut status = AgentStatusEntry::default();
    status.id = "claude".to_owned();
    status.state = AgentState::Disconnected;
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

fn hidden_ids(intents: &[ControlIntent]) -> Vec<String> {
    intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::SetHiddenModels { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .expect("hidden write")
}

fn hold_other_pending(app: &mut App) {
    assert!(app.try_start_pending("testing provider"));
}

fn pending_is_the_other(app: &App, what: &str) {
    assert!(
        app.pending
            .as_ref()
            .is_some_and(|pending| pending.label == "testing provider"),
        "{what} must not release the other mutation"
    );
}

#[test]
fn matching_key_read_opens_the_replace_prompt() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"]);
    app.handle_key(key('e'));
    let token = keys_token(&app.take_intents());
    app.apply_session(SessionOutcome::Keys {
        id: "first".to_owned(),
        token,
        result: Ok(vec![one_key()]),
    });
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("new key"),
        "one matching key on the list must open the replace prompt, got {rendered}"
    );
}

#[test]
fn key_read_after_address_keeps_the_address_prompt() {
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
    let token = keys_token(&app.take_intents());
    app.handle_key(key('w'));
    app.apply_session(SessionOutcome::Keys {
        id: "first".to_owned(),
        token,
        result: Ok(vec![one_key()]),
    });
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("address"),
        "the address prompt must stay when the key read matches, got {rendered}"
    );
    assert!(
        !rendered.contains("new key"),
        "a key read must not replace an address prompt that started after it, got {rendered}"
    );
}

#[test]
fn read_timeouts_leave_another_mutations_pending_slot() {
    fn check(name: &str, make: impl FnOnce(u64) -> SessionOutcome) {
        let mut app = App::for_test(PageId::Providers);
        hold_other_pending(&mut app);
        app.apply_session(make(app.generation()));
        pending_is_the_other(&app, name);
    }
    check("providers", |generation| {
        ControlIntent::LoadProviders { generation }.timed_out()
    });
    check("agents", |generation| {
        ControlIntent::LoadAgents { generation }.timed_out()
    });
    check("keys", |_generation| {
        ControlIntent::LoadKeys {
            id: "first".to_owned(),
            token: 1,
        }
        .timed_out()
    });
    check("balances", |_generation| {
        ControlIntent::LoadBalances.timed_out()
    });
    check("profiles", |_generation| {
        ControlIntent::LoadProfiles { token: 1 }.timed_out()
    });
    check("route", |generation| {
        ControlIntent::LoadRouteModels {
            agent: "claude".to_owned(),
            generation,
        }
        .timed_out()
    });
    check("agent models", |generation| {
        ControlIntent::LoadAgentModels {
            agent: "claude".to_owned(),
            generation,
            open: false,
        }
        .timed_out()
    });
}

#[test]
fn picks_timeout_restores_the_enabled_set() {
    let mut app = App::for_test(PageId::Providers);
    load_providers(&mut app, &["first"]);
    open_picker(&mut app, &["a", "b"], &["a", "b"], &revision(0x33));
    app.handle_key(enter());
    let intents = app.take_intents();
    assert!(
        intents
            .iter()
            .any(|intent| matches!(intent, ControlIntent::ReplacePicks { .. })),
        "the toggle must send a picks write, got {intents:?}"
    );
    app.apply_session(
        ControlIntent::ReplacePicks {
            id: "first".to_owned(),
            revision: revision(0x33),
            ids: vec!["b".to_owned()],
        }
        .timed_out(),
    );
    let rendered = format!("{:?}", app.providers);
    assert!(
        rendered.contains("enabled: [\"a\", \"b\"]")
            || rendered.contains("enabled: [\"b\", \"a\"]"),
        "timeout must restore both enabled models, got {rendered}"
    );
}

#[test]
fn profile_save_reopens_the_picker_and_a_later_list_keeps_the_cursor() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    let _ = app.take_intents();
    app.apply_session(finished(|done| done.reload_profiles = true));
    let intents = app.take_intents();
    let token = intents
        .iter()
        .find_map(|intent| match intent {
            ControlIntent::LoadProfiles { token } if *token > 0 => Some(*token),
            _ => None,
        })
        .expect("post-save profile read");
    let rendered = format!("{:?}", app.agents);
    assert!(
        rendered.contains("Profiles"),
        "the post-save read must show the picker, got {rendered}"
    );

    let mut work = ProfileSummary::default();
    work.name = "work".to_owned();
    let mut home = ProfileSummary::default();
    home.name = "home".to_owned();
    app.apply_session(SessionOutcome::Profiles {
        token,
        result: Ok(vec![work, home]),
    });
    app.handle_key(key('j'));
    app.apply_session(SessionOutcome::Profiles {
        token,
        result: Ok(vec![
            {
                let mut profile = ProfileSummary::default();
                profile.name = "work".to_owned();
                profile
            },
            {
                let mut profile = ProfileSummary::default();
                profile.name = "home".to_owned();
                profile
            },
        ]),
    });
    let rendered = format!("{:?}", app.agents);
    assert!(
        rendered.contains("cursor: 1"),
        "a list that is already open must keep its cursor, got {rendered}"
    );
}

#[test]
fn saved_hidden_models_survive_the_next_failed_toggle() {
    let mut app = App::for_test(PageId::Agents);
    load_agents(&mut app);
    let _ = app.take_intents();
    app.handle_key(key('l'));
    app.handle_key(enter());
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
    let mut models = hofund_control_client::hofund_control_proto::AgentModelsResponse::default();
    models.shown = vec!["a".to_owned(), "b".to_owned()];
    app.apply_session(SessionOutcome::AgentModels {
        generation,
        agent: "claude".to_owned(),
        open: true,
        result: Ok(models),
    });
    app.handle_key(key('j'));
    app.handle_key(key('j'));
    app.handle_key(enter());
    assert_eq!(hidden_ids(&app.take_intents()), vec!["a".to_owned()]);
    app.apply_session(finished(|done| {
        done.commit_hidden = Some("claude".to_owned());
        done.flash = ("saved visibility for claude".to_owned(), true);
    }));
    let _ = app.take_intents();
    app.handle_key(key('j'));
    app.handle_key(enter());
    assert_eq!(
        hidden_ids(&app.take_intents()),
        vec!["a".to_owned(), "b".to_owned()]
    );
    app.apply_session(
        ControlIntent::SetHiddenModels {
            agent: "claude".to_owned(),
            ids: vec!["a".to_owned(), "b".to_owned()],
        }
        .timed_out(),
    );
    app.handle_key(enter());
    assert_eq!(
        hidden_ids(&app.take_intents()),
        vec!["a".to_owned(), "b".to_owned()],
        "the saved hide of a must still be in the list after the failed toggle of b"
    );
}
