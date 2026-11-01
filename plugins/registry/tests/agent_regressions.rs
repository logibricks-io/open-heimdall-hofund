//! Regression coverage for shipped agents beyond their golden files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
                "model_providers.hofund.supports_websockets",
                Slot::Json("true"),
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
fn e1_codex_websocket_flag_survives_lifecycle_and_preserves_user_providers() {
    let home = scratch();
    let path = home.join(".codex/config.toml");
    let original = r#"model = "user-model"

[model_providers.other]
name = "Other provider"
base_url = "https://other.example/v1"
supports_websockets = false
extra = true
"#;
    write(&path, original);
    let session = Session::new(&*home);
    let target = target();
    let agent = adapter("codex", TargetOs::Linux);
    let read = || -> toml::Table {
        toml::from_str(&fs::read_to_string(&path).expect("config")).expect("TOML")
    };

    agent.apply(&session, &target).expect("connect");
    assert_eq!(
        read()["model_providers"]["hofund"]["supports_websockets"].as_bool(),
        Some(true)
    );
    assert_eq!(
        read()["model_providers"]["other"],
        toml::from_str::<toml::Table>(original).expect("original")["model_providers"]["other"]
    );
    assert!(agent.status(&session, &target).expect("status").connected());

    let drifted = fs::read_to_string(&path)
        .expect("connected config")
        .replace("supports_websockets = true", "supports_websockets = false");
    write(&path, &drifted);
    assert!(!agent.status(&session, &target).expect("drift").connected());
    agent.apply(&session, &target).expect("repair drift");
    assert_eq!(
        read()["model_providers"]["hofund"]["supports_websockets"].as_bool(),
        Some(true)
    );

    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    agent.set_model(&session, &target, None).expect("clear");
    assert_eq!(
        read()["model_providers"]["hofund"]["supports_websockets"].as_bool(),
        Some(true)
    );
    agent.disconnect(&session, false).expect("disconnect");
    let restored = read();
    assert!(restored["model_providers"].get("hofund").is_none());
    assert_eq!(
        restored["model_providers"]["other"],
        toml::from_str::<toml::Table>(original).expect("original")["model_providers"]["other"]
    );
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
    let session = Session::new(&*home);
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
            let session = Session::new(&*home);
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
        let session = Session::new(&*home);
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
                let session = Session::new(&*home);
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
                let session = Session::new(&*home).with_path(home.join("empty-bin"));
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
        let session = Session::new(&*home).with_path(home.join("empty-bin"));
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
            let session = Session::new(&*home).with_path(home.join("bin"));
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
            let session = Session::new(&*home);
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
                let session = Session::new(&*home);
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
    let session = Session::new(&*home);
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
    let session = Session::new(&*home);
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
            let session = Session::new(&*home)
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
            let session = Session::new(&*home);
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
            let session = Session::new(&*home);
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
                .read(&Session::new(&*home))
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
            let session = Session::new(&*home);
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
fn shipped_gemini_cli_second_apply_string_model_disconnect_restores_the_string_model() {
    let initial = "{\n  \"model\": \"user-model\"\n}\n";
    for os in OPERATING_SYSTEMS {
        let home = scratch();
        write(&home.join(".gemini/settings.json"), initial);
        write(&home.join(".gemini/.env"), GEMINI_ENV);
        let before = tree(&home);
        let session = Session::new(&*home);
        let adapter = adapter("gemini-cli", os);
        adapter.apply(&session, &target()).expect("first apply");
        assert!(
            adapter
                .apply(&session, &target())
                .expect("second apply")
                .is_empty()
        );
        let disconnected = adapter
            .disconnect(&session, false)
            .expect("string-model disconnect");
        assert!(disconnected.changed());
        assert_eq!(tree(&home), before);
        let second = adapter
            .disconnect(&session, false)
            .expect("second string-model disconnect");
        assert!(!second.changed());
        assert_eq!(tree(&home), before);
    }
}

#[test]
fn shipped_gemini_cli_string_model_disconnect_rejects_extra_model_keys_without_writing() {
    let initial = "{\n  \"model\": \"user-model\"\n}\n";
    for os in OPERATING_SYSTEMS {
        let home = scratch();
        let settings_path = home.join(".gemini/settings.json");
        write(&settings_path, initial);
        write(&home.join(".gemini/.env"), GEMINI_ENV);
        let session = Session::new(&*home);
        let adapter = adapter("gemini-cli", os);
        adapter.apply(&session, &target()).expect("apply");

        let mut settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings_path).expect("settings")).expect("JSON");
        settings["model"]
            .as_object_mut()
            .expect("model object")
            .insert("temperature".to_owned(), serde_json::json!(0.2));
        let edited = serde_json::to_vec_pretty(&settings).expect("edited JSON");
        fs::write(&settings_path, &edited).expect("edit settings");
        let before_disconnect = fs::read(&settings_path).expect("edited settings");

        let error = adapter
            .disconnect(&session, false)
            .expect_err("extra model key")
            .to_string();
        assert!(
            error.contains(
                "`model` must contain only a string `name` to restore the original string; move other keys out of `model` before disconnect"
            ),
            "{error}"
        );
        assert!(error.contains("--force"), "{error}");
        assert_eq!(
            fs::read(&settings_path).expect("settings after refusal"),
            before_disconnect
        );
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
            Session::from_env_iter(&*home, [(variable, home.join("absolute-root").as_os_str())]);
        let plan = adapter(id, TargetOs::Windows)
            .plan(&session, &target())
            .expect("mixed-case plan");
        let mut paths = plan
            .changes()
            .iter()
            .map(|change| {
                change
                    .path()
                    .strip_prefix(&*home)
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

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

fn scratch() -> Scratch {
    Scratch(tempfile::tempdir().expect("scratch"))
}

#[test]
fn shipped_vscode_clear_restores_previous_default_model() {
    let home = scratch();
    let session = Session::new(&*home);
    let dir = home.join(".config").join("Code").join("User");
    let models = dir.join("chatLanguageModels.json");
    let settings = dir.join("settings.json");
    // VS Code writes JSONC: comments and trailing commas are allowed.
    let models_text =
        "[\n  // mine\n  { \"vendor\": \"ollama\", \"name\": \"local\", \"models\": [] },\n]\n";
    let settings_text = "{\n  \"chat.defaultModel\": \"mine\",\n  \"chat\": { \"agent\": { \"enabled\": true } },\n}\n";
    fs::create_dir_all(&dir).expect("dir");
    fs::write(&models, models_text).expect("seed models");
    fs::write(&settings, settings_text).expect("seed settings");
    let target = target();
    let agent = adapter("vscode", TargetOs::Linux);
    let plan = agent.apply(&session, &target).expect("apply");
    assert_eq!(plan.changes().len(), 1, "{plan:?}");
    let read = |path: &Path| -> serde_json::Value {
        hofund_kit_agent::parse_jsonc(
            std::str::from_utf8(&fs::read(path).expect("read")).expect("UTF-8"),
        )
    };
    let groups = read(&models);
    assert!(
        fs::read_to_string(&models)
            .expect("models")
            .contains("// mine")
    );
    assert_eq!(groups[0]["name"], "local");
    assert_eq!(groups[1]["name"], "hofund");
    assert_eq!(
        groups[1]["models"][0]["requestHeaders"]["Authorization"],
        format!("Bearer {}", target.token())
    );
    assert_eq!(
        fs::read_to_string(&settings).expect("settings"),
        settings_text
    );
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let json = read(&settings);
    assert_eq!(json["chat.defaultModel"], "route-a");
    assert_eq!(
        json["chat"],
        serde_json::json!({ "agent": { "enabled": true } })
    );
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("route-a")
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    agent.set_model(&session, &target, None).expect("clear");
    let json = read(&settings);
    assert_eq!(json["chat.defaultModel"], "mine");
    assert_eq!(
        json["chat"],
        serde_json::json!({ "agent": { "enabled": true } })
    );
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(&models).expect("models"), models_text);
    assert_eq!(
        fs::read_to_string(&settings).expect("settings"),
        settings_text
    );
}

#[test]
fn shipped_vscode_clear_without_previous_default_model_removes_key() {
    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".config/Code/User/settings.json");
    write(&settings, "{\"editor.fontSize\": 14}\n");
    let target = target();
    let agent = adapter("vscode", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    agent.set_model(&session, &target, None).expect("clear");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(json.get("chat.defaultModel"), None);
    assert_eq!(json["editor.fontSize"], 14);
}

#[test]
fn shipped_vscode_clear_with_an_empty_previous_default_model_removes_key() {
    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".config/Code/User/settings.json");
    write(
        &settings,
        "{\"chat.defaultModel\": \"\", \"editor.fontSize\": 14}\n",
    );
    let target = target();
    let agent = adapter("vscode", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    agent.set_model(&session, &target, None).expect("clear");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(json.get("chat.defaultModel"), None);
    assert_eq!(json["editor.fontSize"], 14);
}

#[test]
fn shipped_vscode_clear_does_not_restore_a_previous_gateway_model() {
    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".config/Code/User/settings.json");
    write(
        &settings,
        "{\"chat.defaultModel\": \"route-a\", \"editor.fontSize\": 14}\n",
    );
    let target = target_with(&["route-a", "route-b"]);
    let agent = adapter("vscode", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-b"))
        .expect("pick");
    agent.set_model(&session, &target, None).expect("clear");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(json.get("chat.defaultModel"), None);
    assert_eq!(json["editor.fontSize"], 14);
}

#[test]
fn shipped_vscode_clear_then_disconnect_restores_once() {
    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".config/Code/User/settings.json");
    write(&settings, "{\"chat.defaultModel\": \"mine\"}\n");
    let target = target();
    let agent = adapter("vscode", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    agent.set_model(&session, &target, None).expect("clear");
    let mut edited: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(edited["chat.defaultModel"], "mine");
    edited["note"] = serde_json::json!("keep");
    write(&settings, &edited.to_string());
    agent.disconnect(&session, false).expect("disconnect");
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(restored["chat.defaultModel"], "mine");
    assert_eq!(restored["note"], "keep");
    assert!(!agent.disconnect(&session, false).expect("again").changed());
}

#[test]
fn shipped_vscode_creates_settings_only_for_a_pick() {
    let home = scratch();
    let session = Session::new(&*home);
    let dir = home.join(".config").join("Code").join("User");
    let settings = dir.join("settings.json");
    let target = target();
    let agent = adapter("vscode", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    assert!(dir.join("chatLanguageModels.json").is_file());
    assert!(!settings.exists());
    assert!(agent.status(&session, &target).expect("status").connected());
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).expect("read")).expect("json");
    assert_eq!(json["chat.defaultModel"], "route-a");
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!settings.exists());
    assert!(!dir.join("chatLanguageModels.json").exists());
}

