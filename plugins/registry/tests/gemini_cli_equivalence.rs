//! Byte equivalence between shipped compiled Gemini CLI and its frozen const oracle.

use std::fs;
use std::path::{Path, PathBuf};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Plan, Session, TargetOs, legacy};
use hofund_plugin_api::{AgentDescription, AgentRoute, FileKind, Slot, Visible};
use support::{
    copy_tree, normalized_error, normalized_outcome, normalized_status, relative_paths, scratch,
    tree,
};

mod support;

const USER_SETTINGS: &str = r#"{
  "model": { "name": "user-model", "temperature": 0.2 },
  "theme": "dark",
  "security": { "auth": { "selectedType": "oauth-personal", "userKey": true } }
}
"#;

const USER_ENV: &str =
    "# keep\nKEEP=yes\nGEMINI_API_KEY=user-key\nGOOGLE_GEMINI_BASE_URL=https://user.example\n";

const NO_MODEL_SETTINGS: &str = r#"{
  "theme": "dark",
  "ui": { "showBanner": false }
}
"#;

const STALE_SETTINGS: &str = r#"{
  "model": { "name": "stale-model" },
  "security": { "auth": { "selectedType": "gemini-api-key", "userKey": true } }
}
"#;

const STALE_ENV: &str = "GEMINI_API_KEY=stale-token\nGOOGLE_GEMINI_BASE_URL=https://stale.example\nGEMINI_API_KEY_AUTH_MECHANISM=bearer\nKEEP=yes\n";

const STRING_MODEL_SETTINGS: &str = r#"{
  "model": "user-model",
  "ui": { "theme": "dark" }
}
"#;

const COMMENTED_SETTINGS: &str = r#"{
  // The original bytes return on disconnect.
  "model": { "name": "user-model" },
  "security": { "auth": { "selectedType": "oauth-personal" } },
}
"#;

#[test]
fn shipped_gemini_cli_description_matches_frozen_const_fields() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    assert_eq!(description.id().as_str(), "gemini-cli");
    assert_eq!(description.display_name(), "Gemini CLI");
    assert_eq!(description.binary(), "gemini");
    assert_eq!(description.route(), AgentRoute::Gateway);
    assert_eq!(description.visible(), Visible::All);
    assert_eq!(description.model_file(), Some("settings"));
    assert_eq!(description.model_key(), Some("model.name"));
    assert_eq!(description.effort_file(), None);
    assert_eq!(description.effort_key(), None);

    assert_eq!(description.files().len(), 2);
    for (file, id, kind, name) in [
        (
            &description.files()[0],
            "settings",
            FileKind::Json,
            "settings.json",
        ),
        (&description.files()[1], "env", FileKind::Env, ".env"),
    ] {
        let path = format!("~/.gemini/{name}");
        assert_eq!(file.id(), id);
        assert_eq!(file.kind(), kind);
        assert_eq!(file.linux(), path);
        assert_eq!(file.macos(), path);
        assert_eq!(file.windows(), path);
        assert_eq!(file.env_override(), Some("GEMINI_CLI_HOME"));
        assert_eq!(file.env_replaces(), Some("~"));
        assert!(file.create());
    }

    let expected = [
        (
            "settings",
            "security.auth.selectedType",
            Slot::Literal("gemini-api-key"),
        ),
        ("env", "GEMINI_API_KEY", Slot::Token),
        ("env", "GOOGLE_GEMINI_BASE_URL", Slot::Origin),
        (
            "env",
            "GEMINI_API_KEY_AUTH_MECHANISM",
            Slot::Literal("bearer"),
        ),
        (
            "settings",
            "model.name",
            Slot::model_if_unset(Some("model")),
        ),
    ];
    assert_eq!(description.sets().len(), expected.len());
    for (set, (file, key, slot)) in description.sets().iter().zip(expected) {
        assert_eq!(set.file(), file);
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
    FromEnvIter,
}

struct Scenario {
    name: &'static str,
    settings: Option<&'static str>,
    env: Option<&'static str>,
    models: &'static [&'static str],
    override_kind: OverrideKind,
    source: OverrideSource,
}

fn scenario(
    name: &'static str,
    settings: Option<&'static str>,
    env: Option<&'static str>,
    models: &'static [&'static str],
) -> Scenario {
    Scenario {
        name,
        settings,
        env,
        models,
        override_kind: OverrideKind::None,
        source: OverrideSource::Named,
    }
}

