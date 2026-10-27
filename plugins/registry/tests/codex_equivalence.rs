//! Byte equivalence between shipped compiled Codex and its frozen const oracle.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{
    AgentAdapter, Compiled, ConnectionStatus, DisconnectOutcome, GatewayTarget, Plan, Session,
    TargetOs, legacy,
};
use hofund_plugin_api::{AgentDescription, AgentRoute, FileKind, Slot, Visible};

const USER_CONFIG: &str = r#"# keep this comment
model = "user-model"
temperature = 0.2

[model_providers.other]
name = "Other"
base_url = "https://other.example/v1"
"#;

const STALE_CONFIG: &str = r#"model_provider = "openai"
model = "user-model"

[model_providers.hofund]
name = "stale"
base_url = "https://stale.example/v1"
wire_api = "chat"
experimental_bearer_token = "stale-token"
extra = "keep"
"#;

#[test]
fn shipped_codex_description_matches_frozen_const_fields() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    assert_eq!(description.id().as_str(), "codex");
    assert_eq!(description.display_name(), "Codex CLI");
    assert_eq!(description.binary(), "codex");
    assert_eq!(description.route(), AgentRoute::Gateway);
    assert_eq!(description.visible(), Visible::All);
    assert_eq!(description.model_file(), Some("config"));
    assert_eq!(description.model_key(), Some("model"));
    assert_eq!(description.effort_file(), None);
    assert_eq!(description.effort_key(), None);

    assert_eq!(description.files().len(), 1);
    let file = &description.files()[0];
    assert_eq!(file.id(), "config");
    assert_eq!(file.kind(), FileKind::Toml);
    assert_eq!(file.linux(), "~/.codex/config.toml");
    assert_eq!(file.macos(), "~/.codex/config.toml");
    assert_eq!(file.windows(), "~/.codex/config.toml");
    assert_eq!(file.env_override(), Some("CODEX_HOME"));
    assert_eq!(file.env_replaces(), Some("~/.codex"));
    assert!(file.create());

    let expected = [
        ("model_provider", Slot::Literal("hofund")),
        ("model_providers.hofund.name", Slot::Literal("hofund")),
        ("model_providers.hofund.base_url", Slot::V1),
        (
            "model_providers.hofund.wire_api",
            Slot::Literal("responses"),
        ),
        (
            "model_providers.hofund.experimental_bearer_token",
            Slot::Token,
        ),
    ];
    assert_eq!(description.sets().len(), expected.len());
    for (set, (key, slot)) in description.sets().iter().zip(expected) {
        assert_eq!(set.file(), "config");
        assert_eq!(set.key(), key);
        assert_eq!(set.slot(), &slot);
    }
}

#[derive(Clone, Copy, Debug)]
enum OverrideKind {
    None,
    Absolute,
    Relative,
    Empty,
}

#[derive(Clone, Copy, Debug)]
enum OverrideSource {
    Named,
    Environment,
}

struct Scenario {
    name: &'static str,
    initial: Option<&'static str>,
    override_kind: OverrideKind,
    source: OverrideSource,
}

#[test]
fn shipped_codex_matches_frozen_const_across_the_full_lifecycle() {
    let scenarios = [
        Scenario {
            name: "fresh",
            initial: None,
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "user-file",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::None,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "stale-hofund",
            initial: Some(STALE_CONFIG),
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "absolute-named",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Absolute,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "absolute-env",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Absolute,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "relative-named-with-decoy",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Relative,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "relative-env-with-decoy",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Relative,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "empty-named",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Empty,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "empty-env",
            initial: Some(USER_CONFIG),
            override_kind: OverrideKind::Empty,
            source: OverrideSource::Environment,
        },
    ];
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for scenario in &scenarios {
            compare_scenario(description, os, scenario);
        }
    }
}

