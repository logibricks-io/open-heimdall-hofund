//! Regression coverage for shipped agents beyond their golden files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Session, TargetOs};
use hofund_plugin_api::{
    AgentDescription, AgentRoute, FileKind, MatchNeedle, Slot, TierId, Visible,
};

const CLAUDE_CONFIG: &str = r#"{
  "model": "user-model",
  "permissions": { "allow": ["Read"] },
  "env": { "KEEP": "yes" }
}
"#;
const CODEX_CONFIG: &str = r#"# keep this comment
model = "user-model"
temperature = 0.2
"#;
const GEMINI_SETTINGS: &str = r#"{
  "model": { "name": "user-model", "temperature": 0.2 },
  "security": { "auth": { "selectedType": "oauth-personal", "userKey": true } }
}
"#;
const GEMINI_ENV: &str =
    "# keep\nKEEP=yes\nGEMINI_API_KEY=user-key\nGOOGLE_GEMINI_BASE_URL=https://user.example\n";
const OPENCODE_CONFIG: &str = r#"{
  "model": "user-model",
  "plugin": ["a"],
  "provider": { "other": { "name": "Other" } }
}
"#;
const OPERATING_SYSTEMS: [TargetOs; 3] = [TargetOs::Linux, TargetOs::Macos, TargetOs::Windows];

#[derive(Clone, Copy)]
struct Spec {
    id: &'static str,
    files: &'static [&'static str],
    contents: &'static [&'static str],
    invalid: &'static str,
}

const SPECS: [Spec; 4] = [
    Spec {
        id: "claude-code",
        files: &[".claude/settings.json"],
        contents: &[CLAUDE_CONFIG],
        invalid: "{\"env\": [\n",
    },
    Spec {
        id: "codex",
        files: &[".codex/config.toml"],
        contents: &[CODEX_CONFIG],
        invalid: "model_provider = [\n",
    },
    Spec {
        id: "gemini-cli",
        files: &[".gemini/settings.json", ".gemini/.env"],
        contents: &[GEMINI_SETTINGS, GEMINI_ENV],
        invalid: "{\"env\": [\n",
    },
    Spec {
        id: "opencode",
        files: &[".config/opencode/opencode.json"],
        contents: &[OPENCODE_CONFIG],
        invalid: "{\"provider\": [\n",
    },
];

fn spec(id: &str) -> Spec {
    SPECS
        .into_iter()
        .find(|spec| spec.id == id)
        .expect("agent spec")
}

fn adapter(id: &str, os: TargetOs) -> Compiled {
    Compiled::new(hofund_plugin_registry::agent(id).expect("shipped agent")).with_os(os)
}

fn target() -> GatewayTarget {
    target_with(&["route-a"])
}

fn target_with(models: &[&str]) -> GatewayTarget {
    GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "local-token",
        models.iter().map(|model| (*model).to_owned()).collect(),
    )
}

