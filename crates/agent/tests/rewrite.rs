//! Fixture tests for the four adapters. Each one runs in a temp directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{DisconnectOutcome, GatewayTarget, Session, find, legacy};
use sha2::{Digest, Sha256};

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hofund-agent-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).expect("dir");
    path
}

fn target() -> GatewayTarget {
    GatewayTarget::new("http://127.0.0.1:3425/", "local-token", Vec::new())
}

fn target_with(models: &[&str]) -> GatewayTarget {
    target().with_models(models.iter().map(|model| (*model).to_owned()).collect())
}

fn adapter(id: &str) -> &'static dyn hofund_agent::AgentAdapter {
    match id {
        "claude-code" => legacy::claude_code(),
        "codex" => legacy::codex(),
        "gemini-cli" => legacy::gemini_cli(),
        _ => find(id).expect(id),
    }
}

fn write(path: &Path, bytes: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, bytes).expect("write");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read")
}

struct Case {
    id: &'static str,
    relative: &'static [&'static str],
    original: &'static str,
    marker: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            id: "claude-code",
            relative: &[".claude", "settings.json"],
            original: "{\n  \"ui\": { \"theme\": \"dark\" },\n  \"extra\": 1\n}\n",
            marker: "\"theme\": \"dark\"",
        },
        Case {
            id: "codex",
            relative: &[".codex", "config.toml"],
            original: "# keep me\nmodel = \"gpt-5\"\ntemperature = 0.2\n\n[model_providers.openai]\nname = \"OpenAI\"\n",
            marker: "# keep me",
        },
        Case {
            id: "gemini-cli",
            relative: &[".gemini", "settings.json"],
            original: "{\n  \"ui\": { \"theme\": \"dark\" },\n  \"security\": { \"auth\": { \"selectedType\": \"oauth-personal\", \"extra\": true } }\n}\n",
            marker: "\"theme\": \"dark\"",
        },
        Case {
            id: "opencode",
            relative: &[".config", "opencode", "opencode.jsonc"],
            original: "{\n  // keep\n  \"plugin\": [\"a\"],\n  \"providers\": { \"other\": { \"name\": \"Other\" } },\n}\n",
            marker: "\"plugin\"",
        },
    ]
}

fn joined(home: &Path, parts: &[&str]) -> PathBuf {
    let mut path = home.to_path_buf();
    for part in parts {
        path = path.join(part);
    }
    path
}

#[test]
fn apply_is_idempotent_and_revert_restores_the_original_bytes() {
    let gateway = target();
    for case in cases() {
        let home = scratch();
        let path = joined(&home, case.relative);
        write(&path, case.original);
        let untouched = home.join("notes.txt");
        write(&untouched, "leave me\n");
        let session = Session::new(&home);
        let adapter = adapter(case.id);

        let preview = adapter.plan(&session, &gateway).expect("plan");
        assert!(!preview.changes().is_empty(), "{}", case.id);
        assert!(!path.with_file_name(backup_name(&path)).exists());
        assert_eq!(read(&path), case.original, "plan writes nothing");

        adapter.apply(&session, &gateway).expect("apply");
        let applied = fs::read(&path).expect("applied");
        assert_ne!(applied, case.original.as_bytes());
        assert!(read(&path).contains(case.marker), "{}", case.id);
        assert!(
            adapter
                .status(&session, &gateway)
                .expect("status")
                .connected()
        );
        assert_eq!(read(&untouched), "leave me\n");

        let backup = sibling(&path, ".hofund.bak");
        assert_eq!(fs::read(&backup).expect("bak"), case.original.as_bytes());

        adapter.apply(&session, &gateway).expect("reapply");
        assert_eq!(fs::read(&path).expect("reapplied"), applied);
        assert_eq!(
            fs::read(&backup).expect("bak stays"),
            case.original.as_bytes()
        );

        adapter.revert(&session).expect("revert");
        assert_eq!(fs::read(&path).expect("restored"), case.original.as_bytes());
        assert!(!backup.exists());
        assert!(
            !adapter
                .status(&session, &gateway)
                .expect("status")
                .connected()
        );
    }
}