fn override_scenario(name: &'static str, kind: OverrideKind, source: OverrideSource) -> Scenario {
    Scenario {
        override_kind: kind,
        source,
        ..scenario(name, Some(USER_SETTINGS), Some(USER_ENV), &["route-a"])
    }
}

#[test]
fn shipped_gemini_cli_matches_frozen_const_across_the_full_lifecycle() {
    let scenarios = [
        scenario("fresh", None, None, &["route-a"]),
        scenario("settings-only", Some(USER_SETTINGS), None, &["route-a"]),
        scenario("env-only", None, Some(USER_ENV), &["route-a"]),
        scenario(
            "existing-both-user",
            Some(USER_SETTINGS),
            Some(USER_ENV),
            &["route-a"],
        ),
        scenario(
            "existing-stale-hofund",
            Some(STALE_SETTINGS),
            Some(STALE_ENV),
            &["route-a"],
        ),
        scenario(
            "string-model",
            Some(STRING_MODEL_SETTINGS),
            Some(USER_ENV),
            &["route-a"],
        ),
        scenario(
            "existing-model-name",
            Some(USER_SETTINGS),
            Some(USER_ENV),
            &["route-a"],
        ),
        scenario("single-route", Some(NO_MODEL_SETTINGS), None, &["route-a"]),
        scenario(
            "several-routes",
            Some(NO_MODEL_SETTINGS),
            None,
            &["route-a", "route-b"],
        ),
        scenario("no-routes", Some(NO_MODEL_SETTINGS), None, &[]),
        scenario(
            "configured-model",
            Some(USER_SETTINGS),
            Some(USER_ENV),
            &["route-a"],
        ),
        scenario(
            "commented",
            Some(COMMENTED_SETTINGS),
            Some(USER_ENV),
            &["route-a"],
        ),
        override_scenario(
            "absolute-named",
            OverrideKind::Absolute,
            OverrideSource::Named,
        ),
        override_scenario(
            "absolute-env",
            OverrideKind::Absolute,
            OverrideSource::Environment,
        ),
        override_scenario(
            "absolute-from-env-iter",
            OverrideKind::Absolute,
            OverrideSource::FromEnvIter,
        ),
        override_scenario(
            "relative-named",
            OverrideKind::Relative,
            OverrideSource::Named,
        ),
        override_scenario(
            "relative-env",
            OverrideKind::Relative,
            OverrideSource::Environment,
        ),
        override_scenario(
            "relative-from-env-iter",
            OverrideKind::Relative,
            OverrideSource::FromEnvIter,
        ),
        override_scenario("empty-named", OverrideKind::Empty, OverrideSource::Named),
        override_scenario(
            "empty-env",
            OverrideKind::Empty,
            OverrideSource::Environment,
        ),
        override_scenario(
            "empty-from-env-iter",
            OverrideKind::Empty,
            OverrideSource::FromEnvIter,
        ),
    ];
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for scenario in &scenarios {
            compare_scenario(description, os, scenario);
        }
    }
}

#[test]
fn shipped_gemini_cli_invalid_json_preserves_the_file_without_a_backup() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_path = legacy_home.join(".gemini/settings.json");
        let compiled_path = compiled_home.join(".gemini/settings.json");
        write(&legacy_path, "{\"env\": [\n");
        write(&compiled_path, "{\"env\": [\n");
        write(&legacy_home.join(".gemini/.env"), USER_ENV);
        write(&compiled_home.join(".gemini/.env"), USER_ENV);
        let before = tree(&legacy_home);
        assert_eq!(before, tree(&compiled_home), "{context}: initial tree");

        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::gemini_cli();
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
            b"{\"env\": [\n",
            "{context}: invalid bytes changed"
        );
        for relative in [".gemini/settings.json", ".gemini/.env"] {
            assert_eq!(
                fs::read(compiled_home.join(relative)).expect("unchanged invalid input"),
                before
                    .get(&PathBuf::from(relative))
                    .cloned()
                    .expect("before file"),
                "{context}: {relative} bytes changed"
            );
            assert!(
                !compiled_home
                    .join(format!("{relative}.hofund.bak"))
                    .exists(),
                "{context}: invalid apply made a backup for {relative}"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_disconnect_refuses_a_broken_file_until_force() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_path = legacy_home.join(".gemini/settings.json");
        let compiled_path = compiled_home.join(".gemini/settings.json");
        write(&legacy_path, USER_SETTINGS);
        write(&compiled_path, USER_SETTINGS);
        let before = tree(&legacy_home);
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::gemini_cli();
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
        assert!(error.contains("gemini-cli"), "{context}: {error}");
        assert_eq!(tree(&legacy_home), broken, "{context}: legacy refusal");
        assert_eq!(
            tree(&compiled_home),
            broken,
            "{context}: refusal changed files"
        );
        assert!(
            compiled_home
                .join(".gemini/settings.json.hofund.bak")
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
            USER_SETTINGS.as_bytes(),
            "{context}: restored bytes"
        );
    }
}