fn compare_scenario(description: &'static AgentDescription, os: TargetOs, scenario: &Scenario) {
    let context = format!("{os:?} / {}", scenario.name);
    let legacy_home = scratch();
    let compiled_home = scratch();
    prepare(&legacy_home, scenario);
    prepare(&compiled_home, scenario);
    let before = tree(&legacy_home);
    assert_eq!(before, tree(&compiled_home), "{context}: initial tree");

    let legacy_session = session(&legacy_home, scenario);
    let compiled_session = session(&compiled_home, scenario);
    let legacy_adapter = legacy::codex();
    let compiled = Compiled::new(description).with_os(os);
    let target = gateway_target("local-token");
    assert_eq!(
        legacy_adapter.detect(&legacy_session),
        compiled.detect(&compiled_session),
        "{context}: detect"
    );
    assert!(compiled.detect(&compiled_session), "{context}: fake binary");
    assert_eq!(
        normalized_status(
            &legacy_home,
            &legacy_adapter
                .status(&legacy_session, &target)
                .expect("legacy initial status")
        ),
        normalized_status(
            &compiled_home,
            &compiled
                .status(&compiled_session, &target)
                .expect("compiled initial status")
        ),
        "{context}: initial status"
    );
    assert_eq!(
        legacy_adapter
            .configured_model(&legacy_session)
            .expect("legacy initial model"),
        compiled
            .configured_model(&compiled_session)
            .expect("compiled initial model"),
        "{context}: initial configured model"
    );

    let legacy_plan = legacy_adapter
        .plan(&legacy_session, &target)
        .expect("legacy plan");
    let compiled_plan = compiled
        .plan(&compiled_session, &target)
        .expect("compiled plan");
    assert_plan(
        &legacy_home,
        &compiled_home,
        &legacy_plan,
        &compiled_plan,
        &context,
    );
    assert_eq!(tree(&legacy_home), before, "{context}: legacy plan wrote");
    assert_eq!(
        tree(&compiled_home),
        before,
        "{context}: compiled plan wrote"
    );

    let legacy_applied = legacy_adapter
        .apply(&legacy_session, &target)
        .expect("legacy apply");
    let compiled_applied = compiled
        .apply(&compiled_session, &target)
        .expect("compiled apply");
    assert_plan(
        &legacy_home,
        &compiled_home,
        &legacy_applied,
        &compiled_applied,
        &context,
    );
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: apply and backups"
    );
    assert_eq!(
        normalized_status(
            &legacy_home,
            &legacy_adapter
                .status(&legacy_session, &target)
                .expect("legacy applied status")
        ),
        normalized_status(
            &compiled_home,
            &compiled
                .status(&compiled_session, &target)
                .expect("compiled applied status")
        ),
        "{context}: applied status"
    );

    let legacy_forced = scratch();
    let compiled_forced = scratch();
    copy_tree(&legacy_home, &legacy_forced);
    copy_tree(&compiled_home, &compiled_forced);
    let legacy_clean = scratch();
    let compiled_clean = scratch();

    let rotated = gateway_target("rotated-token");
    let legacy_updated = legacy_adapter
        .refresh_token(&legacy_session, &rotated)
        .expect("legacy refresh");
    let compiled_updated = compiled
        .refresh_token(&compiled_session, &rotated)
        .expect("compiled refresh");
    assert_eq!(
        relative_paths(&legacy_home, &legacy_updated),
        relative_paths(&compiled_home, &compiled_updated),
        "{context}: refresh paths"
    );
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: refresh tree"
    );
    copy_tree(&legacy_home, &legacy_clean);
    copy_tree(&compiled_home, &compiled_clean);

    edit_connected_files(&legacy_home, &legacy_updated);
    edit_connected_files(&compiled_home, &compiled_updated);
    let legacy_drift = legacy_adapter
        .disconnect(&legacy_session, false)
        .expect("legacy drift disconnect");
    let compiled_drift = compiled
        .disconnect(&compiled_session, false)
        .expect("compiled drift disconnect");
    assert_eq!(
        normalized_outcome(&legacy_home, &legacy_drift),
        normalized_outcome(&compiled_home, &compiled_drift),
        "{context}: drift outcome and warnings"
    );
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: drift disconnect tree"
    );
    assert_eq!(
        legacy_adapter
            .configured_model(&legacy_session)
            .expect("legacy final model"),
        compiled
            .configured_model(&compiled_session)
            .expect("compiled final model"),
        "{context}: final configured model"
    );

    let legacy_clean_session = session(&legacy_clean, scenario);
    let compiled_clean_session = session(&compiled_clean, scenario);
    let legacy_clean_outcome = legacy_adapter
        .disconnect(&legacy_clean_session, false)
        .expect("legacy clean disconnect");
    let compiled_clean_outcome = compiled
        .disconnect(&compiled_clean_session, false)
        .expect("compiled clean disconnect");
    assert_eq!(
        normalized_outcome(&legacy_clean, &legacy_clean_outcome),
        normalized_outcome(&compiled_clean, &compiled_clean_outcome),
        "{context}: clean outcome"
    );
    assert_eq!(
        tree(&legacy_clean),
        before,
        "{context}: legacy clean restore"
    );
    assert_eq!(
        tree(&compiled_clean),
        before,
        "{context}: compiled clean restore"
    );

    let legacy_forced_session = session(&legacy_forced, scenario);
    let compiled_forced_session = session(&compiled_forced, scenario);
    let legacy_forced_outcome = legacy_adapter
        .disconnect(&legacy_forced_session, true)
        .expect("legacy forced disconnect");
    let compiled_forced_outcome = compiled
        .disconnect(&compiled_forced_session, true)
        .expect("compiled forced disconnect");
    assert_eq!(
        legacy_forced_outcome, compiled_forced_outcome,
        "{context}: forced outcome"
    );
    assert_eq!(
        tree(&legacy_forced),
        before,
        "{context}: legacy forced restore"
    );
    assert_eq!(
        tree(&compiled_forced),
        before,
        "{context}: compiled forced restore"
    );
}