#[test]
fn gemini_env_keeps_unrelated_lines_and_a_second_apply_is_a_no_op() {
    let home = scratch();
    let env = home.join(".gemini").join(".env");
    let original = "# note\r\nexport KEEP=1\r\nGEMINI_API_KEY=old\r\n";
    write(&env, original);
    let session = Session::new(&home);
    let adapter = legacy::gemini_cli();
    adapter.apply(&session, &target()).expect("apply");
    let text = read(&env);
    assert!(text.contains("# note"));
    assert!(text.contains("export KEEP=1"));
    assert!(text.contains("GEMINI_API_KEY=local-token"));
    assert!(text.contains("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3425"));
    assert!(text.contains("GEMINI_API_KEY_AUTH_MECHANISM=bearer"));
    let bytes = fs::read(&env).expect("bytes");
    adapter.apply(&session, &target()).expect("reapply");
    assert_eq!(fs::read(&env).expect("again"), bytes);
    adapter.revert(&session).expect("revert");
    assert_eq!(read(&env), original);
}

#[test]
fn gemini_cli_home_controls_both_files_and_unusable_values_use_home() {
    let adapter = legacy::gemini_cli();
    let home = scratch();
    let override_home = scratch();
    let session = Session::new(&home).with_gemini_cli_home(&override_home);
    let settings = override_home.join(".gemini").join("settings.json");
    let env = override_home.join(".gemini").join(".env");
    let plan = adapter.plan(&session, &target()).expect("plan");
    let mut planned = plan
        .changes()
        .iter()
        .map(|change| change.path().to_path_buf())
        .collect::<Vec<_>>();
    planned.sort();
    let mut expected = vec![settings.clone(), env.clone()];
    expected.sort();
    assert_eq!(planned, expected);
    adapter.apply(&session, &target()).expect("apply");
    assert!(settings.is_file());
    assert!(env.is_file());
    assert!(!home.join(".gemini").exists());

    for value in ["", "relative"] {
        let home = scratch();
        let root = home.join(".gemini");
        let session = Session::new(&home).with_gemini_cli_home(value);
        let plan = adapter.plan(&session, &target()).expect("default plan");
        assert!(
            plan.changes()
                .iter()
                .all(|change| change.path().starts_with(&root))
        );
        adapter.apply(&session, &target()).expect("default apply");
        assert!(root.join("settings.json").is_file());
        assert!(root.join(".env").is_file());
    }
}

#[test]
fn creating_a_missing_file_reverts_by_deleting_it() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = legacy::claude_code();
    assert!(!adapter.detect(&session));
    adapter.apply(&session, &target()).expect("apply");
    let path = home.join(".claude").join("settings.json");
    assert!(path.is_file());
    assert!(sibling(&path, ".hofund.absent").is_file());
    assert!(!sibling(&path, ".hofund.bak").exists());
    let bytes = fs::read(&path).expect("bytes");
    adapter.apply(&session, &target()).expect("reapply");
    assert_eq!(fs::read(&path).expect("again"), bytes);
    adapter.revert(&session).expect("revert");
    assert!(!path.exists());
    assert!(!sibling(&path, ".hofund.absent").exists());
}

#[test]
fn opencode_without_jsonc_creates_json() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = find("opencode").expect("opencode");
    adapter.apply(&session, &target()).expect("apply");
    let json = home.join(".config").join("opencode").join("opencode.json");
    let jsonc = home.join(".config").join("opencode").join("opencode.jsonc");
    assert!(json.is_file());
    assert!(!jsonc.exists());
    let text = read(&json);
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    let provider = &value["provider"]["hofund"];
    assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:3425/v1");
    assert_eq!(provider["options"]["apiKey"], "local-token");
    assert!(provider["models"].is_object());
    assert!(value.get("providers").is_none());
}

#[test]
fn invalid_toml_does_not_leave_a_backup() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model_provider = [\n");
    let session = Session::new(&home);
    let adapter = legacy::codex();
    assert!(adapter.plan(&session, &target()).is_err());
    assert!(!sibling(&path, ".hofund.bak").exists());
    assert_eq!(read(&path), "model_provider = [\n");
}

#[test]
fn overrides_and_path_detection_do_not_use_the_real_home() {
    let home = scratch();
    let elsewhere = scratch();
    let session = Session::new(&home).with_claude_config_dir(elsewhere.join("claude-dir"));
    let adapter = legacy::claude_code();
    adapter.apply(&session, &target()).expect("apply");
    let path = elsewhere.join("claude-dir").join("settings.json");
    assert!(path.is_file());
    assert!(!home.join(".claude").exists());

    let bin = scratch();
    let file_name = if cfg!(windows) { "codex.exe" } else { "codex" };
    write(&bin.join(file_name), "");
    let session = Session::new(&home).with_path(&bin);
    assert!(legacy::codex().detect(&session));
    assert!(!legacy::gemini_cli().detect(&session));
}

