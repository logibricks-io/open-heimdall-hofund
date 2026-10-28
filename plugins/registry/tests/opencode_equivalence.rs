//! Byte equivalence between shipped compiled OpenCode and its frozen const oracle.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{
    AgentAdapter, Compiled, ConnectionStatus, DisconnectOutcome, GatewayTarget, Plan, Session,
    TargetOs, legacy,
};
use hofund_plugin_api::{AgentDescription, AgentRoute, FileKind, Slot, Visible};

const USER_CONFIG: &str = r#"{
  "$schema": "https://opencode.ai/config.json",
  "model": "user-model",
  "plugin": ["a"],
  "provider": { "other": { "name": "Other" } },
  "theme": "dark"
}
"#;

const JSONC_CONFIG: &str = r#"{
  // JSONC comments are dropped by apply and restored by disconnect.
  "model": "user-model",
  "plugin": ["a"],
  "provider": { "other": { "name": "Other" } },
}
"#;

const NO_MODEL_CONFIG: &str = r#"{
  "plugin": ["a"],
  "theme": "dark"
}
"#;

const CONFIGURED_MODEL_CONFIG: &str = r#"{
  "model": "hofund/route-a",
  "plugin": ["a"]
}
"#;

const STALE_CONFIG: &str = r#"{
  "model": "stale-model",
  "provider": {
    "hofund": {
      "name": "stale",
      "npm": "stale-package",
      "options": {
        "baseURL": "https://stale.example/v1",
        "apiKey": "stale-token",
        "extra": "keep"
      },
      "models": { "stale": { "name": "stale" } },
      "extra": "keep"
    }
  }
}
"#;

#[test]
fn shipped_opencode_description_matches_frozen_const_fields() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    assert_eq!(description.id().as_str(), "opencode");
    assert_eq!(description.display_name(), "OpenCode");
    assert_eq!(description.binary(), "opencode");
    assert_eq!(description.route(), AgentRoute::Gateway);
    assert_eq!(description.visible(), Visible::All);
    assert_eq!(description.model_file(), Some("config"));
    assert_eq!(description.model_key(), Some("model"));
    assert_eq!(description.effort_file(), None);
    assert_eq!(description.effort_key(), None);

    assert_eq!(description.files().len(), 2);
    for (file, name, create) in [
        (&description.files()[0], "opencode.jsonc", false),
        (&description.files()[1], "opencode.json", true),
    ] {
        let path = format!("~/.config/opencode/{name}");
        assert_eq!(file.id(), "config");
        assert_eq!(file.kind(), FileKind::Json);
        assert_eq!(file.linux(), path);
        assert_eq!(file.macos(), path);
        assert_eq!(file.windows(), path);
        assert_eq!(file.env_override(), Some("XDG_CONFIG_HOME"));
        assert_eq!(file.env_replaces(), Some("~/.config"));
        assert_eq!(file.create(), create);
    }

    let expected = [
        ("provider.hofund.name", Slot::Literal("hofund")),
        (
            "provider.hofund.npm",
            Slot::Literal("@ai-sdk/openai-compatible"),
        ),
        ("provider.hofund.options.baseURL", Slot::V1),
        ("provider.hofund.options.apiKey", Slot::Token),
        ("provider.hofund.models", Slot::CatalogModels),
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

#[derive(Clone, Copy, Debug)]
enum ConfigPresence {
    Fresh,
    Json,
    Jsonc,
    Both,
}

struct Scenario {
    name: &'static str,
    initial: Option<&'static str>,
    presence: ConfigPresence,
    models: &'static [&'static str],
    override_kind: OverrideKind,
    source: OverrideSource,
}

