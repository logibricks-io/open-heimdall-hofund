//! Byte equivalence between shipped compiled Codex and its frozen const oracle.

use std::fs;
use std::path::{Path, PathBuf};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Plan, Session, TargetOs, legacy};
use hofund_plugin_api::{AgentDescription, AgentRoute, FileKind, Slot, Visible};
use support::{
    copy_tree, normalized_error, normalized_outcome, normalized_status, relative_paths, scratch,
    tree,
};

mod support;

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

#[test]
fn shipped_codex_invalid_toml_preserves_the_file_without_a_backup() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_path = legacy_home.join(".codex/config.toml");
        let compiled_path = compiled_home.join(".codex/config.toml");
        write(&legacy_path, "model_provider = [\n");
        write(&compiled_path, "model_provider = [\n");
        let before = tree(&legacy_home);
        assert_eq!(before, tree(&compiled_home), "{context}: initial tree");

        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::codex();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");

        let legacy_error = legacy_adapter
            .plan(&legacy_session, &target)
            .expect_err("legacy invalid plan");
        let compiled_error = compiled
            .plan(&compiled_session, &target)
            .expect_err("compiled invalid plan");
        assert_eq!(
            normalized_error(&legacy_home, &legacy_error),
            normalized_error(&compiled_home, &compiled_error),
            "{context}: plan error"
        );
        assert_eq!(tree(&legacy_home), before, "{context}: legacy plan");
        assert_eq!(
            tree(&compiled_home),
            before,
            "{context}: compiled plan wrote or made a backup"
        );

        let legacy_error = legacy_adapter
            .apply(&legacy_session, &target)
            .expect_err("legacy invalid apply");
        let compiled_error = compiled
            .apply(&compiled_session, &target)
            .expect_err("compiled invalid apply");
        assert_eq!(
            normalized_error(&legacy_home, &legacy_error),
            normalized_error(&compiled_home, &compiled_error),
            "{context}: apply error"
        );
        assert_eq!(tree(&legacy_home), before, "{context}: legacy apply");
        assert_eq!(tree(&compiled_home), before, "{context}: compiled apply");
        assert_eq!(
            fs::read(&compiled_path).expect("compiled invalid bytes"),
            b"model_provider = [\n",
            "{context}: invalid bytes changed"
        );
        assert!(
            !compiled_home.join(".codex/config.toml.hofund.bak").exists(),
            "{context}: invalid apply made a backup"
        );
    }
}

#[test]
fn shipped_codex_disconnect_refuses_a_broken_file_until_force() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_path = legacy_home.join(".codex/config.toml");
        let compiled_path = compiled_home.join(".codex/config.toml");
        write(&legacy_path, USER_CONFIG);
        write(&compiled_path, USER_CONFIG);
        let before = tree(&legacy_home);
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::codex();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");
        legacy_adapter
            .apply(&legacy_session, &target)
            .expect("legacy apply");
        compiled
            .apply(&compiled_session, &target)
            .expect("compiled apply");
        assert_eq!(
            tree(&legacy_home),
            tree(&compiled_home),
            "{context}: apply tree"
        );

        write(&legacy_path, "model_provider = [\n");
        write(&compiled_path, "model_provider = [\n");
        let broken = tree(&compiled_home);
        let legacy_error = legacy_adapter
            .disconnect(&legacy_session, false)
            .expect_err("legacy broken disconnect");
        let compiled_error = compiled
            .disconnect(&compiled_session, false)
            .expect_err("compiled broken disconnect");
        assert_eq!(
            normalized_error(&legacy_home, &legacy_error),
            normalized_error(&compiled_home, &compiled_error),
            "{context}: refusal"
        );
        let error = compiled_error.to_string();
        assert!(error.contains("--force"), "{context}: {error}");
        assert!(error.contains("codex"), "{context}: {error}");
        assert_eq!(tree(&legacy_home), broken, "{context}: legacy refusal");
        assert_eq!(
            tree(&compiled_home),
            broken,
            "{context}: refusal changed files"
        );
        assert!(
            compiled_home
                .join(".codex/config.toml.hofund.bak")
                .is_file(),
            "{context}: refusal removed the backup"
        );

        let legacy_outcome = legacy_adapter
            .disconnect(&legacy_session, true)
            .expect("legacy forced disconnect");
        let compiled_outcome = compiled
            .disconnect(&compiled_session, true)
            .expect("compiled forced disconnect");
        assert_eq!(legacy_outcome, compiled_outcome, "{context}: forced");
        assert!(compiled_outcome.changed(), "{context}: forced unchanged");
        assert_eq!(tree(&legacy_home), before, "{context}: legacy restore");
        assert_eq!(
            tree(&compiled_home),
            before,
            "{context}: forced disconnect did not restore the original tree"
        );
        assert_eq!(
            fs::read(&compiled_path).expect("compiled restored bytes"),
            USER_CONFIG.as_bytes(),
            "{context}: restored bytes"
        );
    }
}