#[test]
fn shipped_gemini_cli_read_and_revert_cover_existing_and_missing_files() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, initial) in [("existing", Some(USER_SETTINGS)), ("missing", None)] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let legacy_path = legacy_home.join(".gemini/settings.json");
            let compiled_path = compiled_home.join(".gemini/settings.json");
            if let Some(initial) = initial {
                write(&legacy_path, initial);
                write(&compiled_path, initial);
            }
            let before = tree(&legacy_home);
            assert_eq!(before, tree(&compiled_home), "{context}: initial tree");
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::gemini_cli();
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
            assert_eq!(compiled_read.len(), 2, "{context}: read file count");
            assert_eq!(compiled_read[0].path, compiled_path, "{context}: read path");
            let compiled_env_path = compiled_home.join(".gemini/.env");
            assert_eq!(
                compiled_read[1].path, compiled_env_path,
                "{context}: env read path"
            );
            let expected_bytes = fs::read(&compiled_path).expect("compiled applied bytes");
            assert_eq!(
                compiled_read[0].bytes.as_deref(),
                Some(expected_bytes.as_slice()),
                "{context}: read bytes"
            );
            let expected_env = fs::read(&compiled_env_path).expect("compiled applied env");
            assert_eq!(
                compiled_read[1].bytes.as_deref(),
                Some(expected_env.as_slice()),
                "{context}: env read bytes"
            );
            let applied = String::from_utf8(expected_bytes).expect("applied JSON");
            assert!(
                applied.contains("\"selectedType\": \"gemini-api-key\""),
                "{context}: {applied}"
            );
            let applied_env = String::from_utf8(expected_env).expect("applied env");
            assert!(
                applied_env.contains("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3425"),
                "{context}: {applied_env}"
            );
            assert!(
                applied_env.contains("GEMINI_API_KEY=local-token"),
                "{context}: {applied_env}"
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
                    !compiled_home.join(".gemini/.env").exists(),
                    "{context}: created env remains"
                );
                assert!(
                    compiled_home.join(".gemini").is_dir(),
                    "{context}: revert removed the empty directory"
                );
            }
        }
    }
}

#[test]
fn shipped_gemini_cli_detects_either_managed_file_without_a_binary() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, contents) in [("settings.json", USER_SETTINGS), (".env", USER_ENV)] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let legacy_session =
                Session::new(&legacy_home).with_path(legacy_home.join("empty-bin"));
            let compiled_session =
                Session::new(&compiled_home).with_path(compiled_home.join("empty-bin"));
            let legacy_adapter = legacy::gemini_cli();
            let compiled = Compiled::new(description).with_os(os);
            assert!(!legacy_adapter.detect(&legacy_session), "{context}: legacy");
            assert!(
                !compiled.detect(&compiled_session),
                "{context}: detected an absent binary and files"
            );

            write(&legacy_home.join(".gemini").join(name), contents);
            write(&compiled_home.join(".gemini").join(name), contents);
            assert!(legacy_adapter.detect(&legacy_session), "{context}: legacy");
            assert!(
                compiled.detect(&compiled_session),
                "{context}: {name} alone was not detected"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_detects_a_binary_without_a_settings_file() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let binary = if cfg!(windows) {
            "gemini.exe"
        } else {
            "gemini"
        };
        write(&legacy_home.join("bin").join(binary), "");
        write(&compiled_home.join("bin").join(binary), "");
        let legacy_session = Session::new(&legacy_home).with_path(legacy_home.join("bin"));
        let compiled_session = Session::new(&compiled_home).with_path(compiled_home.join("bin"));
        let legacy_adapter = legacy::gemini_cli();
        let compiled = Compiled::new(description).with_os(os);
        assert!(!legacy_home.join(".gemini/settings.json").exists());
        assert!(!compiled_home.join(".gemini/settings.json").exists());
        assert!(legacy_adapter.detect(&legacy_session), "{context}: legacy");
        assert!(
            compiled.detect(&compiled_session),
            "{context}: binary alone was not detected"
        );
    }
}