#[test]
fn shipped_qwen_pick_points_the_session_at_the_gateway_entry() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".qwen").join("settings.json");
    let original = r#"{
  "security": { "auth": { "selectedType": "qwen-oauth" } },
  "model": { "name": "mine", "baseUrl": "https://mine.example.test/v1" },
  "modelProviders": { "openai": [
    { "baseUrl": "https://mine.example.test/v1", "envKey": "MY_KEY", "id": "route-a", "name": "mine" }
  ] }
}
"#;
    fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    fs::write(&path, original).expect("seed");
    let target = target();
    let agent = adapter("qwen", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    let read = || -> serde_json::Value {
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json")
    };
    assert_eq!(read()["security"]["auth"]["selectedType"], "qwen-oauth");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let json = read();
    // The user's entry of the same id comes first; the base URL picks hofund's.
    assert_eq!(json["model"]["name"], "route-a");
    assert_eq!(json["model"]["baseUrl"], "http://127.0.0.1:3425/v1");
    assert_eq!(json["security"]["auth"]["selectedType"], "openai");
    assert!(agent.status(&session, &target).expect("status").connected());
    agent.set_model(&session, &target, None).expect("clear");
    let json = read();
    assert_eq!(json["model"]["name"], "mine");
    assert_eq!(json["model"]["baseUrl"], "https://mine.example.test/v1");
    assert_eq!(json["security"]["auth"]["selectedType"], "qwen-oauth");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(
        read(),
        serde_json::from_str::<serde_json::Value>(original).expect("original")
    );
}