#[test]
fn shipped_agent_descriptions_preserve_public_fields() {
    let claude = hofund_plugin_registry::agent("claude-code").expect("claude-code");
    assert_common(claude, "claude-code", "Claude Code", "claude");
    assert_eq!(claude.model_file(), Some("settings"));
    assert_eq!(claude.model_key(), Some("model"));
    assert_eq!(claude.files().len(), 1);
    assert_file(
        &claude.files()[0],
        "settings",
        FileKind::Json,
        "~/.claude/settings.json",
        "CLAUDE_CONFIG_DIR",
        "~/.claude",
        true,
    );
    let tier = |tier| Slot::ModelTier {
        tier: TierId::from_static(tier),
        when_model_matches: None,
    };
    assert_sets(
        claude,
        "settings",
        &[
            ("env.ANTHROPIC_BASE_URL", Slot::Origin),
            ("env.ANTHROPIC_AUTH_TOKEN", Slot::Token),
            ("model", Slot::model_if_unset(None)),
            ("env.ANTHROPIC_DEFAULT_OPUS_MODEL", tier("opus")),
            ("env.ANTHROPIC_DEFAULT_SONNET_MODEL", tier("sonnet")),
            ("env.ANTHROPIC_DEFAULT_HAIKU_MODEL", tier("haiku")),
            (
                "env.ANTHROPIC_DEFAULT_FABLE_MODEL",
                Slot::ModelTier {
                    tier: TierId::from_static("fable"),
                    when_model_matches: Some(MatchNeedle::new("claude-fable-")),
                },
            ),
            ("env.ANTHROPIC_SMALL_FAST_MODEL", tier("fast")),
        ],
    );

    let codex = hofund_plugin_registry::agent("codex").expect("codex");
    assert_common(codex, "codex", "Codex CLI", "codex");
    assert_eq!(codex.model_file(), Some("config"));
    assert_eq!(codex.model_key(), Some("model"));
    assert_eq!(codex.files().len(), 1);
    assert_file(
        &codex.files()[0],
        "config",
        FileKind::Toml,
        "~/.codex/config.toml",
        "CODEX_HOME",
        "~/.codex",
        true,
    );
    assert_sets(
        codex,
        "config",
        &[
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
        ],
    );

    let gemini = hofund_plugin_registry::agent("gemini-cli").expect("gemini-cli");
    assert_common(gemini, "gemini-cli", "Gemini CLI", "gemini");
    assert_eq!(gemini.model_file(), Some("settings"));
    assert_eq!(gemini.model_key(), Some("model.name"));
    assert_eq!(gemini.files().len(), 2);
    for (file, id, kind, name) in [
        (
            &gemini.files()[0],
            "settings",
            FileKind::Json,
            "settings.json",
        ),
        (&gemini.files()[1], "env", FileKind::Env, ".env"),
    ] {
        assert_file(
            file,
            id,
            kind,
            &format!("~/.gemini/{name}"),
            "GEMINI_CLI_HOME",
            "~",
            true,
        );
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
    assert_eq!(gemini.sets().len(), expected.len());
    for (set, (file, key, slot)) in gemini.sets().iter().zip(expected) {
        assert_eq!(set.file(), file);
        assert_eq!(set.key(), key);
        assert_eq!(set.slot(), &slot);
    }

    let opencode = hofund_plugin_registry::agent("opencode").expect("opencode");
    assert_common(opencode, "opencode", "OpenCode", "opencode");
    assert_eq!(opencode.model_file(), Some("config"));
    assert_eq!(opencode.model_key(), Some("model"));
    assert_eq!(opencode.files().len(), 2);
    for (file, name, create) in [
        (&opencode.files()[0], "opencode.jsonc", false),
        (&opencode.files()[1], "opencode.json", true),
    ] {
        assert_file(
            file,
            "config",
            FileKind::Json,
            &format!("~/.config/opencode/{name}"),
            "XDG_CONFIG_HOME",
            "~/.config",
            create,
        );
    }
    assert_sets(
        opencode,
        "config",
        &[
            ("provider.hofund.name", Slot::Literal("hofund")),
            (
                "provider.hofund.npm",
                Slot::Literal("@ai-sdk/openai-compatible"),
            ),
            ("provider.hofund.options.baseURL", Slot::V1),
            ("provider.hofund.options.apiKey", Slot::Token),
            ("provider.hofund.models", Slot::CatalogModels),
        ],
    );
}

fn assert_common(description: &AgentDescription, id: &str, display_name: &str, binary: &str) {
    assert_eq!(description.id().as_str(), id);
    assert_eq!(description.display_name(), display_name);
    assert_eq!(description.binary(), binary);
    assert_eq!(description.route(), AgentRoute::Gateway);
    assert_eq!(description.visible(), Visible::All);
    assert_eq!(description.effort_file(), None);
    assert_eq!(description.effort_key(), None);
}

#[allow(clippy::too_many_arguments)]
fn assert_file(
    file: &hofund_plugin_api::AgentFile,
    id: &str,
    kind: FileKind,
    path: &str,
    env_override: &str,
    env_replaces: &str,
    create: bool,
) {
    assert_eq!(file.id(), id);
    assert_eq!(file.kind(), kind);
    assert_eq!(file.linux(), path);
    assert_eq!(file.macos(), path);
    assert_eq!(file.windows(), path);
    assert_eq!(file.env_override(), Some(env_override));
    assert_eq!(file.env_replaces(), Some(env_replaces));
    assert_eq!(file.create(), create);
}

fn assert_sets(description: &AgentDescription, file: &str, expected: &[(&str, Slot)]) {
    assert_eq!(description.sets().len(), expected.len());
    for (set, (key, slot)) in description.sets().iter().zip(expected) {
        assert_eq!(set.file(), file);
        assert_eq!(set.key(), *key);
        assert_eq!(set.slot(), slot);
    }
}

#[test]
fn shipped_agents_reject_invalid_config_without_backups() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            check_invalid(spec, os, spec.files[0]);
        }
        check_invalid(spec("opencode"), os, ".config/opencode/opencode.jsonc");
    }
}