#[test]
fn shipped_opencode_matches_frozen_const_across_the_full_lifecycle() {
    let scenarios = [
        Scenario {
            name: "fresh",
            initial: None,
            presence: ConfigPresence::Fresh,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "json-user-file",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "jsonc-file",
            initial: Some(JSONC_CONFIG),
            presence: ConfigPresence::Jsonc,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "both-files",
            initial: Some(JSONC_CONFIG),
            presence: ConfigPresence::Both,
            models: &["route-a", "route-b"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "stale-hofund",
            initial: Some(STALE_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "no-routes",
            initial: Some(NO_MODEL_CONFIG),
            presence: ConfigPresence::Json,
            models: &[],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "single-route",
            initial: Some(NO_MODEL_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "several-routes",
            initial: Some(NO_MODEL_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a", "route-b"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "configured-model",
            initial: Some(CONFIGURED_MODEL_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::None,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "absolute-named",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Absolute,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "absolute-env",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Absolute,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "relative-named-with-decoy",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Relative,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "relative-env-with-decoy",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Relative,
            source: OverrideSource::Environment,
        },
        Scenario {
            name: "empty-named",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Empty,
            source: OverrideSource::Named,
        },
        Scenario {
            name: "empty-env",
            initial: Some(USER_CONFIG),
            presence: ConfigPresence::Json,
            models: &["route-a"],
            override_kind: OverrideKind::Empty,
            source: OverrideSource::Environment,
        },
    ];
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for scenario in &scenarios {
            compare_scenario(description, os, scenario);
        }
    }
}

#[test]
fn shipped_opencode_invalid_json_preserves_the_file_without_a_backup() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for name in ["opencode.json", "opencode.jsonc"] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let relative = format!(".config/opencode/{name}");
            let legacy_path = legacy_home.join(&relative);
            let compiled_path = compiled_home.join(&relative);
            write(&legacy_path, "{\"provider\": [\n");
            write(&compiled_path, "{\"provider\": [\n");
            let before = tree(&legacy_home);
            assert_eq!(before, tree(&compiled_home), "{context}: initial tree");

            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::opencode();
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
                b"{\"provider\": [\n",
                "{context}: invalid bytes changed"
            );
            assert!(
                !compiled_home
                    .join(format!("{relative}.hofund.bak"))
                    .exists(),
                "{context}: invalid apply made a backup"
            );
        }
    }
}

#[test]
fn shipped_opencode_disconnect_refuses_a_broken_file_until_force() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_path = legacy_home.join(".config/opencode/opencode.json");
        let compiled_path = compiled_home.join(".config/opencode/opencode.json");
        write(&legacy_path, USER_CONFIG);
        write(&compiled_path, USER_CONFIG);
        let before = tree(&legacy_home);
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::opencode();
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

        write(&legacy_path, "{\"env\": [\n");
        write(&compiled_path, "{\"env\": [\n");
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
        assert!(error.contains("opencode"), "{context}: {error}");
        assert_eq!(tree(&legacy_home), broken, "{context}: legacy refusal");
        assert_eq!(
            tree(&compiled_home),
            broken,
            "{context}: refusal changed files"
        );
        assert!(
            compiled_home
                .join(".config/opencode/opencode.json.hofund.bak")
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
fn shipped_opencode_read_and_revert_cover_existing_and_missing_files() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, initial) in [("existing", Some(USER_CONFIG)), ("missing", None)] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let legacy_path = legacy_home.join(".config/opencode/opencode.json");
            let compiled_path = compiled_home.join(".config/opencode/opencode.json");
            if let Some(initial) = initial {
                write(&legacy_path, initial);
                write(&compiled_path, initial);
            }
            let before = tree(&legacy_home);
            assert_eq!(before, tree(&compiled_home), "{context}: initial tree");
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::opencode();
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
            let applied = String::from_utf8(expected_bytes).expect("applied JSON");
            assert!(
                applied.contains("\"baseURL\": \"http://127.0.0.1:3425/v1\""),
                "{context}: {applied}"
            );
            assert!(
                applied.contains("\"apiKey\": \"local-token\""),
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
            assert_eq!(
                tree(&legacy_home),
                tree(&compiled_home),
                "{context}: revert tree"
            );
            if let Some(initial) = initial {
                assert_eq!(
                    tree(&compiled_home),
                    before,
                    "{context}: revert did not restore the initial tree"
                );
                assert_eq!(
                    fs::read(&compiled_path).expect("compiled reverted bytes"),
                    initial.as_bytes(),
                    "{context}: reverted bytes"
                );
            } else {
                assert!(!compiled_path.exists(), "{context}: created file remains");
                assert!(
                    compiled_home.join(".config/opencode").is_dir(),
                    "{context}: revert removed the empty directory"
                );
            }
        }
    }
}