#[test]
fn shipped_codex_read_and_revert_cover_existing_and_missing_files() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, initial) in [("existing", Some(USER_CONFIG)), ("missing", None)] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let legacy_path = legacy_home.join(".codex/config.toml");
            let compiled_path = compiled_home.join(".codex/config.toml");
            if let Some(initial) = initial {
                write(&legacy_path, initial);
                write(&compiled_path, initial);
            } else {
                fs::create_dir_all(legacy_path.parent().expect("parent")).expect("legacy parent");
                fs::create_dir_all(compiled_path.parent().expect("parent"))
                    .expect("compiled parent");
            }
            let before = tree(&legacy_home);
            assert_eq!(before, tree(&compiled_home), "{context}: initial tree");
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::codex();
            let compiled = Compiled::new(description).with_os(os);
            let target = gateway_target("local-token");
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");

            let legacy_read = legacy_adapter.read(&legacy_session).expect("legacy read");
            let compiled_read = compiled.read(&compiled_session).expect("compiled read");
            assert_eq!(compiled_read.len(), 1, "{context}: read file count");
            assert_eq!(compiled_read[0].path, compiled_path, "{context}: read path");
            let expected_bytes = fs::read(&compiled_path).expect("compiled applied bytes");
            assert_eq!(
                compiled_read[0].bytes.as_deref(),
                Some(expected_bytes.as_slice()),
                "{context}: read bytes"
            );
            let applied = String::from_utf8(expected_bytes).expect("applied TOML");
            assert!(
                applied.contains("model_provider = \"hofund\""),
                "{context}: {applied}"
            );
            assert!(
                applied.contains("base_url = \"http://127.0.0.1:3425/v1\""),
                "{context}: {applied}"
            );
            assert!(
                applied.contains("experimental_bearer_token = \"local-token\""),
                "{context}: {applied}"
            );
            let normalize = |home: &Path, files: &[hofund_agent::ConfigFile]| {
                files
                    .iter()
                    .map(|file| {
                        (
                            file.path
                                .strip_prefix(home)
                                .expect("relative")
                                .to_path_buf(),
                            file.bytes.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                normalize(&legacy_home, &legacy_read),
                normalize(&compiled_home, &compiled_read),
                "{context}: read equivalence"
            );

            legacy_adapter
                .revert(&legacy_session)
                .expect("legacy revert");
            compiled.revert(&compiled_session).expect("compiled revert");
            assert_eq!(tree(&legacy_home), before, "{context}: legacy revert");
            assert_eq!(
                tree(&compiled_home),
                before,
                "{context}: revert did not restore the initial tree"
            );
            if let Some(initial) = initial {
                assert_eq!(
                    fs::read(&compiled_path).expect("compiled reverted bytes"),
                    initial.as_bytes(),
                    "{context}: reverted bytes"
                );
            } else {
                assert!(!compiled_path.exists(), "{context}: created file remains");
            }
        }
    }
}