#[test]
fn shipped_droid_clear_restores_previous_default_model() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".factory/settings.json");
    write(
        &path,
        "{\"sessionDefaultSettings\":{\"model\":\"custom:mine\"},\"theme\":\"dark\"}\n",
    );
    let target = target();
    let agent = adapter("droid", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let picked: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    assert_eq!(
        picked["sessionDefaultSettings"]["model"],
        "custom:hofund/route-a"
    );
    agent.set_model(&session, &target, None).expect("clear");
    let cleared: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    assert_eq!(cleared["sessionDefaultSettings"]["model"], "custom:mine");
    assert_eq!(cleared["theme"], "dark");
}

const CLINE_PROVIDERS: &str = r#"{
  "version": 1,
  "lastUsedProvider": "anthropic",
  "providers": {
    "anthropic": {
      "settings": { "provider": "anthropic", "apiKey": "user-anthropic-key", "model": "claude-sonnet-4-5" },
      "updatedAt": "2026-03-01T10:00:00.000Z",
      "tokenSource": "manual"
    },
    "openai-compatible": {
      "settings": { "provider": "openai-compatible", "apiKey": "user-key", "model": "my-model", "baseUrl": "http://localhost:8000/v1" },
      "updatedAt": "2026-03-02T10:00:00.000Z",
      "tokenSource": "manual"
    }
  }
}
"#;

#[test]
fn shipped_cline_pick_takes_over_openai_compatible_and_clear_gives_it_back() {
    let home = scratch();
    let session = Session::new(&*home);
    let dir = home.join(".cline").join("data").join("settings");
    let providers = dir.join("providers.json");
    write(&providers, CLINE_PROVIDERS);
    let target = target();
    let agent = adapter("cline", TargetOs::Linux);
    let read = || -> serde_json::Value {
        serde_json::from_slice(&fs::read(&providers).expect("read")).expect("json")
    };
    let original = read();
    agent.apply(&session, &target).expect("apply");
    assert_eq!(read(), original);
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("my-model")
    );
    // The user's own model of a route's name points elsewhere: a refresh
    // leaves their entry alone.
    agent.refresh_token(&session, &target).expect("refresh");
    assert_eq!(read(), original);

    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let json = read();
    assert_eq!(json["version"], 1);
    assert_eq!(json["lastUsedProvider"], "openai-compatible");
    assert_eq!(
        json["providers"]["anthropic"],
        original["providers"]["anthropic"]
    );
    let entry = &json["providers"]["openai-compatible"];
    assert_eq!(
        entry["settings"],
        serde_json::json!({
            "provider": "openai-compatible",
            "apiKey": target.token(),
            "baseUrl": "http://127.0.0.1:3425/v1",
            "headers": { "User-Agent": "cline" },
            "model": "route-a"
        })
    );
    assert_eq!(entry["updatedAt"], "2026-01-01T00:00:00.000Z");
    assert_eq!(entry["tokenSource"], "manual");
    assert!(agent.status(&session, &target).expect("status").connected());
    // A second connect keeps the pick as it is.
    let picked = read();
    agent.apply(&session, &target).expect("apply again");
    assert_eq!(read(), picked);

    // A key the user adds inside the entry survives a refresh, which only
    // swaps the token.
    let mut edited = read();
    edited["providers"]["openai-compatible"]["settings"]["reasoning"] =
        serde_json::json!({ "effort": "high" });
    write(&providers, &edited.to_string());
    let fresh = GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "fresh-token",
        vec!["route-a".to_owned()],
    );
    agent.refresh_token(&session, &fresh).expect("refresh");
    let mut expected = edited.clone();
    expected["providers"]["openai-compatible"]["settings"]["apiKey"] =
        serde_json::json!("fresh-token");
    assert_eq!(read(), expected);

    // Once the entry points elsewhere it is the user's again.
    let mut moved = read();
    moved["providers"]["openai-compatible"]["settings"]["baseUrl"] =
        serde_json::json!("http://localhost:9000/v1");
    write(&providers, &moved.to_string());
    let later = GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "later-token",
        vec!["route-a".to_owned()],
    );
    agent.refresh_token(&session, &later).expect("refresh");
    assert_eq!(read(), moved);

    // So it is once the user picks a model that is not a route.
    let mut own = expected.clone();
    own["providers"]["openai-compatible"]["settings"]["model"] = serde_json::json!("my-model");
    write(&providers, &own.to_string());
    agent.refresh_token(&session, &later).expect("refresh");
    assert_eq!(read(), own);

    // An entry without a key is not hofund's either; a refresh adds none.
    let mut keyless = expected.clone();
    keyless["providers"]["openai-compatible"]["settings"]
        .as_object_mut()
        .expect("settings")
        .remove("apiKey");
    write(&providers, &keyless.to_string());
    agent.refresh_token(&session, &later).expect("refresh");
    assert_eq!(read(), keyless);
    write(&providers, &expected.to_string());

    agent.set_model(&session, &target, None).expect("clear");
    assert_eq!(read(), original);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(read(), original);
    assert!(!dir.join("models.json").exists());
}