#[test]
fn shipped_opencode_detects_either_config_file_without_a_binary() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for name in ["opencode.json", "opencode.jsonc"] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let legacy_session =
                Session::new(&legacy_home).with_path(legacy_home.join("empty-bin"));
            let compiled_session =
                Session::new(&compiled_home).with_path(compiled_home.join("empty-bin"));
            let legacy_adapter = legacy::opencode();
            let compiled = Compiled::new(description).with_os(os);
            assert!(!legacy_adapter.detect(&legacy_session), "{context}: legacy");
            assert!(
                !compiled.detect(&compiled_session),
                "{context}: detected an absent binary and config"
            );

            let relative = format!(".config/opencode/{name}");
            write(&legacy_home.join(&relative), USER_CONFIG);
            write(&compiled_home.join(&relative), USER_CONFIG);
            assert!(legacy_adapter.detect(&legacy_session), "{context}: legacy");
            assert!(
                compiled.detect(&compiled_session),
                "{context}: config alone was not detected"
            );
        }
    }
}

#[test]
fn shipped_opencode_detects_a_binary_without_a_config_file() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let binary = if cfg!(windows) {
            "opencode.exe"
        } else {
            "opencode"
        };
        write(&legacy_home.join("bin").join(binary), "");
        write(&compiled_home.join("bin").join(binary), "");
        let legacy_session = Session::new(&legacy_home).with_path(legacy_home.join("bin"));
        let compiled_session = Session::new(&compiled_home).with_path(compiled_home.join("bin"));
        let legacy_adapter = legacy::opencode();
        let compiled = Compiled::new(description).with_os(os);
        assert!(!legacy_home.join(".config/opencode").exists());
        assert!(!compiled_home.join(".config/opencode").exists());
        assert!(legacy_adapter.detect(&legacy_session), "{context}: legacy");
        assert!(
            compiled.detect(&compiled_session),
            "{context}: binary alone was not detected"
        );
    }
}

#[test]
fn shipped_opencode_disconnect_removes_the_empty_directory_it_created() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, override_dir) in [("default", None), ("environment", Some("absolute-xdg"))] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let root = |home: &Path| home.join(override_dir.unwrap_or(".config"));
            assert!(!root(&legacy_home).exists(), "{context}: legacy root");
            assert!(!root(&compiled_home).exists(), "{context}: compiled root");
            let session = |home: &Path| {
                let session = Session::new(home);
                match override_dir {
                    Some(directory) => session.with_env("XDG_CONFIG_HOME", home.join(directory)),
                    None => session,
                }
            };
            let legacy_session = session(&legacy_home);
            let compiled_session = session(&compiled_home);
            let legacy_adapter = legacy::opencode();
            let compiled = Compiled::new(description).with_os(os);
            let target = gateway_target("local-token");
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");
            assert!(root(&legacy_home).join("opencode/opencode.json").is_file());
            assert!(
                root(&compiled_home)
                    .join("opencode/opencode.json")
                    .is_file()
            );

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
            assert!(
                !root(&legacy_home).join("opencode").exists(),
                "{context}: legacy config directory"
            );
            assert!(
                !root(&compiled_home).join("opencode").exists(),
                "{context}: compiled config directory remains"
            );
            assert_eq!(
                tree(&legacy_home),
                tree(&compiled_home),
                "{context}: final tree"
            );
            if override_dir.is_none() {
                assert!(!root(&compiled_home).exists(), "{context}: root remains");
                assert!(tree(&compiled_home).is_empty(), "{context}: final tree");
            } else {
                assert!(root(&compiled_home).is_dir(), "{context}: XDG root");
                assert_eq!(
                    tree(&compiled_home),
                    BTreeMap::from([(PathBuf::from("absolute-xdg/"), Vec::new())]),
                    "{context}: XDG root was not preserved"
                );
            }
        }
    }
}