#[test]
fn shipped_gemini_cli_disconnect_removes_the_empty_directory_it_created() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, override_dir) in [("default", None), ("environment", Some("absolute-gemini"))] {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            let root = |home: &Path| home.join(override_dir.unwrap_or(".gemini"));
            assert!(!root(&legacy_home).exists(), "{context}: legacy root");
            assert!(!root(&compiled_home).exists(), "{context}: compiled root");
            let session = |home: &Path| {
                let session = Session::new(home);
                match override_dir {
                    Some(directory) => session.with_env("GEMINI_CLI_HOME", home.join(directory)),
                    None => session,
                }
            };
            let legacy_session = session(&legacy_home);
            let compiled_session = session(&compiled_home);
            let legacy_adapter = legacy::gemini_cli();
            let compiled = Compiled::new(description).with_os(os);
            let target = gateway_target("local-token");
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");
            let settings = |home: &Path| {
                root(home).join(if override_dir.is_some() {
                    ".gemini/settings.json"
                } else {
                    "settings.json"
                })
            };
            assert!(settings(&legacy_home).is_file());
            assert!(settings(&compiled_home).is_file());
            assert!(settings(&legacy_home).with_file_name(".env").is_file());
            assert!(settings(&compiled_home).with_file_name(".env").is_file());

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
            assert!(!root(&legacy_home).exists(), "{context}: legacy root");
            assert!(
                !root(&compiled_home).exists(),
                "{context}: compiled root remains"
            );
            assert_eq!(
                tree(&legacy_home),
                tree(&compiled_home),
                "{context}: final tree"
            );
            assert!(tree(&compiled_home).is_empty(), "{context}: final tree");
        }
    }
}

#[test]
fn shipped_gemini_cli_disconnect_keeps_a_directory_with_a_user_file() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::gemini_cli();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");
        legacy_adapter
            .apply(&legacy_session, &target)
            .expect("legacy apply");
        compiled
            .apply(&compiled_session, &target)
            .expect("compiled apply");
        write(&legacy_home.join(".gemini/notes.txt"), "stay\n");
        write(&compiled_home.join(".gemini/notes.txt"), "stay\n");

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
        assert!(!compiled_home.join(".gemini/settings.json").exists());
        assert!(compiled_home.join(".gemini").is_dir());
        assert_eq!(
            fs::read(compiled_home.join(".gemini/notes.txt")).expect("user file"),
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
fn shipped_gemini_cli_matches_legacy_for_non_utf8_bom_and_crlf_bytes() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (relative, valid) in [
            (".gemini/settings.json", USER_SETTINGS),
            (".gemini/.env", USER_ENV),
        ] {
            let invalid = [valid.as_bytes(), b"\xff"].concat();
            compare_rejected_bytes(
                description,
                os,
                relative,
                "non-UTF-8",
                &invalid,
                "not UTF-8",
            );
            let crlf = valid.replace('\n', "\r\n");
            compare_byte_round_trip(description, os, relative, "CRLF", crlf.as_bytes());
            let bom = [b"\xef\xbb\xbf".as_slice(), valid.as_bytes()].concat();
            if relative.ends_with(".json") {
                compare_rejected_bytes(
                    description,
                    os,
                    relative,
                    "UTF-8 BOM",
                    &bom,
                    "expected value",
                );
            } else {
                compare_byte_round_trip(description, os, relative, "UTF-8 BOM", &bom);
            }
        }
    }
}

fn compare_rejected_bytes(
    description: &'static AgentDescription,
    os: TargetOs,
    relative: &str,
    name: &str,
    initial: &[u8],
    expected_error: &str,
) {
    let context = format!("{os:?} / {relative} / {name}");
    let legacy_home = scratch();
    let compiled_home = scratch();
    write_bytes(&legacy_home.join(relative), initial);
    write_bytes(&compiled_home.join(relative), initial);
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::gemini_cli();
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
            .join(format!("{relative}.hofund.bak"))
            .exists()
    );
}