#[test]
fn shipped_cline_writes_providers_only_for_a_pick() {
    let home = scratch();
    let session = Session::new(&*home);
    let dir = home.join(".cline").join("data").join("settings");
    let providers = dir.join("providers.json");
    let target = target();
    let agent = adapter("cline", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    assert!(dir.join("models.json").is_file());
    assert!(!providers.exists());
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&providers).expect("read")).expect("json");
    assert_eq!(json["version"], 1);
    assert_eq!(json["lastUsedProvider"], "openai-compatible");
    assert_eq!(
        json["providers"]["openai-compatible"]["settings"]["model"],
        "route-a"
    );
    agent.set_model(&session, &target, None).expect("clear");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(&providers).expect("read")).expect("json");
    assert_eq!(json.get("version"), None);
    assert_eq!(json.get("lastUsedProvider"), None);
    assert_eq!(json, serde_json::json!({}));
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!providers.exists());
    assert!(!dir.join("models.json").exists());
}

const KIMI_CONFIG: &str = r#"# my model choice
default_model = "kimi-code/kimi-for-coding"

# mine
[providers."managed:kimi-code"]
type = "kimi"
base_url = "https://api.kimi.com/coding/v1"
api_key = "user-key"

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
max_context_size = 262144
"#;

#[test]
fn shipped_kimi_clear_restores_default_model_when_table_exists() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".kimi-code").join("config.toml");
    fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    fs::write(&path, KIMI_CONFIG).expect("seed");
    let target = target();
    let agent = adapter("kimi", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    assert!(
        fs::read_to_string(&path)
            .expect("apply")
            .contains("# my model choice")
    );
    let read = || -> toml::Table {
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml")
    };
    let config = read();
    assert_eq!(
        config["default_model"].as_str(),
        Some("kimi-code/kimi-for-coding")
    );
    let route = &config["models"]["hofund/route-a"];
    assert_eq!(route["provider"].as_str(), Some("hofund"));
    assert_eq!(route["model"].as_str(), Some("route-a"));
    assert_eq!(route["max_context_size"].as_integer(), Some(128000));
    assert_eq!(
        config["providers"]["hofund"]["base_url"].as_str(),
        Some("http://127.0.0.1:3425/v1")
    );
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    assert!(
        fs::read_to_string(&path)
            .expect("pick")
            .contains("# my model choice")
    );
    assert_eq!(read()["default_model"].as_str(), Some("hofund/route-a"));
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("hofund/route-a")
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    agent.set_model(&session, &target, None).expect("clear");
    assert_eq!(
        read()["default_model"].as_str(),
        Some("kimi-code/kimi-for-coding")
    );
    let cleared_text = fs::read_to_string(&path).expect("clear");
    assert!(cleared_text.contains("# my model choice"), "{cleared_text}");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    assert!(
        fs::read_to_string(&path)
            .expect("pick")
            .contains("# my model choice")
    );
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(&path).expect("read"), KIMI_CONFIG);
}

#[test]
fn shipped_kimi_clear_drops_default_model_when_table_is_gone() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".kimi-code").join("config.toml");
    write(&path, KIMI_CONFIG);
    let target = target();
    let agent = adapter("kimi", TargetOs::Linux);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let without_user_model = KIMI_CONFIG
        .split("[models.\"kimi-code/kimi-for-coding\"]")
        .next()
        .expect("prefix");
    write(&path, without_user_model);
    agent.set_model(&session, &target, None).expect("clear");
    let config: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
    assert!(!config.contains_key("default_model"));
}

#[test]
fn shipped_kimi_status_reports_stale_hofund_default() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".kimi-code").join("config.toml");
    write(&path, KIMI_CONFIG);
    let agent = adapter("kimi", TargetOs::Linux);
    agent
        .set_model(
            &session,
            &target_with(&["route-a", "route-b"]),
            Some("route-a"),
        )
        .expect("pick");
    let remaining = target_with(&["route-b"]);
    agent.apply(&session, &remaining).expect("route removed");
    let config: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
    assert_eq!(config["default_model"].as_str(), Some("hofund/route-a"));
    assert!(config["models"].get("hofund/route-b").is_some());
    assert!(config["models"].get("hofund/route-a").is_none());
    assert!(
        !agent
            .status(&session, &remaining)
            .expect("status")
            .connected()
    );
}

const GROK_CONFIG: &str = r#"[models]
default = "grok-4.7"

[model."my-local"]
model = "llama3"
base_url = "http://localhost:11434/v1"
api_key = "local"

[features]
campaigns = true
"#;

#[test]
fn shipped_grok_pick_turns_campaigns_off_and_clear_gives_them_back() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".grok").join("config.toml");
    fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    fs::write(&path, GROK_CONFIG).expect("seed");
    let target = target();
    let agent = adapter("grok", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    let read = || -> toml::Table {
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml")
    };
    let config = read();
    assert_eq!(config["models"]["default"].as_str(), Some("grok-4.7"));
    assert_eq!(config["features"]["campaigns"].as_bool(), Some(true));
    let route = &config["model"]["hofund/route-a"];
    assert_eq!(route["model"].as_str(), Some("route-a"));
    assert_eq!(route["base_url"].as_str(), Some("http://127.0.0.1:3425/v1"));
    assert_eq!(route["api_backend"].as_str(), Some("chat_completions"));
    assert_eq!(
        config["model"]["my-local"]["model"].as_str(),
        Some("llama3")
    );
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let config = read();
    assert_eq!(config["models"]["default"].as_str(), Some("hofund/route-a"));
    assert_eq!(config["features"]["campaigns"].as_bool(), Some(false));
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("hofund/route-a")
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    // Clearing drops the default (#433) and puts the user's campaigns back.
    agent.set_model(&session, &target, None).expect("clear");
    let config = read();
    assert!(
        config
            .get("models")
            .and_then(|m| m.get("default"))
            .is_none()
    );
    assert_eq!(config["features"]["campaigns"].as_bool(), Some(true));
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    // The clear dropped the emptied [models] table, so disconnect writes it
    // back at the end: the same settings, in another order.
    assert_eq!(
        read(),
        toml::from_str::<toml::Table>(GROK_CONFIG).expect("toml")
    );
    agent.apply(&session, &target).expect("apply again");
    agent.disconnect(&session, false).expect("disconnect again");
    assert_eq!(
        read(),
        toml::from_str::<toml::Table>(GROK_CONFIG).expect("toml")
    );
}