#[test]
fn codex_points_at_v1_responses_and_keeps_the_user_model() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    legacy::codex().apply(&session, &target()).expect("apply");
    let text = read(&path);
    assert!(text.contains("model = \"gpt-5\""));
    assert!(text.contains("model_provider = \"hofund\""));
    assert!(text.contains("base_url = \"http://127.0.0.1:3425/v1\""));
    assert!(text.contains("wire_api = \"responses\""));
    assert!(text.contains("experimental_bearer_token = \"local-token\""));
    assert!(!text.contains("requires_openai_auth"));
}

#[test]
fn disconnect_keeps_a_key_added_while_connected() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = legacy::codex();
    adapter.apply(&session, &target()).expect("apply");
    let text = format!("sandbox_mode = \"workspace-write\"\n{}", read(&path));
    fs::write(&path, text).expect("edit");
    assert!(disconnect(adapter, &session, false).changed());
    let after = read(&path);
    assert!(
        after.contains("sandbox_mode = \"workspace-write\""),
        "{after}"
    );
    assert!(after.contains("model = \"gpt-5\""), "{after}");
    assert!(!after.contains("experimental_bearer_token"), "{after}");
    assert!(!after.contains("model_provider"), "{after}");
    assert!(!after.contains("hofund"), "{after}");
    assert!(!sibling(&path, ".hofund.bak").exists());
    assert!(!disconnect(adapter, &session, false).changed());
}

#[test]
fn disconnect_refuses_a_broken_file_until_force() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = legacy::codex();
    adapter.apply(&session, &target()).expect("apply");
    fs::write(&path, "model_provider = [\n").expect("break");
    let error = adapter
        .disconnect(&session, false)
        .expect_err("refuse")
        .to_string();
    assert!(error.contains("--force"), "{error}");
    assert!(error.contains("codex"), "{error}");
    assert_eq!(read(&path), "model_provider = [\n");
    assert!(sibling(&path, ".hofund.bak").is_file());
    assert!(disconnect(adapter, &session, true).changed());
    let restored = read(&path);
    assert!(restored.contains("model = \"gpt-5\""));
    assert!(!restored.contains("model_provider"));
    assert!(!sibling(&path, ".hofund.bak").exists());
}

#[test]
fn disconnect_removes_an_empty_directory_it_created() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = legacy::claude_code();
    adapter.apply(&session, &target()).expect("apply");
    let path = home.join(".claude").join("settings.json");
    assert!(path.is_file());
    assert!(disconnect(adapter, &session, false).changed());
    assert!(!path.exists());
    assert!(!home.join(".claude").exists());
    assert!(!disconnect(adapter, &session, false).changed());
}

#[test]
fn disconnect_keeps_a_directory_that_still_holds_a_file() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = legacy::codex();
    adapter.apply(&session, &target()).expect("apply");
    let path = home.join(".codex").join("config.toml");
    let extra = home.join(".codex").join("notes.txt");
    write(&extra, "stay\n");
    assert!(disconnect(adapter, &session, false).changed());
    assert!(!path.exists());
    assert!(extra.is_file());
    assert!(home.join(".codex").is_dir());
}

#[test]
fn opencode_honors_xdg_config_home() {
    let home = scratch();
    let xdg = scratch();
    let session = Session::new(&home).with_xdg_config_home(&xdg);
    let adapter = find("opencode").expect("opencode");
    adapter.apply(&session, &target()).expect("apply");
    let json = xdg.join("opencode").join("opencode.json");
    assert!(json.is_file());
    assert!(!home.join(".config").exists());
    assert!(disconnect(adapter, &session, false).changed());
    assert!(!json.exists());
    assert!(!xdg.join("opencode").exists());
    assert!(xdg.is_dir());
}

#[test]
fn refresh_token_rewrites_the_token_and_keeps_the_backup() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = legacy::codex();
    adapter.apply(&session, &target()).expect("apply");
    let backup = fs::read(sibling(&path, ".hofund.bak")).expect("bak");
    let rotated = target().with_token("rotated-token");
    let updated = adapter.refresh_token(&session, &rotated).expect("refresh");
    assert_eq!(updated, vec![path.clone()]);
    let text = read(&path);
    assert!(text.contains("rotated-token"));
    assert!(!text.contains("local-token"));
    assert!(text.contains("model = \"gpt-5\""));
    assert_eq!(
        fs::read(sibling(&path, ".hofund.bak")).expect("bak stays"),
        backup
    );
    assert!(disconnect(adapter, &session, false).changed());
    assert_eq!(read(&path), "model = \"gpt-5\"\n");
    assert!(!sibling(&path, ".hofund.bak").exists());
}

