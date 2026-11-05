//! Sidecar checkpoint rollback when a multi-agent edit fails partway through.

use std::os::unix::fs::PermissionsExt;

use hofund_agent::{
    GatewayTarget, Session, SidecarCheckpoint, catalog_from, reseat_after_provider_change,
};
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId, Slot, Visible,
};

static CLAUDE_FILES: &[AgentFile] = &[AgentFile::from_static(
    "settings",
    FileKind::Json,
    "~/.claude/settings.json",
    "~/.claude/settings.json",
    "~/.claude/settings.json",
    Some("CLAUDE_CONFIG_DIR"),
)];
static CLAUDE_SETS: &[AgentSet] = &[
    AgentSet::from_static("settings", "env.ANTHROPIC_BASE_URL", Slot::Origin),
    AgentSet::from_static("settings", "env.ANTHROPIC_AUTH_TOKEN", Slot::Token),
    AgentSet::from_static("settings", "model", Slot::model_if_unset(None)),
];
static CLAUDE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("claude-code"),
    "Claude Code",
    "claude",
    CLAUDE_FILES,
    CLAUDE_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("settings");

static CODEX_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Toml,
    "~/.codex/config.toml",
    "~/.codex/config.toml",
    "~/.codex/config.toml",
    Some("CODEX_HOME"),
)];
static CODEX_SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "model_provider", Slot::Literal("hofund")),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.name",
        Slot::Literal("hofund"),
    ),
    AgentSet::from_static("config", "model_providers.hofund.base_url", Slot::V1),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.wire_api",
        Slot::Literal("responses"),
    ),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.experimental_bearer_token",
        Slot::Token,
    ),
    AgentSet::from_static("config", "model", Slot::model_if_unset(None)),
];
static CODEX: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("codex"),
    "Codex CLI",
    "codex",
    CODEX_FILES,
    CODEX_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("config");

static COMPILED: &[&AgentDescription] = &[&CLAUDE, &CODEX];

fn two_agent_session(dir: &tempfile::TempDir) -> (Session, std::path::PathBuf, std::path::PathBuf) {
    let claude_settings = dir
        .path()
        .join("home")
        .join(".claude")
        .join("settings.json");
    std::fs::create_dir_all(claude_settings.parent().expect("claude parent")).expect("claude dir");
    std::fs::write(&claude_settings, r#"{"model":"on-spare"}"#).expect("claude settings");
    let codex_config = dir.path().join("home").join(".codex").join("config.toml");
    std::fs::create_dir_all(codex_config.parent().expect("codex parent")).expect("codex dir");
    std::fs::write(
        &codex_config,
        "model = \"on-spare\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let session = Session::new(dir.path().join("home"));
    (session, claude_settings, codex_config)
}

#[test]
fn r7a_reseat_restores_first_agent_when_second_write_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let (session, claude_settings, codex_config) = two_agent_session(&dir);
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let catalog = catalog_from(COMPILED).expect("catalog");
    let target = GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "secret-bearer-token-value",
        Vec::new(),
    );
    let sendable_fallback = vec!["normal".to_owned()];
    let agent_models = |_agent: &str| vec!["normal".to_owned()];
    let mut checkpoint = SidecarCheckpoint::new();
    let error = reseat_after_provider_change(
        &catalog,
        &session,
        &target,
        "spare",
        false,
        &["on-spare".to_owned()],
        &sendable_fallback,
        agent_models,
        &mut checkpoint,
    )
    .expect_err("codex write must fail when its config is inaccessible");
    assert!(!error.to_string().is_empty());
    checkpoint.restore().expect("restore first agent write");
    assert_eq!(
        std::fs::read_to_string(&claude_settings).expect("claude restored"),
        r#"{"model":"on-spare"}"#
    );
}

#[test]
fn r7a_rename_group_refs_restores_first_agent_when_second_write_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let (session, claude_settings, codex_config) = two_agent_session(&dir);
    std::fs::write(&claude_settings, r#"{"model":"group/old"}"#).expect("claude group model");
    std::fs::write(
        &codex_config,
        "model = \"group/old\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex group model");
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let catalog = catalog_from(COMPILED).expect("catalog");
    let mut checkpoint = SidecarCheckpoint::new();
    let error = catalog
        .rename_group_refs(&session, "old", "new", &mut checkpoint)
        .expect_err("codex rename must fail when its config is inaccessible");
    assert!(!error.to_string().is_empty());
    checkpoint.restore().expect("restore claude rename");
    assert_eq!(
        std::fs::read_to_string(&claude_settings).expect("claude restored"),
        r#"{"model":"group/old"}"#
    );
}