#[test]
fn shipped_opencode_disconnect_keeps_a_directory_with_a_user_file() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::opencode();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");
        legacy_adapter
            .apply(&legacy_session, &target)
            .expect("legacy apply");
        compiled
            .apply(&compiled_session, &target)
            .expect("compiled apply");
        write(&legacy_home.join(".config/opencode/notes.txt"), "stay\n");
        write(&compiled_home.join(".config/opencode/notes.txt"), "stay\n");

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
        assert!(
            !compiled_home
                .join(".config/opencode/opencode.json")
                .exists()
        );
        assert!(compiled_home.join(".config/opencode").is_dir());
        assert_eq!(
            fs::read(compiled_home.join(".config/opencode/notes.txt")).expect("user file"),
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
fn shipped_opencode_matches_legacy_for_non_utf8_bom_and_crlf_bytes() {
    let description = hofund_plugin_registry::agent("opencode").expect("shipped opencode");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        compare_rejected_bytes(
            description,
            os,
            "non-UTF-8",
            b"{\"model\":\"user-model\"}\n\xff",
            "not UTF-8",
        );
        let bom = [b"\xef\xbb\xbf".as_slice(), USER_CONFIG.as_bytes()].concat();
        compare_rejected_bytes(description, os, "UTF-8 BOM", &bom, "expected value");
        let crlf = USER_CONFIG.replace('\n', "\r\n");
        compare_byte_round_trip(description, os, "CRLF", crlf.as_bytes());
    }
}

fn compare_rejected_bytes(
    description: &'static AgentDescription,
    os: TargetOs,
    name: &str,
    initial: &[u8],
    expected_error: &str,
) {
    let context = format!("{os:?} / {name}");
    let legacy_home = scratch();
    let compiled_home = scratch();
    write_bytes(&legacy_home.join(".config/opencode/opencode.json"), initial);
    write_bytes(
        &compiled_home.join(".config/opencode/opencode.json"),
        initial,
    );
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::opencode();
    let compiled = Compiled::new(description).with_os(os);
    let target = gateway_target("local-token");

    let legacy_error = legacy_adapter
        .plan(&legacy_session, &target)
        .expect_err("legacy rejected plan");
    let compiled_error = compiled
        .plan(&compiled_session, &target)
        .expect_err("compiled rejected plan");
    assert_eq!(
        normalized_error(&legacy_home, &legacy_error),
        normalized_error(&compiled_home, &compiled_error),
        "{context}: plan error"
    );
    assert!(compiled_error.to_string().contains(expected_error));
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: plan tree"
    );
    assert_eq!(tree(&compiled_home), before, "{context}: plan wrote");
    let legacy_error = legacy_adapter
        .apply(&legacy_session, &target)
        .expect_err("legacy rejected apply");
    let compiled_error = compiled
        .apply(&compiled_session, &target)
        .expect_err("compiled rejected apply");
    assert_eq!(
        normalized_error(&legacy_home, &legacy_error),
        normalized_error(&compiled_home, &compiled_error),
        "{context}: apply error"
    );
    assert!(compiled_error.to_string().contains(expected_error));
    assert_eq!(tree(&legacy_home), before, "{context}: legacy apply");
    assert_eq!(tree(&compiled_home), before, "{context}: apply wrote");

    let legacy_outcome = legacy_adapter
        .disconnect(&legacy_session, false)
        .expect("legacy rejected-input disconnect");
    let compiled_outcome = compiled
        .disconnect(&compiled_session, false)
        .expect("compiled rejected-input disconnect");
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
    assert!(
        !compiled_home
            .join(".config/opencode/opencode.json.hofund.bak")
            .exists()
    );
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
    let legacy_path = legacy_home.join(".config/opencode/opencode.json");
    let compiled_path = compiled_home.join(".config/opencode/opencode.json");
    write_bytes(&legacy_path, initial);
    write_bytes(&compiled_path, initial);
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::opencode();
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
        fs::read(compiled_home.join(".config/opencode/opencode.json.hofund.bak")).expect("backup"),
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
    let legacy_adapter = legacy::opencode();
    let compiled = Compiled::new(description).with_os(os);
    let target = gateway_target_with_models("local-token", scenario.models);
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
    if matches!(scenario.presence, ConfigPresence::Both) {
        assert_eq!(
            fs::read(compiled_home.join(".config/opencode/opencode.json"))
                .expect("JSON beside JSONC"),
            b"{\n  \"model\": \"json-model\",\n  \"plugin\": [\"json\"]\n}\n",
            "{context}: JSON changed when JSONC was present"
        );
    }
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

    let rotated = gateway_target_with_models("rotated-token", scenario.models);
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
        home.join("absolute-xdg/opencode")
    } else {
        home.join(".config/opencode")
    };
    if let Some(contents) = scenario.initial {
        let name = match scenario.presence {
            ConfigPresence::Fresh | ConfigPresence::Json => "opencode.json",
            ConfigPresence::Jsonc | ConfigPresence::Both => "opencode.jsonc",
        };
        write(&root.join(name), contents);
        if matches!(scenario.presence, ConfigPresence::Both) {
            write(
                &root.join("opencode.json"),
                "{\n  \"model\": \"json-model\",\n  \"plugin\": [\"json\"]\n}\n",
            );
        }
    }
    if matches!(scenario.override_kind, OverrideKind::Relative) {
        write(
            &home.join("relative-xdg/opencode/opencode.json"),
            "{ \"model\": \"decoy\" }\n",
        );
    }
    let binary = if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    };
    write(&home.join("bin").join(binary), "");
}