#[test]
fn clean_disconnect_restores_the_sha256_of_every_managed_file() {
    let files = [
        (
            "claude-code",
            &[".claude", "settings.json"][..],
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"sk-existing","ANTHROPIC_BASE_URL":"https://my-proxy.example"}}"#,
        ),
        (
            "codex",
            &[".codex", "config.toml"],
            "model_provider = \"azure\"\nmodel = \"gpt-5\"\n",
        ),
        (
            "gemini-cli",
            &[".gemini", "settings.json"],
            r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        ),
        (
            "gemini-cli",
            &[".gemini", ".env"],
            "GEMINI_API_KEY=existing-key\nKEEP=1\n",
        ),
        (
            "opencode",
            &[".config", "opencode", "opencode.json"],
            r#"{"plugin":["a"]}"#,
        ),
    ];
    for (id, relative, original) in files {
        let home = scratch();
        let path = joined(&home, relative);
        write(&path, original);
        let before = sha256(original.as_bytes());
        let session = Session::new(&home);
        let adapter = adapter(id);
        adapter.apply(&session, &target()).expect("apply");
        assert_ne!(sha256(&fs::read(&path).expect("connected")), before);
        let outcome = disconnect(adapter, &session, false);
        assert!(outcome.changed(), "{id}");
        assert!(
            outcome.warnings().is_empty(),
            "{id}: {:?}",
            outcome.warnings()
        );
        let restored = fs::read(&path).unwrap_or_else(|_| panic!("{id} missing"));
        assert_eq!(
            sha256(&restored),
            before,
            "{id}\n{}",
            String::from_utf8_lossy(&restored)
        );
        assert!(!sibling(&path, ".hofund.bak").exists(), "{id}");
        assert!(!sibling(&path, ".hofund.wrote").exists(), "{id}");
    }
}

#[test]
fn disconnect_keeps_a_value_the_user_changed() {
    let home = scratch();
    let path = home.join(".claude").join("settings.json");
    write(
        &path,
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://my-proxy.example","ANTHROPIC_AUTH_TOKEN":"sk-existing"}}"#,
    );
    let session = Session::new(&home);
    let adapter = legacy::claude_code();
    adapter.apply(&session, &target()).expect("apply");
    let edited = read(&path).replace("local-token", "user-edited-token");
    fs::write(&path, edited).expect("edit");
    let outcome = disconnect(adapter, &session, false);
    assert!(outcome.changed());
    assert!(
        outcome
            .warnings()
            .iter()
            .any(|warning| warning.contains("ANTHROPIC_AUTH_TOKEN")),
        "{:?}",
        outcome.warnings()
    );
    let after = read(&path);
    assert!(after.contains("user-edited-token"), "{after}");
    assert!(after.contains("https://my-proxy.example"), "{after}");
    assert!(!after.contains("127.0.0.1"), "{after}");
    assert!(!sibling(&path, ".hofund.bak").exists());
    assert!(!disconnect(adapter, &session, true).changed());
}

#[test]
fn gemini_writes_model_name_from_the_only_route() {
    let home = scratch();
    let session = Session::new(&home);
    legacy::gemini_cli()
        .apply(&session, &target_with(&["gemini-2.5-pro"]))
        .expect("apply");
    let settings = home.join(".gemini").join("settings.json");
    let value: serde_json::Value = serde_json::from_str(&read(&settings)).expect("json");
    assert_eq!(value["model"]["name"], "gemini-2.5-pro");
    assert_eq!(value["model"].as_object().expect("object").len(), 1);
    assert_eq!(
        legacy::gemini_cli()
            .configured_model(&session)
            .expect("read")
            .as_deref(),
        Some("gemini-2.5-pro")
    );
}