fn check_invalid(spec: Spec, os: TargetOs, relative: &str) {
    let home = scratch();
    plant(&home, spec);
    write(&home.join(relative), spec.invalid);
    let before = tree(&home);
    let session = Session::new(&home);
    let adapter = adapter(spec.id, os);
    adapter.plan(&session, &target()).expect_err("invalid plan");
    adapter
        .apply(&session, &target())
        .expect_err("invalid apply");
    assert_eq!(tree(&home), before, "{} / {os:?}", spec.id);
    assert_eq!(
        fs::read(home.join(relative)).expect("invalid bytes"),
        spec.invalid.as_bytes()
    );
    assert!(!home.join(format!("{relative}.hofund.bak")).exists());
}

#[test]
fn shipped_agents_disconnect_refuses_broken_files_until_force() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            let home = scratch();
            plant(&home, spec);
            let before = tree(&home);
            let session = Session::new(&home);
            let adapter = adapter(spec.id, os);
            adapter.apply(&session, &target()).expect("apply");
            let path = home.join(spec.files[0]);
            write(&path, spec.invalid);
            let broken = tree(&home);
            let error = adapter
                .disconnect(&session, false)
                .expect_err("broken disconnect")
                .to_string();
            assert!(error.contains("--force"), "{error}");
            assert!(error.contains(spec.id), "{error}");
            assert_eq!(tree(&home), broken);
            assert!(home.join(format!("{}.hofund.bak", spec.files[0])).is_file());
            assert!(adapter.disconnect(&session, true).expect("force").changed());
            assert_eq!(tree(&home), before, "{} / {os:?}", spec.id);
        }
    }
}

#[test]
fn shipped_codex_forced_disconnect_removes_created_config_directory() {
    for os in OPERATING_SYSTEMS {
        let home = scratch();
        let session = Session::new(&home);
        let adapter = adapter("codex", os);
        adapter.apply(&session, &target()).expect("apply");
        let path = home.join(".codex/config.toml");
        assert!(path.is_file());
        write(&path, spec("codex").invalid);
        adapter
            .disconnect(&session, false)
            .expect_err("broken disconnect");

        assert!(adapter.disconnect(&session, true).expect("force").changed());
        assert!(!path.exists(), "{os:?}: config remains");
        assert!(
            !home.join(".codex").exists(),
            "{os:?}: config directory remains"
        );
        assert!(home.is_dir(), "{os:?}: home was removed");
        assert!(tree(&home).is_empty(), "{os:?}: markers remain");
    }
}

#[test]
fn shipped_agents_read_and_revert_cover_existing_and_missing_files() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            for existing in [false, true] {
                let home = scratch();
                if existing {
                    plant(&home, spec);
                }
                let before = tree(&home);
                let session = Session::new(&home);
                let adapter = adapter(spec.id, os);
                adapter.apply(&session, &target()).expect("apply");
                let read = adapter.read(&session).expect("read");
                assert_eq!(read.len(), spec.files.len());
                for (file, relative) in read.iter().zip(spec.files) {
                    assert_eq!(file.path, home.join(relative));
                    assert_eq!(
                        file.bytes.as_deref(),
                        Some(
                            fs::read(home.join(relative))
                                .expect("applied bytes")
                                .as_slice()
                        )
                    );
                }
                adapter.revert(&session).expect("revert");
                if existing {
                    assert_eq!(tree(&home), before, "{} / {os:?}", spec.id);
                } else {
                    for relative in spec.files {
                        assert!(!home.join(relative).exists());
                        assert!(!home.join(format!("{relative}.hofund.absent")).exists());
                    }
                }
            }
        }
    }
}

#[test]
fn shipped_agents_detect_managed_files_without_binaries() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            for (index, relative) in spec.files.iter().enumerate() {
                let home = scratch();
                let session = Session::new(&home).with_path(home.join("empty-bin"));
                let adapter = adapter(spec.id, os);
                assert!(!adapter.detect(&session));
                write(&home.join(relative), spec.contents[index]);
                assert!(adapter.detect(&session), "{} / {relative}", spec.id);
            }
        }
    }
}

#[test]
fn shipped_opencode_detects_jsonc_without_a_binary() {
    for os in OPERATING_SYSTEMS {
        let home = scratch();
        let session = Session::new(&home).with_path(home.join("empty-bin"));
        let adapter = adapter("opencode", os);
        assert!(!adapter.detect(&session));
        write(
            &home.join(".config/opencode/opencode.jsonc"),
            OPENCODE_CONFIG,
        );
        assert!(adapter.detect(&session));
    }
}