fn session(home: &Path, scenario: &Scenario) -> Session {
    let session = Session::new(home);
    let session = match scenario.override_kind {
        OverrideKind::None => session,
        OverrideKind::Absolute => set_override(session, home.join("absolute-xdg"), scenario.source),
        OverrideKind::Relative => {
            set_override(session, PathBuf::from("relative-xdg"), scenario.source)
        }
        OverrideKind::Empty => set_override(session, PathBuf::new(), scenario.source),
    };
    session.with_path(home.join("bin"))
}

fn set_override(session: Session, value: PathBuf, source: OverrideSource) -> Session {
    match source {
        OverrideSource::Named => session.with_xdg_config_home(value),
        OverrideSource::Environment => session.with_env("XDG_CONFIG_HOME", value),
    }
}

fn gateway_target(token: &str) -> GatewayTarget {
    gateway_target_with_models(token, &["route-a"])
}

fn gateway_target_with_models(token: &str, models: &[&str]) -> GatewayTarget {
    GatewayTarget::new(
        "http://127.0.0.1:3425/",
        token,
        models.iter().map(|model| (*model).to_owned()).collect(),
    )
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

fn normalized_error(home: &Path, error: &hofund_agent::AgentError) -> String {
    error
        .to_string()
        .replace(home.to_string_lossy().as_ref(), "<home>")
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
        let text = fs::read_to_string(path)
            .expect("connected JSON")
            .replace("rotated-token", "user-token");
        let prefix = text.strip_suffix("}\n").expect("JSON root object");
        let edited = format!("{},\n  \"user_added\": \"keep\"\n}}\n", prefix.trim_end());
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
            let mut relative = path
                .strip_prefix(root)
                .expect("relative")
                .as_os_str()
                .to_os_string();
            relative.push("/");
            files.insert(PathBuf::from(relative), Vec::new());
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
        let path = destination.join(&relative);
        if relative.as_os_str().to_string_lossy().ends_with('/') {
            fs::create_dir_all(path).expect("copy directory");
            continue;
        }
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
        "hofund-opencode-equivalence-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("scratch");
    path
}