#[test]
fn shipped_grok_pick_in_a_fresh_home_leaves_nothing_after_disconnect() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".grok").join("config.toml");
    let target = target();
    let agent = adapter("grok", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let config: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
    assert_eq!(config["features"]["campaigns"].as_bool(), Some(false));
    agent.set_model(&session, &target, None).expect("clear");
    let config: toml::Table =
        toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
    assert!(!config.contains_key("features"));
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());
}

#[test]
fn shipped_hermes_connect_keeps_the_yaml_comment() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".hermes").join("config.yaml");
    let original = "# my hermes setup\nmodel:\n  provider: openrouter\nproviders:\n  local:\n    base_url: http://localhost:8080/v1\n";
    write(&path, original);
    let target = target();
    let agent = adapter("hermes", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    let connected = fs::read_to_string(&path).expect("connected");
    assert!(connected.starts_with("# my hermes setup\n"), "{connected}");
    assert!(connected.contains("  hofund:\n"), "{connected}");
    assert!(connected.contains("    base_url: \"http://127.0.0.1:3425/v1\"\n"));
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(&path).expect("restored"), original);
}

const GOOSE_CONFIG: &str = "# my goose settings
GOOSE_MODE: smart_approve
active_provider: anthropic
providers:
  anthropic:
    enabled: true
    model: claude-sonnet-4-5
    configured: true
";

fn goose_provider(root: &Path) -> serde_json::Value {
    let path = root
        .join("goose")
        .join("custom_providers")
        .join("hofund.json");
    serde_json::from_str(&fs::read_to_string(path).expect("read provider")).expect("json")
}

#[test]
fn shipped_goose_pick_makes_hofund_active_and_clear_drops_the_pick() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".config").join("goose").join("config.yaml");
    fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    fs::write(&path, GOOSE_CONFIG).expect("seed");
    let target = target();
    let agent = adapter("goose", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    // Connect writes only the provider file; the user's choice stays.
    assert_eq!(fs::read_to_string(&path).expect("read"), GOOSE_CONFIG);
    let provider = goose_provider(&home.join(".config"));
    assert_eq!(provider["base_url"], "http://127.0.0.1:3425/v1");
    assert_eq!(provider["headers"]["Authorization"], "Bearer local-token");
    assert_eq!(provider["models"][0]["name"], "route-a");
    assert_eq!(agent.configured_model(&session).expect("model"), None);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let picked = fs::read_to_string(&path).expect("read");
    assert!(picked.contains("active_provider: hofund\n"), "{picked}");
    assert_eq!(
        picked,
        "# my goose settings\nGOOSE_MODE: smart_approve\nactive_provider: hofund\nproviders:\n  anthropic:\n    enabled: true\n    model: claude-sonnet-4-5\n    configured: true\n  hofund:\n    enabled: true\n    configured: true\n    model: route-a\n"
    );
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("route-a")
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    // Clearing and disconnecting remove the pick and keep other providers.
    let cleared_config = "# my goose settings\nGOOSE_MODE: smart_approve\nproviders:\n  anthropic:\n    enabled: true\n    model: claude-sonnet-4-5\n    configured: true\n";
    agent.set_model(&session, &target, None).expect("clear");
    let cleared = fs::read_to_string(&path).expect("read");
    assert_eq!(cleared, cleared_config);
    assert_eq!(agent.configured_model(&session).expect("model"), None);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    let restored = fs::read_to_string(&path).expect("read");
    assert_eq!(restored, cleared_config);
    assert!(
        !home
            .join(".config/goose/custom_providers/hofund.json")
            .exists()
    );
}

#[test]
fn shipped_goose_pick_in_a_fresh_home_leaves_nothing_after_disconnect() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target();
    let agent = adapter("goose", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let config = home.join(".config").join("goose").join("config.yaml");
    let picked = fs::read_to_string(&config).expect("read");
    assert!(picked.contains("GOOSE_PROVIDER: hofund"), "{picked}");
    assert!(picked.contains("GOOSE_MODEL: route-a"), "{picked}");
    agent.set_model(&session, &target, None).expect("clear");
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!home.join(".config").exists());
}