#[test]
fn shipped_codex_detects_a_config_file_without_a_binary() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_session = Session::new(&legacy_home).with_path(legacy_home.join("empty-bin"));
        let compiled_session =
            Session::new(&compiled_home).with_path(compiled_home.join("empty-bin"));
        let legacy_adapter = legacy::codex();
        let compiled = Compiled::new(description).with_os(os);
        assert!(!legacy_adapter.detect(&legacy_session), "{context}: legacy");
        assert!(
            !compiled.detect(&compiled_session),
            "{context}: detected an absent binary and config"
        );

        write(&legacy_home.join(".codex/config.toml"), USER_CONFIG);
        write(&compiled_home.join(".codex/config.toml"), USER_CONFIG);
        assert!(legacy_adapter.detect(&legacy_session), "{context}: legacy");
        assert!(
            compiled.detect(&compiled_session),
            "{context}: config alone was not detected"
        );
    }
}

#[test]
fn shipped_codex_disconnect_keeps_a_directory_with_a_user_file() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::codex();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");
        legacy_adapter
            .apply(&legacy_session, &target)
            .expect("legacy apply");
        compiled
            .apply(&compiled_session, &target)
            .expect("compiled apply");
        write(&legacy_home.join(".codex/notes.txt"), "stay\n");
        write(&compiled_home.join(".codex/notes.txt"), "stay\n");

        let legacy_outcome = legacy_adapter
            .disconnect(&legacy_session, false)
            .expect("legacy disconnect");
        let compiled_outcome = compiled
            .disconnect(&compiled_session, false)
            .expect("compiled disconnect");
        assert_eq!(
            normalized_outcome(&legacy_home, &legacy_outcome),
            normalized_outcome(&compiled_home, &compiled_outcome),
            "{context}: outcome"
        );
        assert!(compiled_outcome.changed(), "{context}: unchanged");
        assert!(
            compiled_outcome.warnings().is_empty(),
            "{context}: warnings"
        );
        assert!(!compiled_home.join(".codex/config.toml").exists());
        assert!(compiled_home.join(".codex").is_dir());
        assert_eq!(
            fs::read(compiled_home.join(".codex/notes.txt")).expect("user file"),
            b"stay\n",
            "{context}: user file"
        );
        assert_eq!(
            tree(&legacy_home),
            tree(&compiled_home),
            "{context}: final tree"
        );
    }
}

#[test]
fn shipped_codex_matches_legacy_for_non_utf8_bom_and_crlf_bytes() {
    let description = hofund_plugin_registry::agent("codex").expect("shipped codex");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        compare_non_utf8(description, os);
        let bom = [b"\xef\xbb\xbf".as_slice(), USER_CONFIG.as_bytes()].concat();
        compare_byte_round_trip(description, os, "UTF-8 BOM", &bom);
        let crlf = USER_CONFIG.replace('\n', "\r\n");
        compare_byte_round_trip(description, os, "CRLF", crlf.as_bytes());
    }
}

fn compare_non_utf8(description: &'static AgentDescription, os: TargetOs) {
    let context = format!("{os:?} / non-UTF-8");
    let legacy_home = scratch();
    let compiled_home = scratch();
    let initial = b"model = \"user-model\"\n\xff";
    write_bytes(&legacy_home.join(".codex/config.toml"), initial);
    write_bytes(&compiled_home.join(".codex/config.toml"), initial);
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::codex();
    let compiled = Compiled::new(description).with_os(os);
    let target = gateway_target("local-token");

    let legacy_error = legacy_adapter
        .plan(&legacy_session, &target)
        .expect_err("legacy non-UTF-8 plan");
    let compiled_error = compiled
        .plan(&compiled_session, &target)
        .expect_err("compiled non-UTF-8 plan");
    assert_eq!(
        normalized_error(&legacy_home, &legacy_error),
        normalized_error(&compiled_home, &compiled_error),
        "{context}: plan error"
    );
    assert!(compiled_error.to_string().contains("not UTF-8"));
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: plan tree"
    );
    assert_eq!(tree(&compiled_home), before, "{context}: plan wrote");
    let legacy_error = legacy_adapter
        .apply(&legacy_session, &target)
        .expect_err("legacy non-UTF-8 apply");
    let compiled_error = compiled
        .apply(&compiled_session, &target)
        .expect_err("compiled non-UTF-8 apply");
    assert_eq!(
        normalized_error(&legacy_home, &legacy_error),
        normalized_error(&compiled_home, &compiled_error),
        "{context}: apply error"
    );
    assert!(compiled_error.to_string().contains("not UTF-8"));
    assert_eq!(tree(&legacy_home), before, "{context}: legacy apply");
    assert_eq!(tree(&compiled_home), before, "{context}: apply wrote");

    let legacy_outcome = legacy_adapter
        .disconnect(&legacy_session, false)
        .expect("legacy non-UTF-8 disconnect");
    let compiled_outcome = compiled
        .disconnect(&compiled_session, false)
        .expect("compiled non-UTF-8 disconnect");
    assert_eq!(
        normalized_outcome(&legacy_home, &legacy_outcome),
        normalized_outcome(&compiled_home, &compiled_outcome),
        "{context}: disconnect"
    );
    assert!(!compiled_outcome.changed(), "{context}: unexpected change");
    assert!(compiled_outcome.warnings().is_empty());
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: disconnect tree"
    );
    assert_eq!(tree(&compiled_home), before, "{context}: disconnect wrote");
    assert!(!compiled_home.join(".codex/config.toml.hofund.bak").exists());
}