fn prepare(home: &Path, scenario: &Scenario) {
    let root = if matches!(scenario.override_kind, OverrideKind::Absolute) {
        home.join("absolute-codex")
    } else {
        home.join(".codex")
    };
    if let Some(contents) = scenario.initial {
        write(&root.join("config.toml"), contents);
    }
    if matches!(scenario.override_kind, OverrideKind::Relative) {
        write(
            &home.join("relative-codex/config.toml"),
            "# decoy\nmodel = \"decoy\"\n",
        );
    }
    let binary = if cfg!(windows) { "codex.exe" } else { "codex" };
    write(&home.join("bin").join(binary), "");
}

fn session(home: &Path, scenario: &Scenario) -> Session {
    let session = Session::new(home);
    let session = match scenario.override_kind {
        OverrideKind::None => session,
        OverrideKind::Absolute => {
            set_override(session, home.join("absolute-codex"), scenario.source)
        }
        OverrideKind::Relative => {
            set_override(session, PathBuf::from("relative-codex"), scenario.source)
        }
        OverrideKind::Empty => set_override(session, PathBuf::new(), scenario.source),
    };
    session.with_path(home.join("bin"))
}

fn set_override(session: Session, value: PathBuf, source: OverrideSource) -> Session {
    match source {
        OverrideSource::Named => session.with_codex_home(value),
        OverrideSource::Environment => session.with_env("CODEX_HOME", value),
    }
}

fn gateway_target(token: &str) -> GatewayTarget {
    GatewayTarget::new("http://127.0.0.1:3425/", token, vec!["route-a".to_owned()])
}

fn assert_plan(
    legacy_home: &Path,
    compiled_home: &Path,
    legacy_plan: &Plan,
    compiled_plan: &Plan,
    context: &str,
) {
    fn normalized(home: &Path, plan: &Plan) -> Vec<(PathBuf, Vec<u8>, Vec<u8>)> {
        plan.changes()
            .iter()
            .map(|change| {
                (
                    change
                        .path()
                        .strip_prefix(home)
                        .expect("relative")
                        .to_path_buf(),
                    change.before().to_vec(),
                    change.after().to_vec(),
                )
            })
            .collect()
    }
    assert_eq!(
        normalized(legacy_home, legacy_plan),
        normalized(compiled_home, compiled_plan),
        "{context}: plan paths and bytes"
    );
}

fn normalized_status(home: &Path, status: &ConnectionStatus) -> (bool, bool, Vec<PathBuf>) {
    (
        status.installed(),
        status.connected(),
        relative_paths(home, status.paths()),
    )
}

fn normalized_outcome(home: &Path, outcome: &DisconnectOutcome) -> (bool, Vec<String>) {
    (
        outcome.changed(),
        outcome
            .warnings()
            .iter()
            .map(|warning| warning.replace(&home.to_string_lossy().to_string(), "<home>"))
            .collect(),
    )
}

fn edit_connected_files(home: &Path, paths: &[PathBuf]) {
    for path in paths {
        let text = fs::read_to_string(path).expect("connected TOML");
        let edited = format!(
            "{}user_added = \"keep\"\n",
            text.replace("rotated-token", "user-token")
        );
        let relative = path.strip_prefix(home).expect("relative");
        write(&home.join(relative), &edited);
    }
}

fn relative_paths(home: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.strip_prefix(home).expect("relative").to_path_buf())
        .collect()
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect_files(root, root, &mut files);
    files
}

fn collect_files(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("tree entry").path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
            files.insert(
                path.strip_prefix(root).expect("relative").to_path_buf(),
                fs::read(path).expect("file"),
            );
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    for (relative, bytes) in tree(source) {
        let path = destination.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("copy parent");
        }
        fs::write(path, bytes).expect("copy file");
    }
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hofund-codex-equivalence-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("scratch");
    path
}