#[test]
fn shipped_agents_detect_binaries_without_config_files() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            let home = scratch();
            let binary = if cfg!(windows) {
                format!("{}.exe", description_binary(spec.id))
            } else {
                description_binary(spec.id).to_owned()
            };
            write(&home.join("bin").join(binary), "");
            let session = Session::new(&home).with_path(home.join("bin"));
            assert!(adapter(spec.id, os).detect(&session), "{}", spec.id);
        }
    }
}

fn description_binary(id: &str) -> &'static str {
    hofund_plugin_registry::agent(id)
        .expect("shipped agent")
        .binary()
}

#[test]
fn shipped_agents_disconnect_keeps_directories_with_user_files() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            let home = scratch();
            let session = Session::new(&home);
            let adapter = adapter(spec.id, os);
            adapter.apply(&session, &target()).expect("apply");
            let parent = home
                .join(spec.files[0])
                .parent()
                .expect("parent")
                .to_path_buf();
            write(&parent.join("notes.txt"), "stay\n");
            let outcome = adapter.disconnect(&session, false).expect("disconnect");
            assert!(outcome.changed());
            assert!(outcome.warnings().is_empty());
            for relative in spec.files {
                assert!(!home.join(relative).exists());
            }
            assert_eq!(
                fs::read(parent.join("notes.txt")).expect("notes"),
                b"stay\n"
            );
        }
    }
}

#[test]
fn shipped_agents_disconnect_removes_only_directories_they_created() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            for overridden in [false, true] {
                let home = scratch();
                let session = Session::new(&home);
                let session = if overridden {
                    set_override(session, spec.id, home.join("absolute-root"))
                } else {
                    session
                };
                let adapter = adapter(spec.id, os);
                adapter.apply(&session, &target()).expect("apply");
                let outcome = adapter.disconnect(&session, false).expect("disconnect");
                assert!(outcome.changed());
                assert!(outcome.warnings().is_empty());
                if overridden && spec.id == "opencode" {
                    assert!(home.join("absolute-root").is_dir());
                    assert!(!home.join("absolute-root/opencode").exists());
                } else if overridden {
                    assert!(!home.join("absolute-root").exists(), "{}", spec.id);
                } else {
                    assert!(tree(&home).is_empty(), "{} / {os:?}", spec.id);
                }
            }
        }
    }
}

fn set_override(session: Session, id: &str, value: PathBuf) -> Session {
    match id {
        "claude-code" => session.with_claude_config_dir(value),
        "codex" => session.with_codex_home(value),
        "gemini-cli" => session.with_gemini_cli_home(value),
        "opencode" => session.with_xdg_config_home(value),
        _ => panic!("unknown agent {id}"),
    }
}

#[test]
fn shipped_agents_handle_non_utf8_bom_and_crlf_bytes() {
    for os in OPERATING_SYSTEMS {
        for spec in SPECS {
            for (index, relative) in spec.files.iter().enumerate() {
                let invalid = [spec.contents[index].as_bytes(), b"\xff"].concat();
                check_rejected(spec.id, os, relative, &invalid, "not UTF-8");
                let crlf = spec.contents[index].replace('\n', "\r\n");
                check_round_trip(spec.id, os, relative, crlf.as_bytes());
                let bom = [b"\xef\xbb\xbf".as_slice(), spec.contents[index].as_bytes()].concat();
                if relative.ends_with(".json") {
                    check_rejected(spec.id, os, relative, &bom, "expected value");
                } else {
                    check_round_trip(spec.id, os, relative, &bom);
                }
            }
        }
    }
}