fn compare_byte_round_trip(
    description: &'static AgentDescription,
    os: TargetOs,
    relative: &str,
    name: &str,
    initial: &[u8],
) {
    let context = format!("{os:?} / {relative} / {name}");
    let legacy_home = scratch();
    let compiled_home = scratch();
    let legacy_path = legacy_home.join(relative);
    let compiled_path = compiled_home.join(relative);
    write_bytes(&legacy_path, initial);
    write_bytes(&compiled_path, initial);
    let before = tree(&legacy_home);
    let legacy_session = Session::new(&legacy_home);
    let compiled_session = Session::new(&compiled_home);
    let legacy_adapter = legacy::gemini_cli();
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
        fs::read(compiled_home.join(format!("{relative}.hofund.bak"))).expect("backup"),
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

#[cfg(windows)]
#[test]
fn shipped_gemini_cli_windows_mixed_case_override_matches_frozen_const() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    let legacy_home = scratch();
    let compiled_home = scratch();
    let legacy_override = legacy_home.join("absolute-gemini");
    let compiled_override = compiled_home.join("absolute-gemini");
    let legacy_session = Session::from_env_iter(
        &legacy_home,
        [("gemini_cli_home", legacy_override.as_os_str())],
    );
    let compiled_session = Session::from_env_iter(
        &compiled_home,
        [("gemini_cli_home", compiled_override.as_os_str())],
    );
    let target = gateway_target("local-token");
    let legacy_plan = legacy::gemini_cli()
        .plan(&legacy_session, &target)
        .expect("legacy mixed-case plan");
    let compiled_plan = Compiled::new(description)
        .with_os(TargetOs::Windows)
        .plan(&compiled_session, &target)
        .expect("compiled mixed-case plan");
    assert_plan(
        &legacy_home,
        &compiled_home,
        &legacy_plan,
        &compiled_plan,
        "Windows mixed-case override",
    );
    assert!(
        compiled_plan
            .changes()
            .iter()
            .all(|change| change.path().starts_with(&compiled_override))
    );
}

#[test]
fn shipped_gemini_cli_absolute_override_detection_ignores_default_home_decoys() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for override_file in [None, Some("settings.json"), Some(".env")] {
            let context = format!("{os:?} / {override_file:?}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            for home in [&legacy_home, &compiled_home] {
                write(&home.join(".gemini/settings.json"), USER_SETTINGS);
                write(&home.join(".gemini/.env"), USER_ENV);
                if let Some(name) = override_file {
                    write(
                        &home.join("absolute-gemini/.gemini").join(name),
                        if name == ".env" {
                            USER_ENV
                        } else {
                            USER_SETTINGS
                        },
                    );
                }
            }
            let legacy_session = Session::new(&legacy_home)
                .with_gemini_cli_home(legacy_home.join("absolute-gemini"))
                .with_path(legacy_home.join("empty-bin"));
            let compiled_session = Session::new(&compiled_home)
                .with_gemini_cli_home(compiled_home.join("absolute-gemini"))
                .with_path(compiled_home.join("empty-bin"));
            let legacy_detect = legacy::gemini_cli().detect(&legacy_session);
            let compiled_detect = Compiled::new(description)
                .with_os(os)
                .detect(&compiled_session);
            assert_eq!(legacy_detect, compiled_detect, "{context}: detect");
            assert_eq!(
                compiled_detect,
                override_file.is_some(),
                "{context}: default-home decoy affected detection"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_user_edits_in_each_file_have_matching_disconnect_warnings() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for relative in [".gemini/settings.json", ".gemini/.env"] {
            let context = format!("{os:?} / {relative}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            for home in [&legacy_home, &compiled_home] {
                write(&home.join(".gemini/settings.json"), USER_SETTINGS);
                write(&home.join(".gemini/.env"), USER_ENV);
            }
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::gemini_cli();
            let compiled = Compiled::new(description).with_os(os);
            let target = gateway_target("local-token");
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");
            for home in [&legacy_home, &compiled_home] {
                let path = home.join(relative);
                let text = fs::read_to_string(&path).expect("connected file");
                let edited = if relative.ends_with(".json") {
                    text.replace("gemini-api-key", "user-auth")
                } else {
                    text.replace("local-token", "user-token")
                };
                write(&path, &edited);
            }
            let legacy_outcome = legacy_adapter
                .disconnect(&legacy_session, false)
                .expect("legacy edited disconnect");
            let compiled_outcome = compiled
                .disconnect(&compiled_session, false)
                .expect("compiled edited disconnect");
            assert_eq!(
                normalized_outcome(&legacy_home, &legacy_outcome),
                normalized_outcome(&compiled_home, &compiled_outcome),
                "{context}: outcome and warnings"
            );
            assert!(
                !compiled_outcome.warnings().is_empty(),
                "{context}: managed edit produced no warning"
            );
            assert_eq!(
                tree(&legacy_home),
                tree(&compiled_home),
                "{context}: final tree"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_status_and_disconnect_match_when_either_file_is_deleted() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for relative in [".gemini/settings.json", ".gemini/.env"] {
            let context = format!("{os:?} / {relative}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            for home in [&legacy_home, &compiled_home] {
                write(&home.join(".gemini/settings.json"), USER_SETTINGS);
                write(&home.join(".gemini/.env"), USER_ENV);
            }
            let before = tree(&legacy_home);
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let legacy_adapter = legacy::gemini_cli();
            let compiled = Compiled::new(description).with_os(os);
            let target = gateway_target("local-token");
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");
            fs::remove_file(legacy_home.join(relative)).expect("delete legacy file");
            fs::remove_file(compiled_home.join(relative)).expect("delete compiled file");
            assert_eq!(
                normalized_status(
                    &legacy_home,
                    &legacy_adapter
                        .status(&legacy_session, &target)
                        .expect("legacy deleted-file status")
                ),
                normalized_status(
                    &compiled_home,
                    &compiled
                        .status(&compiled_session, &target)
                        .expect("compiled deleted-file status")
                ),
                "{context}: status"
            );
            let legacy_outcome = legacy_adapter
                .disconnect(&legacy_session, false)
                .expect("legacy deleted-file disconnect");
            let compiled_outcome = compiled
                .disconnect(&compiled_session, false)
                .expect("compiled deleted-file disconnect");
            assert_eq!(
                normalized_outcome(&legacy_home, &legacy_outcome),
                normalized_outcome(&compiled_home, &compiled_outcome),
                "{context}: disconnect"
            );
            assert_eq!(tree(&legacy_home), before, "{context}: legacy restore");
            assert_eq!(tree(&compiled_home), before, "{context}: compiled restore");
        }
    }
}

#[test]
fn shipped_gemini_cli_malformed_env_lines_are_preserved_like_the_const() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    let initial = b"# keep\r\nnot an assignment\r\n=broken\r\nexport\r\nGEMINI_API_KEY=old\r\n";
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        compare_byte_round_trip(description, os, ".gemini/.env", "malformed lines", initial);
    }
}