fn compare_byte_round_trip(
    description: &'static AgentDescription,
    os: TargetOs,
    name: &str,
    initial: &[u8],
) {
    let context = format!("{os:?} / {name}");
    let legacy_home = scratch();
    let compiled_home = scratch();
    let legacy_path = legacy_home.join(".codex/config.toml");
    let compiled_path = compiled_home.join(".codex/config.toml");
    write_bytes(&legacy_path, initial);
    write_bytes(&compiled_path, initial);
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::codex();
    let compiled = Compiled::new(description).with_os(os);
    let target = gateway_target("local-token");

    let legacy_plan = legacy_adapter
        .plan(&legacy_session, &target)
        .expect("legacy byte plan");
    let compiled_plan = compiled
        .plan(&compiled_session, &target)
        .expect("compiled byte plan");
    assert!(!compiled_plan.changes().is_empty(), "{context}: empty plan");
    assert_plan(
        &legacy_home,
        &compiled_home,
        &legacy_plan,
        &compiled_plan,
        &context,
    );
    assert_eq!(tree(&compiled_home), before, "{context}: plan wrote");
    let legacy_applied = legacy_adapter
        .apply(&legacy_session, &target)
        .expect("legacy byte apply");
    let compiled_applied = compiled
        .apply(&compiled_session, &target)
        .expect("compiled byte apply");
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
        "{context}: apply tree"
    );
    assert_eq!(
        fs::read(compiled_home.join(".codex/config.toml.hofund.bak")).expect("backup"),
        initial,
        "{context}: backup bytes"
    );

    let legacy_outcome = legacy_adapter
        .disconnect(&legacy_session, false)
        .expect("legacy byte disconnect");
    let compiled_outcome = compiled
        .disconnect(&compiled_session, false)
        .expect("compiled byte disconnect");
    assert_eq!(
        normalized_outcome(&legacy_home, &legacy_outcome),
        normalized_outcome(&compiled_home, &compiled_outcome),
        "{context}: disconnect"
    );
    assert!(compiled_outcome.changed(), "{context}: unchanged");
    assert!(compiled_outcome.warnings().is_empty());
    assert_eq!(tree(&legacy_home), before, "{context}: legacy restore");
    assert_eq!(
        tree(&compiled_home),
        before,
        "{context}: clean disconnect did not restore the original bytes"
    );
    assert_eq!(
        fs::read(compiled_path).expect("restored bytes"),
        initial,
        "{context}: restored bytes"
    );
}

fn write_bytes(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
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
    fn normalized(home: &Path, plan: &Plan) -> Vec<(PathBuf, Vec<u8>, Vec<u8>, String)> {
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
                    change
                        .diff()
                        .replace(home.to_string_lossy().as_ref(), "<home>"),
                )
            })
            .collect()
    }
    assert_eq!(
        normalized(legacy_home, legacy_plan),
        normalized(compiled_home, compiled_plan),
        "{context}: plan paths, bytes, and diff"
    );
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

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
}