fn check_rejected(id: &str, os: TargetOs, relative: &str, initial: &[u8], expected: &str) {
    let home = scratch();
    write_bytes(&home.join(relative), initial);
    let before = tree(&home);
    let session = Session::new(&home);
    let adapter = adapter(id, os);
    for error in [
        adapter.plan(&session, &target()).expect_err("plan"),
        adapter.apply(&session, &target()).expect_err("apply"),
    ] {
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(tree(&home), before);
    let outcome = adapter.disconnect(&session, false).expect("disconnect");
    assert!(!outcome.changed());
    assert!(outcome.warnings().is_empty());
    assert_eq!(tree(&home), before);
}

fn check_round_trip(id: &str, os: TargetOs, relative: &str, initial: &[u8]) {
    let home = scratch();
    write_bytes(&home.join(relative), initial);
    let before = tree(&home);
    let session = Session::new(&home);
    let adapter = adapter(id, os);
    assert!(!adapter.plan(&session, &target()).expect("plan").is_empty());
    assert_eq!(tree(&home), before);
    adapter.apply(&session, &target()).expect("apply");
    assert_eq!(
        fs::read(home.join(format!("{relative}.hofund.bak"))).expect("backup"),
        initial
    );
    let outcome = adapter.disconnect(&session, false).expect("disconnect");
    assert!(outcome.changed());
    assert!(outcome.warnings().is_empty());
    assert_eq!(tree(&home), before, "{id} / {os:?} / {relative}");
    assert_eq!(fs::read(home.join(relative)).expect("restored"), initial);
}

#[test]
fn shipped_gemini_cli_absolute_override_detection_ignores_default_home_decoys() {
    for os in OPERATING_SYSTEMS {
        for override_file in [None, Some("settings.json"), Some(".env")] {
            let home = scratch();
            write(&home.join(".gemini/settings.json"), GEMINI_SETTINGS);
            write(&home.join(".gemini/.env"), GEMINI_ENV);
            if let Some(name) = override_file {
                write(
                    &home.join("absolute-gemini/.gemini").join(name),
                    if name == ".env" {
                        GEMINI_ENV
                    } else {
                        GEMINI_SETTINGS
                    },
                );
            }
            let session = Session::new(&home)
                .with_gemini_cli_home(home.join("absolute-gemini"))
                .with_path(home.join("empty-bin"));
            assert_eq!(
                adapter("gemini-cli", os).detect(&session),
                override_file.is_some(),
                "{os:?} / {override_file:?}"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_warns_for_user_edits_in_each_file() {
    for os in OPERATING_SYSTEMS {
        for relative in spec("gemini-cli").files {
            let home = scratch();
            plant(&home, spec("gemini-cli"));
            let session = Session::new(&home);
            let adapter = adapter("gemini-cli", os);
            adapter.apply(&session, &target()).expect("apply");
            let path = home.join(relative);
            let text = fs::read_to_string(&path).expect("connected file");
            let edited = if relative.ends_with(".json") {
                text.replace("gemini-api-key", "user-auth")
            } else {
                text.replace("local-token", "user-token")
            };
            write(&path, &edited);
            let outcome = adapter.disconnect(&session, false).expect("disconnect");
            assert!(outcome.changed());
            assert!(!outcome.warnings().is_empty(), "{os:?} / {relative}");
        }
    }
}

#[test]
fn shipped_gemini_cli_status_and_disconnect_restore_when_either_file_is_deleted() {
    for os in OPERATING_SYSTEMS {
        for relative in spec("gemini-cli").files {
            let home = scratch();
            plant(&home, spec("gemini-cli"));
            let before = tree(&home);
            let session = Session::new(&home);
            let adapter = adapter("gemini-cli", os);
            adapter.apply(&session, &target()).expect("apply");
            fs::remove_file(home.join(relative)).expect("delete managed file");
            assert!(
                !adapter
                    .status(&session, &target())
                    .expect("status")
                    .connected()
            );
            assert!(
                adapter
                    .disconnect(&session, false)
                    .expect("disconnect")
                    .changed()
            );
            assert_eq!(tree(&home), before, "{os:?} / {relative}");
        }
    }
}

#[test]
fn shipped_gemini_cli_malformed_env_lines_are_preserved() {
    let initial = b"# keep\r\nnot an assignment\r\n=broken\r\nexport\r\nGEMINI_API_KEY=old\r\n";
    for os in OPERATING_SYSTEMS {
        check_round_trip("gemini-cli", os, ".gemini/.env", initial);
    }
}

#[test]
fn shipped_gemini_cli_read_covers_existing_and_missing_files_before_apply() {
    for os in OPERATING_SYSTEMS {
        for (settings, env) in [
            (None, None),
            (Some(GEMINI_SETTINGS), None),
            (None, Some(GEMINI_ENV)),
            (Some(GEMINI_SETTINGS), Some(GEMINI_ENV)),
        ] {
            let home = scratch();
            if let Some(contents) = settings {
                write(&home.join(".gemini/settings.json"), contents);
            }
            if let Some(contents) = env {
                write(&home.join(".gemini/.env"), contents);
            }
            let read = adapter("gemini-cli", os)
                .read(&Session::new(&home))
                .expect("read");
            assert_eq!(read.len(), 2);
            assert_eq!(read[0].bytes.as_deref(), settings.map(str::as_bytes));
            assert_eq!(read[1].bytes.as_deref(), env.map(str::as_bytes));
        }
    }
}

#[test]
fn shipped_gemini_cli_discriminates_model_sources_and_route_counts() {
    let cases = [
        (
            r#"{"model":"legacy"}"#,
            &["route-a"][..],
            Some(serde_json::json!({"name":"legacy"})),
        ),
        (
            r#"{"model":{"name":"kept"}}"#,
            &["route-a"][..],
            Some(serde_json::json!({"name":"kept"})),
        ),
        (
            r#"{"model":{"name":"kept","extra":true}}"#,
            &["route-a"][..],
            Some(serde_json::json!({"name":"kept","extra":true})),
        ),
        (
            r#"{"model":{"extra":true}}"#,
            &["route-a"][..],
            Some(serde_json::json!({"name":"route-a","extra":true})),
        ),
        (
            r#"{"model":{"extra":true}}"#,
            &["route-a", "route-b"][..],
            Some(serde_json::json!({"extra":true})),
        ),
        (
            r#"{"model":"legacy"}"#,
            &["route-a", "route-b"][..],
            Some(serde_json::json!({"name":"legacy"})),
        ),
        (
            r#"{"ui":{"theme":"dark"}}"#,
            &["route-a", "route-b"][..],
            None,
        ),
    ];
    for os in OPERATING_SYSTEMS {
        for (initial, models, expected) in &cases {
            let home = scratch();
            write(&home.join(".gemini/settings.json"), initial);
            let session = Session::new(&home);
            adapter("gemini-cli", os)
                .apply(&session, &target_with(models))
                .expect("apply");
            let settings: serde_json::Value = serde_json::from_slice(
                &fs::read(home.join(".gemini/settings.json")).expect("settings"),
            )
            .expect("JSON");
            assert_eq!(
                settings.get("model"),
                expected.as_ref(),
                "{os:?} / {initial}"
            );
        }
    }
}

#[test]
fn shipped_gemini_cli_second_apply_string_model_disconnect_requires_force() {
    let initial = "{\n  \"model\": \"user-model\"\n}\n";
    for os in OPERATING_SYSTEMS {
        let home = scratch();
        write(&home.join(".gemini/settings.json"), initial);
        write(&home.join(".gemini/.env"), GEMINI_ENV);
        let before = tree(&home);
        let session = Session::new(&home);
        let adapter = adapter("gemini-cli", os);
        adapter.apply(&session, &target()).expect("first apply");
        assert!(
            adapter
                .apply(&session, &target())
                .expect("second apply")
                .is_empty()
        );
        let connected = tree(&home);
        let error = adapter
            .disconnect(&session, false)
            .expect_err("string-model drift")
            .to_string();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(tree(&home), connected);
        assert!(adapter.disconnect(&session, true).expect("force").changed());
        assert_eq!(tree(&home), before);
    }
}

#[cfg(windows)]
#[test]
fn shipped_agents_honor_windows_mixed_case_environment_overrides() {
    for (id, variable, expected) in [
        (
            "gemini-cli",
            "gemini_cli_home",
            &[
                "absolute-root/.gemini/settings.json",
                "absolute-root/.gemini/.env",
            ][..],
        ),
        (
            "opencode",
            "xdg_config_home",
            &["absolute-root/opencode/opencode.json"][..],
        ),
    ] {
        let home = scratch();
        let session =
            Session::from_env_iter(&home, [(variable, home.join("absolute-root").as_os_str())]);
        let plan = adapter(id, TargetOs::Windows)
            .plan(&session, &target())
            .expect("mixed-case plan");
        let mut paths = plan
            .changes()
            .iter()
            .map(|change| {
                change
                    .path()
                    .strip_prefix(&home)
                    .expect("relative")
                    .to_path_buf()
            })
            .collect::<Vec<_>>();
        paths.sort();
        let mut expected = expected.iter().map(PathBuf::from).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(paths, expected);
    }
}

fn plant(home: &Path, spec: Spec) {
    for (relative, contents) in spec.files.iter().zip(spec.contents) {
        write(&home.join(relative), contents);
    }
}

fn write(path: &Path, contents: &str) {
    write_bytes(path, contents.as_bytes());
}

fn write_bytes(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
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
                collect(root, &path, files);
            } else if path.is_file() {
                files.insert(
                    path.strip_prefix(root).expect("relative").to_path_buf(),
                    fs::read(path).expect("file"),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hofund-agent-regression-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("scratch");
    path
}