#[test]
fn shipped_gemini_cli_read_covers_existing_and_missing_files_before_apply() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (settings, env) in [
            (None, None),
            (Some(USER_SETTINGS), None),
            (None, Some(USER_ENV)),
            (Some(USER_SETTINGS), Some(USER_ENV)),
        ] {
            let legacy_home = scratch();
            let compiled_home = scratch();
            for home in [&legacy_home, &compiled_home] {
                if let Some(contents) = settings {
                    write(&home.join(".gemini/settings.json"), contents);
                }
                if let Some(contents) = env {
                    write(&home.join(".gemini/.env"), contents);
                }
            }
            let legacy_read = legacy::gemini_cli()
                .read(&Session::new(&legacy_home))
                .expect("legacy read");
            let compiled_read = Compiled::new(description)
                .with_os(os)
                .read(&Session::new(&compiled_home))
                .expect("compiled read");
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
                "{os:?}: read"
            );
            assert_eq!(compiled_read.len(), 2, "{os:?}: file count");
            assert_eq!(compiled_read[0].bytes.is_some(), settings.is_some());
            assert_eq!(compiled_read[1].bytes.is_some(), env.is_some());
        }
    }
}

#[test]
fn shipped_gemini_cli_model_source_and_route_discrimination_matches_the_const() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    let cases = [
        (
            "string-only-one-route",
            r#"{"model":"legacy"}"#,
            &["route-a"][..],
            Some(serde_json::json!({ "name": "legacy" })),
        ),
        (
            "name-only-one-route",
            r#"{"model":{"name":"kept"}}"#,
            &["route-a"][..],
            Some(serde_json::json!({ "name": "kept" })),
        ),
        (
            "name-and-extra-one-route",
            r#"{"model":{"name":"kept","extra":true}}"#,
            &["route-a"][..],
            Some(serde_json::json!({ "name": "kept", "extra": true })),
        ),
        (
            "object-without-name-one-route",
            r#"{"model":{"extra":true}}"#,
            &["route-a"][..],
            Some(serde_json::json!({ "name": "route-a", "extra": true })),
        ),
        (
            "object-without-name-two-routes",
            r#"{"model":{"extra":true}}"#,
            &["route-a", "route-b"][..],
            Some(serde_json::json!({ "extra": true })),
        ),
        (
            "string-two-routes",
            r#"{"model":"legacy"}"#,
            &["route-a", "route-b"][..],
            Some(serde_json::json!({ "name": "legacy" })),
        ),
        (
            "absent-two-routes",
            r#"{"ui":{"theme":"dark"}}"#,
            &["route-a", "route-b"][..],
            None,
        ),
    ];
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        for (name, initial, models, expected) in &cases {
            let context = format!("{os:?} / {name}");
            let legacy_home = scratch();
            let compiled_home = scratch();
            write(&legacy_home.join(".gemini/settings.json"), initial);
            write(&compiled_home.join(".gemini/settings.json"), initial);
            let legacy_session = Session::new(&legacy_home);
            let compiled_session = Session::new(&compiled_home);
            let target = gateway_target_with_models("local-token", models);
            let legacy_plan = legacy::gemini_cli()
                .plan(&legacy_session, &target)
                .expect("legacy model plan");
            let compiled = Compiled::new(description).with_os(os);
            let compiled_plan = compiled
                .plan(&compiled_session, &target)
                .expect("compiled model plan");
            assert_plan(
                &legacy_home,
                &compiled_home,
                &legacy_plan,
                &compiled_plan,
                &context,
            );
            legacy::gemini_cli()
                .apply(&legacy_session, &target)
                .expect("legacy model apply");
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled model apply");
            assert_eq!(tree(&legacy_home), tree(&compiled_home), "{context}: tree");
            let settings: serde_json::Value = serde_json::from_slice(
                &fs::read(compiled_home.join(".gemini/settings.json")).expect("settings"),
            )
            .expect("JSON");
            assert_eq!(
                settings.get("model"),
                expected.as_ref(),
                "{context}: discriminating model value"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_second_apply_string_model_disconnect_matches_the_const() {
    let description = hofund_plugin_registry::agent("gemini-cli").expect("shipped gemini-cli");
    for os in [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows] {
        let context = format!("{os:?}");
        let legacy_home = scratch();
        let compiled_home = scratch();
        for home in [&legacy_home, &compiled_home] {
            write(&home.join(".gemini/settings.json"), STRING_MODEL_SETTINGS);
            write(&home.join(".gemini/.env"), USER_ENV);
        }
        let before = tree(&legacy_home);
        let legacy_session = Session::new(&legacy_home);
        let compiled_session = Session::new(&compiled_home);
        let legacy_adapter = legacy::gemini_cli();
        let compiled = Compiled::new(description).with_os(os);
        let target = gateway_target("local-token");
        legacy_adapter
            .apply(&legacy_session, &target)
            .expect("legacy first apply");
        compiled
            .apply(&compiled_session, &target)
            .expect("compiled first apply");
        assert!(
            legacy_adapter
                .apply(&legacy_session, &target)
                .expect("legacy second apply")
                .is_empty()
        );
        assert!(
            compiled
                .apply(&compiled_session, &target)
                .expect("compiled second apply")
                .is_empty()
        );
        let broken = tree(&legacy_home);
        assert_eq!(broken, tree(&compiled_home), "{context}: applied tree");
        let legacy_error = legacy_adapter
            .disconnect(&legacy_session, false)
            .expect_err("legacy second-apply refusal");
        let compiled_error = compiled
            .disconnect(&compiled_session, false)
            .expect_err("compiled second-apply refusal");
        assert_eq!(
            normalized_error(&legacy_home, &legacy_error),
            normalized_error(&compiled_home, &compiled_error),
            "{context}: refusal"
        );
        assert!(compiled_error.to_string().contains("--force"));
        assert_eq!(tree(&legacy_home), broken, "{context}: legacy refusal");
        assert_eq!(tree(&compiled_home), broken, "{context}: compiled refusal");
        let legacy_outcome = legacy_adapter
            .disconnect(&legacy_session, true)
            .expect("legacy force");
        let compiled_outcome = compiled
            .disconnect(&compiled_session, true)
            .expect("compiled force");
        assert_eq!(legacy_outcome, compiled_outcome, "{context}: forced");
        assert_eq!(tree(&legacy_home), before, "{context}: legacy restore");
        assert_eq!(tree(&compiled_home), before, "{context}: compiled restore");
    }
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
    let legacy_adapter = legacy::gemini_cli();
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
    let legacy_initial_model = legacy_adapter
        .configured_model(&legacy_session)
        .map_err(|error| normalized_error(&legacy_home, &error));
    let compiled_initial_model = compiled
        .configured_model(&compiled_session)
        .map_err(|error| normalized_error(&compiled_home, &error));
    assert_eq!(
        legacy_initial_model, compiled_initial_model,
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
    let managed_root = if matches!(scenario.override_kind, OverrideKind::Absolute) {
        compiled_home.join("absolute-gemini/.gemini")
    } else {
        compiled_home.join(".gemini")
    };
    for (name, original) in [("settings.json", scenario.settings), (".env", scenario.env)] {
        if let Some(original) = original {
            assert_eq!(
                fs::read(managed_root.join(format!("{name}.hofund.bak"))).expect("original backup"),
                original.as_bytes(),
                "{context}: {name} backup"
            );
        }
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
    assert_eq!(compiled_updated.len(), 1, "{context}: refreshed file count");
    assert_eq!(
        compiled_updated[0]
            .file_name()
            .and_then(|name| name.to_str()),
        Some(".env"),
        "{context}: refresh changed a non-env file"
    );
    assert_eq!(
        tree(&legacy_home),
        tree(&compiled_home),
        "{context}: refresh tree"
    );
    copy_tree(&legacy_home, &legacy_clean);
    copy_tree(&compiled_home, &compiled_clean);

    let legacy_root = if matches!(scenario.override_kind, OverrideKind::Absolute) {
        legacy_home.join("absolute-gemini/.gemini")
    } else {
        legacy_home.join(".gemini")
    };
    let legacy_connected = [legacy_root.join("settings.json"), legacy_root.join(".env")];
    let compiled_connected = [
        managed_root.join("settings.json"),
        managed_root.join(".env"),
    ];
    edit_connected_files(&legacy_home, &legacy_connected);
    edit_connected_files(&compiled_home, &compiled_connected);
    if scenario.name == "string-model" {
        let legacy_error = legacy_adapter
            .disconnect(&legacy_session, false)
            .expect_err("legacy string-model drift refusal");
        let compiled_error = compiled
            .disconnect(&compiled_session, false)
            .expect_err("compiled string-model drift refusal");
        assert_eq!(
            normalized_error(&legacy_home, &legacy_error),
            normalized_error(&compiled_home, &compiled_error),
            "{context}: string-model drift error"
        );
        assert!(
            compiled_error
                .to_string()
                .contains("`name` is not an object")
        );
    } else {
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
    }
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
    let parent = if matches!(scenario.override_kind, OverrideKind::Absolute) {
        home.join("absolute-gemini")
    } else {
        home.to_path_buf()
    };
    let root = parent.join(".gemini");
    if let Some(contents) = scenario.settings {
        write(&root.join("settings.json"), contents);
    }
    if let Some(contents) = scenario.env {
        write(&root.join(".env"), contents);
    }
    if matches!(scenario.override_kind, OverrideKind::Absolute) {
        write(
            &home.join(".gemini/settings.json"),
            "{ \"model\": \"default-home-decoy\" }\n",
        );
        write(&home.join(".gemini/.env"), "GEMINI_API_KEY=decoy\n");
    }
    if matches!(scenario.override_kind, OverrideKind::Relative) {
        write(
            &home.join("relative-gemini/.gemini/settings.json"),
            "{ \"model\": \"decoy\" }\n",
        );
        write(
            &home.join("relative-gemini/.gemini/.env"),
            "GEMINI_API_KEY=decoy\n",
        );
    }
    let binary = if cfg!(windows) {
        "gemini.exe"
    } else {
        "gemini"
    };
    write(&home.join("bin").join(binary), "");
}

fn session(home: &Path, scenario: &Scenario) -> Session {
    let session = Session::new(home);
    let session = match scenario.override_kind {
        OverrideKind::None => session,
        OverrideKind::Absolute => {
            set_override(session, home, home.join("absolute-gemini"), scenario.source)
        }
        OverrideKind::Relative => set_override(
            session,
            home,
            PathBuf::from("relative-gemini"),
            scenario.source,
        ),
        OverrideKind::Empty => set_override(session, home, PathBuf::new(), scenario.source),
    };
    session.with_path(home.join("bin"))
}

fn set_override(session: Session, home: &Path, value: PathBuf, source: OverrideSource) -> Session {
    match source {
        OverrideSource::Named => session.with_gemini_cli_home(value),
        OverrideSource::Environment => session.with_env("GEMINI_CLI_HOME", value),
        OverrideSource::FromEnvIter => {
            Session::from_env_iter(home, [("GEMINI_CLI_HOME", value.as_os_str())])
        }
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

fn edit_connected_files(home: &Path, paths: &[PathBuf]) {
    for path in paths {
        let text = fs::read_to_string(path)
            .expect("connected file")
            .replace("rotated-token", "user-token");
        let edited = if path.file_name().is_some_and(|name| name == ".env") {
            format!("{text}user_added=keep\n")
        } else {
            let prefix = text.strip_suffix("}\n").expect("JSON root object");
            format!("{},\n  \"user_added\": \"keep\"\n}}\n", prefix.trim_end())
        };
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