#[test]
fn shipped_goose_follows_xdg_config_home_except_on_windows() {
    for (value, unix_root) in [
        (Some("absolute"), "xdg"),
        (Some(""), ".config"),
        (Some("relative/config"), ".config"),
        (None, ".config"),
    ] {
        let home = scratch();
        let session = match value {
            Some("absolute") => {
                Session::from_env_iter(&*home, [("XDG_CONFIG_HOME", home.join("xdg").as_os_str())])
            }
            Some(text) => {
                Session::from_env_iter(&*home, [("XDG_CONFIG_HOME", std::ffi::OsStr::new(text))])
            }
            None => Session::new(&*home),
        };
        for os in OPERATING_SYSTEMS {
            let root = if os == TargetOs::Windows {
                "AppData/Roaming/Block/goose/config".to_owned()
            } else {
                format!("{unix_root}/goose")
            };
            let plan = adapter("goose", os)
                .plan(&session, &target())
                .expect("plan");
            let paths = plan
                .changes()
                .iter()
                .map(|change| {
                    change
                        .path()
                        .strip_prefix(&*home)
                        .expect("relative")
                        .to_path_buf()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                [PathBuf::from(format!(
                    "{root}/custom_providers/hofund.json"
                ))],
                "{value:?} on {os:?}"
            );
        }
        // A pick writes config.yaml beside the provider file.
        let agent = adapter("goose", TargetOs::Windows);
        agent.apply(&session, &target()).expect("apply");
        agent
            .set_model(&session, &target(), Some("route-a"))
            .expect("pick");
        assert!(
            home.join("AppData/Roaming/Block/goose/config/config.yaml")
                .is_file()
        );
        assert!(!home.join("xdg").exists());
        agent.disconnect(&session, false).expect("disconnect");
        assert!(!home.join("AppData").exists());
    }
}

#[test]
fn shipped_goose_lets_goose_ask_the_gateway_only_when_no_route_is_listed() {
    let home = scratch();
    let session = Session::new(&*home);
    let root = home.join(".config");
    let agent = adapter("goose", TargetOs::Linux);
    let empty = target_with(&[]);
    agent.apply(&session, &empty).expect("apply without routes");
    let provider = goose_provider(&root);
    assert_eq!(provider["models"], serde_json::json!([]));
    assert!(provider.get("dynamic_models").is_none());
    assert!(agent.status(&session, &empty).expect("status").connected());
    let one = target();
    assert!(!agent.status(&session, &one).expect("status").connected());
    agent.apply(&session, &one).expect("apply with a route");
    let provider = goose_provider(&root);
    assert_eq!(provider["dynamic_models"], serde_json::json!(false));
    assert_eq!(provider["models"][0]["name"], "route-a");
    assert!(agent.status(&session, &one).expect("status").connected());
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!root.exists());
}

#[test]
fn b7_goose_emptied_route_list_drops_the_stale_dynamic_models_flag() {
    let home = scratch();
    let session = Session::new(&*home);
    let root = home.join(".config");
    let agent = adapter("goose", TargetOs::Linux);
    let one = target();
    agent.apply(&session, &one).expect("apply with a route");
    assert_eq!(
        goose_provider(&root)["dynamic_models"],
        serde_json::json!(false)
    );
    let empty = target_with(&[]);
    assert!(!agent.status(&session, &empty).expect("status").connected());
    agent
        .apply(&session, &empty)
        .expect("apply after the routes went away");
    let provider = goose_provider(&root);
    assert!(provider.get("dynamic_models").is_none(), "{provider}");
    assert!(agent.status(&session, &empty).expect("status").connected());
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!root.exists());
}

const ZED_SETTINGS: &str = r#"// Zed settings
{
  "theme": "One Dark", // keep this note
  "agent": {
    "default_model": { "provider": "anthropic", "model": "claude-sonnet-4" }
  }
}
"#;

fn zed_store_session(
    home: &Path,
) -> (
    Session,
    std::sync::Arc<hofund_kit_agent::MemoryCredentialStore>,
) {
    let store = std::sync::Arc::new(hofund_kit_agent::MemoryCredentialStore::new());
    let shared: std::sync::Arc<dyn hofund_plugin_api::CredentialStore> = store.clone();
    (Session::new(home).with_credential_store(shared), store)
}

#[test]
fn shipped_zed_keeps_its_key_in_the_store_and_gives_the_user_model_back() {
    let home = scratch();
    let (session, store) = zed_store_session(&home);
    let path = home.join(".config").join("zed").join("settings.json");
    fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    fs::write(&path, ZED_SETTINGS).expect("seed");
    let target = target();
    let agent = adapter("zed", TargetOs::Linux);
    let plan = agent.plan(&session, &target).expect("plan");
    assert!(
        plan.changes()
            .iter()
            .any(|change| change.path() == Path::new("credential store: http://127.0.0.1:3425/v1")),
        "{plan:?}"
    );
    agent.apply(&session, &target).expect("apply");
    let applied = fs::read_to_string(&path).expect("read");
    assert!(!applied.contains("local-token"), "{applied}");
    assert!(applied.starts_with("// Zed settings\n"), "{applied}");
    assert!(applied.contains("// keep this note"), "{applied}");
    let items = store.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url(), "http://127.0.0.1:3425/v1");
    assert_eq!(items[0].account(), "Bearer");
    assert_eq!(items[0].label(), "zed-github-account");
    assert_eq!(
        items[0].windows_target(),
        "zed:url=http://127.0.0.1:3425/v1"
    );
    assert_eq!(
        store
            .secret("http://127.0.0.1:3425/v1", "Bearer")
            .as_deref(),
        Some("local-token")
    );
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("claude-sonnet-4")
    );
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    let picked = hofund_kit_agent::parse_jsonc(&fs::read_to_string(&path).expect("read"));
    assert_eq!(
        picked["agent"]["default_model"],
        serde_json::json!({ "provider": "hofund", "model": "route-a" })
    );
    agent.set_model(&session, &target, None).expect("clear");
    // Zed reads default_model only whole, so clearing puts back the user's
    // object rather than leaving a provider with no model.
    let cleared = hofund_kit_agent::parse_jsonc(&fs::read_to_string(&path).expect("read"));
    assert_eq!(
        cleared["agent"]["default_model"],
        serde_json::json!({ "provider": "anthropic", "model": "claude-sonnet-4" })
    );
    // Replacing the store item by hand reads as not connected.
    store.set(&items[0], "someone-else");
    assert!(!agent.status(&session, &target).expect("status").connected());
    store.set(&items[0], "local-token");
    assert!(agent.status(&session, &target).expect("status").connected());
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(&path).expect("read"), ZED_SETTINGS);
    assert!(store.items().is_empty());
}

