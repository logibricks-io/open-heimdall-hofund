//! Fixture tests for the four adapters. Each one runs in a temp directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{DisconnectOutcome, GatewayTarget, Session, find};
use sha2::{Digest, Sha256};

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hofund-agent-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).expect("dir");
    path
}

fn target() -> GatewayTarget {
    GatewayTarget {
        base_url: "http://127.0.0.1:3425/".to_owned(),
        token: "local-token".to_owned(),
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
        let adapter = find(case.id).expect(case.id);

        let preview = adapter.plan(&session, &gateway).expect("plan");
        assert!(!preview.changes.is_empty(), "{}", case.id);
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
                .connected
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
                .connected
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
    let adapter = find("gemini-cli").expect("gemini");
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
fn creating_a_missing_file_reverts_by_deleting_it() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = find("claude-code").expect("claude");
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
    let adapter = find("codex").expect("codex");
    assert!(adapter.plan(&session, &target()).is_err());
    assert!(!sibling(&path, ".hofund.bak").exists());
    assert_eq!(read(&path), "model_provider = [\n");
}

#[test]
fn overrides_and_path_detection_do_not_use_the_real_home() {
    let home = scratch();
    let elsewhere = scratch();
    let session = Session::new(&home).with_claude_config_dir(elsewhere.join("claude-dir"));
    let adapter = find("claude-code").expect("claude");
    adapter.apply(&session, &target()).expect("apply");
    let path = elsewhere.join("claude-dir").join("settings.json");
    assert!(path.is_file());
    assert!(!home.join(".claude").exists());

    let bin = scratch();
    let file_name = if cfg!(windows) { "codex.exe" } else { "codex" };
    write(&bin.join(file_name), "");
    let session = Session::new(&home).with_path(&bin);
    assert!(find("codex").expect("codex").detect(&session));
    assert!(!find("gemini-cli").expect("gemini").detect(&session));
}

#[test]
fn codex_points_at_v1_responses_and_keeps_the_user_model() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    find("codex")
        .expect("codex")
        .apply(&session, &target())
        .expect("apply");
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
    let adapter = find("codex").expect("codex");
    adapter.apply(&session, &target()).expect("apply");
    let text = format!("sandbox_mode = \"workspace-write\"\n{}", read(&path));
    fs::write(&path, text).expect("edit");
    assert!(disconnect(adapter, &session, false).changed);
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
    assert!(!disconnect(adapter, &session, false).changed);
}

#[test]
fn disconnect_refuses_a_broken_file_until_force() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = find("codex").expect("codex");
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
    assert!(disconnect(adapter, &session, true).changed);
    let restored = read(&path);
    assert!(restored.contains("model = \"gpt-5\""));
    assert!(!restored.contains("model_provider"));
    assert!(!sibling(&path, ".hofund.bak").exists());
}

#[test]
fn disconnect_removes_an_empty_directory_it_created() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = find("claude-code").expect("claude");
    adapter.apply(&session, &target()).expect("apply");
    let path = home.join(".claude").join("settings.json");
    assert!(path.is_file());
    assert!(disconnect(adapter, &session, false).changed);
    assert!(!path.exists());
    assert!(!home.join(".claude").exists());
    assert!(!disconnect(adapter, &session, false).changed);
}

#[test]
fn disconnect_keeps_a_directory_that_still_holds_a_file() {
    let home = scratch();
    let session = Session::new(&home);
    let adapter = find("codex").expect("codex");
    adapter.apply(&session, &target()).expect("apply");
    let path = home.join(".codex").join("config.toml");
    let extra = home.join(".codex").join("notes.txt");
    write(&extra, "stay\n");
    assert!(disconnect(adapter, &session, false).changed);
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
    assert!(disconnect(adapter, &session, false).changed);
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
    let adapter = find("codex").expect("codex");
    adapter.apply(&session, &target()).expect("apply");
    let backup = fs::read(sibling(&path, ".hofund.bak")).expect("bak");
    let mut rotated = target();
    rotated.token = "rotated-token".to_owned();
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
    assert!(disconnect(adapter, &session, false).changed);
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
        let adapter = find(id).expect(id);
        adapter.apply(&session, &target()).expect("apply");
        assert_ne!(sha256(&fs::read(&path).expect("connected")), before);
        let outcome = disconnect(adapter, &session, false);
        assert!(outcome.changed, "{id}");
        assert!(outcome.warnings.is_empty(), "{id}: {:?}", outcome.warnings);
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
    let adapter = find("claude-code").expect("claude");
    adapter.apply(&session, &target()).expect("apply");
    let edited = read(&path).replace("local-token", "user-edited-token");
    fs::write(&path, edited).expect("edit");
    let outcome = disconnect(adapter, &session, false);
    assert!(outcome.changed);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("ANTHROPIC_AUTH_TOKEN")),
        "{:?}",
        outcome.warnings
    );
    let after = read(&path);
    assert!(after.contains("user-edited-token"), "{after}");
    assert!(after.contains("https://my-proxy.example"), "{after}");
    assert!(!after.contains("127.0.0.1"), "{after}");
    assert!(!sibling(&path, ".hofund.bak").exists());
    assert!(!disconnect(adapter, &session, true).changed);
}

#[test]
fn configured_model_reads_the_tool_model() {
    let home = scratch();
    let path = home.join(".codex").join("config.toml");
    write(&path, "model = \"gpt-5\"\n");
    let session = Session::new(&home);
    let adapter = find("codex").expect("codex");
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