#[test]
fn gemini_moves_a_string_model_and_keeps_an_existing_name() {
    let home = scratch();
    let settings = home.join(".gemini").join("settings.json");
    write(&settings, "{\n  \"model\": \"gemini-2.5-flash\"\n}\n");
    let session = Session::new(&home);
    let adapter = legacy::gemini_cli();
    adapter
        .apply(&session, &target_with(&["other-route"]))
        .expect("apply");
    let value: serde_json::Value = serde_json::from_str(&read(&settings)).expect("json");
    assert_eq!(value["model"]["name"], "gemini-2.5-flash");
    assert!(value["model"].as_str().is_none());

    let home = scratch();
    let settings = home.join(".gemini").join("settings.json");
    write(
        &settings,
        "{\n  \"model\": { \"name\": \"kept\", \"extra\": true }\n}\n",
    );
    let session = Session::new(&home);
    legacy::gemini_cli()
        .apply(&session, &target_with(&["gemini-2.5-pro"]))
        .expect("apply");
    let value: serde_json::Value = serde_json::from_str(&read(&settings)).expect("json");
    assert_eq!(value["model"]["name"], "kept");
    assert_eq!(value["model"]["extra"], true);
}

#[test]
fn gemini_with_several_routes_does_not_pick_a_model() {
    let home = scratch();
    let session = Session::new(&home);
    legacy::gemini_cli()
        .apply(&session, &target_with(&["one", "two"]))
        .expect("apply");
    let settings = home.join(".gemini").join("settings.json");
    let value: serde_json::Value = serde_json::from_str(&read(&settings)).expect("json");
    assert!(value.get("model").is_none());
}

#[test]
fn opencode_lists_each_route_without_invented_catalog_fields() {
    let home = scratch();
    let session = Session::new(&home);
    find("opencode")
        .expect("opencode")
        .apply(&session, &target_with(&["alpha", "beta", "alpha"]))
        .expect("apply");
    let json = home.join(".config").join("opencode").join("opencode.json");
    let value: serde_json::Value = serde_json::from_str(&read(&json)).expect("json");
    let models = value["provider"]["hofund"]["models"]
        .as_object()
        .expect("models");
    assert_eq!(models.len(), 2);
    for id in ["alpha", "beta"] {
        assert_eq!(models[id]["name"], id);
        assert!(
            models[id]["variants"]
                .as_object()
                .expect("variants")
                .is_empty()
        );
        assert!(models[id].get("modalities").is_none());
        assert!(models[id].get("limit").is_none());
        assert!(models[id].get("reasoning").is_none());
    }
}

#[test]
fn claude_writes_tier_models_and_leaves_subagent_and_effort_unset() {
    let home = scratch();
    let session = Session::new(&home);
    legacy::claude_code()
        .apply(&session, &target_with(&["route-a"]))
        .expect("apply");
    let path = home.join(".claude").join("settings.json");
    let value: serde_json::Value = serde_json::from_str(&read(&path)).expect("json");
    assert_eq!(value["model"], "route-a");
    let env = &value["env"];
    for key in [
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ] {
        assert_eq!(env[key], "route-a", "{key}");
    }
    assert!(env.get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
    assert!(env.get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
    assert!(env.get("CLAUDE_CODE_EFFORT_LEVEL").is_none());
    assert!(value.get("effortLevel").is_none());

    let home = scratch();
    let path = home.join(".claude").join("settings.json");
    write(&path, "{\n  \"model\": \"claude-fable-5\"\n}\n");
    let session = Session::new(&home);
    legacy::claude_code()
        .apply(&session, &target_with(&["route-a", "route-b"]))
        .expect("apply");
    let value: serde_json::Value = serde_json::from_str(&read(&path)).expect("json");
    assert_eq!(value["model"], "claude-fable-5");
    assert_eq!(
        value["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
        "claude-fable-5"
    );
    assert_eq!(
        value["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        "claude-fable-5"
    );
    assert!(value["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
}

#[test]
fn configured_model_reads_the_tool_model() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = legacy::codex();
    assert_eq!(
        adapter
            .configured_model(&session)
            .expect("model")
            .as_deref(),
        Some("gpt-5")
    );
    adapter.apply(&session, &target()).expect("apply");
    assert_eq!(
        adapter
            .configured_model(&session)
            .expect("model")
            .as_deref(),
        Some("gpt-5")
    );
}

fn disconnect(
    adapter: &dyn hofund_agent::AgentAdapter,
    session: &Session,
    force: bool,
) -> DisconnectOutcome {
    adapter.disconnect(session, force).expect("disconnect")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().expect("name").to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn backup_name(path: &Path) -> std::ffi::OsString {
    let mut name = path.file_name().expect("name").to_os_string();
    name.push(".hofund.bak");
    name
}