#[test]
fn shipped_zed_without_a_store_refuses_to_connect_and_writes_nothing() {
    let home = scratch();
    let session = Session::new(&*home);
    let agent = adapter("zed", TargetOs::Linux);
    let error = agent.apply(&session, &target()).expect_err("no store");
    assert!(error.to_string().contains("OS credential store"), "{error}");
    assert!(!home.join(".config").exists());
}

#[test]
fn shipped_zed_settings_live_under_appdata_on_windows() {
    let home = scratch();
    let (session, store) = zed_store_session(&home);
    let agent = adapter("zed", TargetOs::Windows);
    agent.apply(&session, &target()).expect("apply");
    assert!(
        home.join("AppData/Roaming/Zed/settings.json").is_file(),
        "settings not written under AppData"
    );
    assert_eq!(
        store.items()[0].windows_target(),
        "zed:url=http://127.0.0.1:3425/v1"
    );
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!home.join("AppData").exists());
}

#[test]
fn shipped_zed_clear_in_a_fresh_home_drops_the_whole_default_model() {
    let home = scratch();
    let (session, store) = zed_store_session(&home);
    let path = home.join(".config").join("zed").join("settings.json");
    let target = target();
    let agent = adapter("zed", TargetOs::Linux);
    agent.apply(&session, &target).expect("apply");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    assert_eq!(
        agent.configured_model(&session).expect("model").as_deref(),
        Some("route-a")
    );
    agent.set_model(&session, &target, None).expect("clear");
    let cleared: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("json");
    assert_eq!(cleared.get("agent"), None, "{cleared}");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!home.join(".config").exists());
    assert!(store.items().is_empty());
}

fn picked_goose(initial: &str) -> (Scratch, Session, Compiled, PathBuf) {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".config/goose/config.yaml");
    write(&path, initial);
    let agent = adapter("goose", TargetOs::Linux);
    let target = target();
    agent.apply(&session, &target).expect("apply");
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick");
    (home, session, agent, path)
}

const B7_GOOSE_OLD: &str =
    "# legacy\nGOOSE_PROVIDER: anthropic\nGOOSE_MODEL: claude\nGOOSE_MODE: auto\n";
const B7_GOOSE_NEW: &str = "# current\nGOOSE_PROVIDER: openai\nGOOSE_MODEL: gpt\nactive_provider: anthropic\nproviders:\n  anthropic:\n    enabled: true\n    configured: true\n    model: claude\n";

#[test]
fn b7_goose_old_layout_pick_writes_flat_keys() {
    let (_home, _session, _agent, path) = picked_goose(B7_GOOSE_OLD);
    assert_eq!(
        fs::read_to_string(path).expect("picked"),
        "# legacy\nGOOSE_PROVIDER: hofund\nGOOSE_MODEL: route-a\nGOOSE_MODE: auto\n"
    );
}

#[test]
fn b7_goose_new_layout_pick_writes_map_and_removes_flat_keys() {
    let (_home, _session, _agent, path) = picked_goose(B7_GOOSE_NEW);
    assert_eq!(
        fs::read_to_string(path).expect("picked"),
        "# current\nactive_provider: hofund\nproviders:\n  anthropic:\n    enabled: true\n    configured: true\n    model: claude\n  hofund:\n    enabled: true\n    configured: true\n    model: route-a\n"
    );
}

#[test]
fn b7_goose_providers_map_alone_is_the_current_layout() {
    let (_home, _session, _agent, path) =
        picked_goose("providers:\n  anthropic:\n    model: claude\n");
    let picked = fs::read_to_string(path).expect("picked");
    assert!(picked.contains("active_provider: hofund\n"), "{picked}");
    assert!(picked.contains("  hofund:\n"), "{picked}");
    assert!(!picked.contains("GOOSE_"), "{picked}");
}

#[test]
fn b7_goose_active_provider_alone_is_the_current_layout() {
    let (_home, _session, _agent, path) = picked_goose("active_provider: anthropic\n");
    let picked = fs::read_to_string(path).expect("picked");
    assert!(picked.starts_with("active_provider: hofund\n"), "{picked}");
    assert!(picked.contains("providers:\n  hofund:\n"), "{picked}");
    assert!(!picked.contains("GOOSE_"), "{picked}");
}

#[test]
fn b7_goose_old_layout_clear_and_disconnect_remove_flat_keys() {
    let (_home, session, agent, path) = picked_goose(B7_GOOSE_OLD);
    let target = target();
    agent.set_model(&session, &target, None).expect("clear");
    let cleared = "# legacy\nGOOSE_MODE: auto\n";
    assert_eq!(fs::read_to_string(&path).expect("cleared"), cleared);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(path).expect("disconnected"), cleared);
}

#[test]
fn b7_goose_new_layout_clear_and_disconnect_remove_active_and_hofund() {
    let (_home, session, agent, path) = picked_goose(B7_GOOSE_NEW);
    let target = target();
    agent.set_model(&session, &target, None).expect("clear");
    let cleared = "# current\nproviders:\n  anthropic:\n    enabled: true\n    configured: true\n    model: claude\n";
    assert_eq!(fs::read_to_string(&path).expect("cleared"), cleared);
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(fs::read_to_string(path).expect("disconnected"), cleared);
}

#[test]
fn b7_goose_other_provider_entries_survive_clear_and_disconnect() {
    let initial = "active_provider: hofund\nproviders:\n  hofund:\n    enabled: true\n    configured: true\n    model: route-a\n  anthropic:\n    enabled: true\n    model: claude\n  openai:\n    enabled: false\n    model: gpt\n";
    let (home, session, agent, path) = picked_goose(initial);
    let target = target();
    for disconnect in [false, true] {
        if disconnect {
            agent
                .set_model(&session, &target, Some("route-a"))
                .expect("pick again");
            agent.disconnect(&session, false).expect("disconnect");
        } else {
            agent.set_model(&session, &target, None).expect("clear");
        }
        let text = fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("  anthropic:\n    enabled: true\n    model: claude\n"),
            "{text}"
        );
        assert!(
            text.contains("  openai:\n    enabled: false\n    model: gpt\n"),
            "{text}"
        );
        assert!(!text.contains("  hofund:\n"), "{text}");
    }
    agent
        .disconnect(&session, false)
        .expect("second disconnect");
    assert!(
        !home
            .join(".config/goose/custom_providers/hofund.json")
            .exists()
    );
}

#[test]
fn b7_goose_configured_model_follows_active_provider() {
    for (initial, expected) in [
        (
            "active_provider: hofund\nproviders:\n  hofund:\n    model: route-a\n",
            Some("route-a"),
        ),
        (
            "active_provider: anthropic\nproviders:\n  hofund:\n    model: route-a\n",
            None,
        ),
        (
            "GOOSE_PROVIDER: hofund\nGOOSE_MODEL: flat-route\n",
            Some("flat-route"),
        ),
        (
            "active_provider: hofund\nGOOSE_MODEL: flat-route\nproviders:\n  hofund:\n    enabled: true\n",
            Some("flat-route"),
        ),
        (
            "active_provider: anthropic\nGOOSE_PROVIDER: hofund\nproviders:\n  hofund:\n    model: route-a\n",
            None,
        ),
        (
            "active_provider: \"\"\nGOOSE_PROVIDER: hofund\nGOOSE_MODEL: flat-route\n",
            Some("flat-route"),
        ),
    ] {
        let home = scratch();
        let session = Session::new(&*home);
        write(&home.join(".config/goose/config.yaml"), initial);
        assert_eq!(
            adapter("goose", TargetOs::Linux)
                .configured_model(&session)
                .expect("configured model")
                .as_deref(),
            expected,
            "{initial}"
        );
    }
}

fn b7_go_binary(path: &Path) {
    let mut bytes = b"\xff Go buildinf:".to_vec();
    bytes.extend_from_slice(&[8, 2, 8, b'g', b'o', b'1', b'.', b'9', b'9', b'.', b'1', 0]);
    write_bytes(path, &bytes);
}

#[test]
fn b7_goose_pressly_binary_is_not_detected() {
    let home = scratch();
    let name = if cfg!(windows) { "goose.exe" } else { "goose" };
    b7_go_binary(&home.join("bin").join(name));
    let session = Session::new(&*home).with_path(home.join("bin"));
    assert!(!adapter("goose", TargetOs::Linux).detect(&session));
}

#[test]
fn b7_goose_real_binary_is_detected() {
    let home = scratch();
    let name = if cfg!(windows) { "goose.exe" } else { "goose" };
    write(&home.join("bin").join(name), "not a Go program\n");
    let session = Session::new(&*home).with_path(home.join("bin"));
    assert!(adapter("goose", TargetOs::Linux).detect(&session));
}

#[test]
fn b7_goose_yaml_comments_survive_pick_clear_and_disconnect() {
    let initial = "# top\nGOOSE_MODE: auto # inline\nactive_provider: anthropic # active\nproviders:\n  # mine\n  anthropic:\n    model: claude # model\n";
    let (_home, session, agent, path) = picked_goose(initial);
    let target = target();
    let picked = fs::read_to_string(&path).expect("picked");
    for comment in ["# top", "# inline", "# active", "# mine", "# model"] {
        assert!(picked.contains(comment), "{comment}: {picked}");
    }
    agent.set_model(&session, &target, None).expect("clear");
    let cleared = fs::read_to_string(&path).expect("cleared");
    for comment in ["# top", "# inline", "# mine", "# model"] {
        assert!(cleared.contains(comment), "{comment}: {cleared}");
    }
    agent
        .set_model(&session, &target, Some("route-a"))
        .expect("pick again");
    let picked_again = fs::read_to_string(&path).expect("picked again");
    for comment in ["# top", "# inline", "# mine", "# model"] {
        assert!(picked_again.contains(comment), "{comment}: {picked_again}");
    }
    agent.disconnect(&session, false).expect("disconnect");
    let disconnected = fs::read_to_string(path).expect("disconnected");
    for comment in ["# top", "# inline", "# mine", "# model"] {
        assert!(disconnected.contains(comment), "{comment}: {disconnected}");
    }
}

#[test]
fn g2_15_shipped_agents_declare_request_hints() {
    let claude = hofund_plugin_registry::agent("claude-code").expect("claude-code");
    assert_eq!(
        claude.request_agent_header(),
        Some("x-claude-code-agent-id")
    );
    assert!(claude.client_prefixes().is_empty());
    assert!(claude.nested_turn_headers().is_empty());

    let codex = hofund_plugin_registry::agent("codex").expect("codex");
    assert!(codex.request_agent_header().is_none());
    assert_eq!(codex.client_prefixes(), ["codex", "acp-extension-codex"]);
    assert_eq!(
        codex.nested_turn_headers(),
        ["x-codex-window-id", "x-codex-turn-metadata"]
    );

    for agent in hofund_plugin_registry::agents() {
        let id = agent.id().as_str();
        if id == "claude-code" || id == "codex" {
            continue;
        }
        assert!(
            agent.request_agent_header().is_none(),
            "{id} request header"
        );
        assert!(agent.client_prefixes().is_empty(), "{id} prefixes");
        assert!(agent.nested_turn_headers().is_empty(), "{id} nested");
    }
}
