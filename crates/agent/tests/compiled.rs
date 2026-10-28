//! Tests for compiled descriptions and the shared editing engine.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Session, TargetOs, catalog, resolve};
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, MatchNeedle, PluginId, Slot,
    TierId, Visible,
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
    AgentSet::from_static(
        "settings",
        "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
        Slot::ModelTier {
            tier: TierId::from_static("opus"),
            when_model_matches: None,
        },
    ),
    AgentSet::from_static(
        "settings",
        "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
        Slot::ModelTier {
            tier: TierId::from_static("sonnet"),
            when_model_matches: None,
        },
    ),
    AgentSet::from_static(
        "settings",
        "env.ANTHROPIC_DEFAULT_HAIKU_MODEL",
        Slot::ModelTier {
            tier: TierId::from_static("haiku"),
            when_model_matches: None,
        },
    ),
    AgentSet::from_static(
        "settings",
        "env.ANTHROPIC_DEFAULT_FABLE_MODEL",
        Slot::ModelTier {
            tier: TierId::from_static("fable"),
            when_model_matches: Some(MatchNeedle::new("claude-fable-")),
        },
    ),
    AgentSet::from_static(
        "settings",
        "env.ANTHROPIC_SMALL_FAST_MODEL",
        Slot::ModelTier {
            tier: TierId::from_static("fast"),
            when_model_matches: None,
        },
    ),
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

static GEMINI_FILES: &[AgentFile] = &[
    AgentFile::from_static(
        "settings",
        FileKind::Json,
        "~/.gemini/settings.json",
        "~/.gemini/settings.json",
        "~/.gemini/settings.json",
        Some("GEMINI_CLI_HOME"),
    )
    .with_env_replaces("~"),
    AgentFile::from_static(
        "env",
        FileKind::Env,
        "~/.gemini/.env",
        "~/.gemini/.env",
        "~/.gemini/.env",
        Some("GEMINI_CLI_HOME"),
    )
    .with_env_replaces("~"),
];
static GEMINI_SETS: &[AgentSet] = &[
    AgentSet::from_static(
        "settings",
        "security.auth.selectedType",
        Slot::Literal("gemini-api-key"),
    ),
    AgentSet::from_static("env", "GEMINI_API_KEY", Slot::Token),
    AgentSet::from_static("env", "GOOGLE_GEMINI_BASE_URL", Slot::Origin),
    AgentSet::from_static(
        "env",
        "GEMINI_API_KEY_AUTH_MECHANISM",
        Slot::Literal("bearer"),
    ),
    AgentSet::from_static(
        "settings",
        "model.name",
        Slot::model_if_unset(Some("model")),
    ),
];
static GEMINI: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("gemini-cli"),
    "Gemini CLI",
    "gemini",
    GEMINI_FILES,
    GEMINI_SETS,
    Some("model.name"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("settings");

static OPENCODE_FILES: &[AgentFile] = &[
    AgentFile::from_static(
        "config",
        FileKind::Json,
        "~/.config/opencode/opencode.jsonc",
        "~/.config/opencode/opencode.jsonc",
        "~/.config/opencode/opencode.jsonc",
        Some("XDG_CONFIG_HOME"),
    )
    .with_env_replaces("~/.config")
    .with_create(false),
    AgentFile::from_static(
        "config",
        FileKind::Json,
        "~/.config/opencode/opencode.json",
        "~/.config/opencode/opencode.json",
        "~/.config/opencode/opencode.json",
        Some("XDG_CONFIG_HOME"),
    )
    .with_env_replaces("~/.config"),
];
static OPENCODE_SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "provider.hofund.name", Slot::Literal("hofund")),
    AgentSet::from_static(
        "config",
        "provider.hofund.npm",
        Slot::Literal("@ai-sdk/openai-compatible"),
    ),
    AgentSet::from_static("config", "provider.hofund.options.baseURL", Slot::V1),
    AgentSet::from_static("config", "provider.hofund.options.apiKey", Slot::Token),
    AgentSet::from_static("config", "provider.hofund.models", Slot::CatalogModels),
];
static OPENCODE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("opencode"),
    "OpenCode",
    "opencode",
    OPENCODE_FILES,
    OPENCODE_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("config");

static DESCRIPTIONS: &[&AgentDescription] = &[&CLAUDE, &CODEX, &GEMINI, &OPENCODE];

static PATH_FILE: AgentFile = AgentFile::from_static(
    "config",
    FileKind::Json,
    "~/.linux/config.json",
    "~/.macos/config.json",
    "~/.windows/config.json",
    Some("EXAMPLE_HOME"),
);
static EXPLICIT_PATH_FILE: AgentFile = AgentFile::from_static(
    "config",
    FileKind::Env,
    "~/.config/example/.env",
    "~/.config/example/.env",
    "~/.config/example/.env",
    Some("EXAMPLE_HOME"),
)
.with_env_replaces("~/.config")
.with_create(false);

static EXTRA: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("compiled-extra"),
    "Compiled Extra",
    "extra",
    &[],
    &[],
    None,
    None,
    Visible::All,
    AgentRoute::None,
    &[],
);

static MIGRATED_AND_EXTRA_LIST: &[&AgentDescription] =
    &[&CLAUDE, &CODEX, &GEMINI, &OPENCODE, &EXTRA];
static CLAUDE_LIST: &[&AgentDescription] = &[&CLAUDE];
static DUPLICATE_LIST: &[&AgentDescription] = &[&EXTRA, &EXTRA];

static ROUTE_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Json,
    "~/.route/config.json",
    "~/.route/config.json",
    "~/.route/config.json",
    None,
)];
static ROUTE_SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "origin", Slot::Origin),
    AgentSet::from_static("config", "token", Slot::Token),
    AgentSet::from_static("config", "models", Slot::CatalogModels),
    AgentSet::from_static("config", "model", Slot::model_if_unset(None)),
    AgentSet::from_static("config", "literal", Slot::Literal("kept")),
];
static ROUTE_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("route-none"),
    "Route None",
    "route-none",
    ROUTE_FILES,
    ROUTE_SETS,
    Some("model"),
    None,
    Visible::Only(&["shown"]),
    AgentRoute::None,
    &[],
);

static VISIBLE_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "models",
    Slot::CatalogModels,
)];
static VISIBLE_ONLY: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("visible-only"),
    "Visible Only",
    "visible-only",
    ROUTE_FILES,
    VISIBLE_SETS,
    None,
    None,
    Visible::Only(&["shown"]),
    AgentRoute::Gateway,
    &[],
);

#[test]
fn path_resolution_selects_os_and_handles_environment_overrides() {
    let home = scratch();
    let session = Session::new(&home);
    for (os, directory) in [
        (TargetOs::Linux, ".linux"),
        (TargetOs::Macos, ".macos"),
        (TargetOs::Windows, ".windows"),
    ] {
        assert_eq!(
            resolve(&PATH_FILE, os, &session),
            home.join(directory).join("config.json")
        );
    }
    assert_eq!(TargetOs::current(), TargetOs::current());
    assert!(PATH_FILE.create());
    assert_eq!(PATH_FILE.env_replaces(), None);
    assert!(!EXPLICIT_PATH_FILE.create());
    assert_eq!(EXPLICIT_PATH_FILE.env_replaces(), Some("~/.config"));

    let absolute = scratch();
    let session = Session::new(&home).with_env("EXAMPLE_HOME", &absolute);
    assert_eq!(session.env("EXAMPLE_HOME"), Some(absolute.as_os_str()));
    assert_eq!(
        resolve(&PATH_FILE, TargetOs::Linux, &session),
        absolute.join("config.json")
    );

    let session = Session::new(&home).with_env("EXAMPLE_HOME", "relative");
    assert_eq!(
        resolve(&PATH_FILE, TargetOs::Linux, &session),
        home.join(".linux").join("config.json")
    );
    assert_eq!(
        resolve(&EXPLICIT_PATH_FILE, TargetOs::Linux, &session),
        home.join(".config").join("example").join(".env")
    );

    let session = Session::new(&home).with_env("EXAMPLE_HOME", "");
    assert_eq!(
        resolve(&PATH_FILE, TargetOs::Linux, &session),
        home.join(".linux").join("config.json")
    );
}

#[derive(Clone, Copy, Debug)]
enum OverrideSource {
    NamedSetter,
    WithEnv,
    FromEnvIter,
}

#[derive(Clone, Copy, Debug)]
enum OverrideValue {
    Empty,
    Absolute,
    Relative,
}

#[test]
fn environment_overrides_select_compiled_adapter_paths() {
    let target = target_with(&["route-a"]);
    for description in DESCRIPTIONS {
        let id = description.id().as_str();
        let compiled = Compiled::new(description);
        for source in [
            OverrideSource::NamedSetter,
            OverrideSource::WithEnv,
            OverrideSource::FromEnvIter,
        ] {
            for value_kind in [
                OverrideValue::Empty,
                OverrideValue::Absolute,
                OverrideValue::Relative,
            ] {
                let context = format!("{id} / {source:?} / {value_kind:?}");
                let default_home = scratch();
                let override_home = scratch();
                let default_session = Session::new(&default_home);
                let value = override_value(&override_home, value_kind);
                let override_session = override_session(&override_home, id, &value, source);
                let default_plan = compiled
                    .plan(&default_session, &target)
                    .expect("default plan");
                let override_plan = compiled
                    .plan(&override_session, &target)
                    .expect("override plan");
                if matches!(value_kind, OverrideValue::Absolute) {
                    assert_eq!(
                        planned_paths(&override_plan, &override_home),
                        expected_absolute_paths(id),
                        "{context}: absolute paths"
                    );
                } else {
                    assert_plan(
                        &default_plan,
                        &override_plan,
                        &default_home,
                        &override_home,
                        &context,
                    );
                }
            }
        }
    }
}

fn override_value(home: &Path, kind: OverrideValue) -> PathBuf {
    match kind {
        OverrideValue::Empty => PathBuf::new(),
        OverrideValue::Absolute => home.join("absolute-override"),
        OverrideValue::Relative => PathBuf::from("relative-override"),
    }
}

fn override_session(home: &Path, id: &str, value: &Path, source: OverrideSource) -> Session {
    let name = match id {
        "claude-code" => "CLAUDE_CONFIG_DIR",
        "codex" => "CODEX_HOME",
        "gemini-cli" => "GEMINI_CLI_HOME",
        "opencode" => "XDG_CONFIG_HOME",
        _ => panic!("unknown description {id}"),
    };
    let session = Session::new(home);
    match (id, source) {
        (_, OverrideSource::WithEnv) => session.with_env(name, value),
        (_, OverrideSource::FromEnvIter) => {
            Session::from_env_iter(home, [(name, value.as_os_str())])
        }
        ("claude-code", OverrideSource::NamedSetter) => session.with_claude_config_dir(value),
        ("codex", OverrideSource::NamedSetter) => session.with_codex_home(value),
        ("gemini-cli", OverrideSource::NamedSetter) => session.with_gemini_cli_home(value),
        ("opencode", OverrideSource::NamedSetter) => session.with_xdg_config_home(value),
        _ => panic!("unknown description {id}"),
    }
}

fn planned_paths(plan: &hofund_agent::Plan, home: &Path) -> Vec<PathBuf> {
    plan.changes()
        .iter()
        .map(|change| {
            change
                .path()
                .strip_prefix(home)
                .expect("override path in home")
                .to_path_buf()
        })
        .collect()
}

fn expected_absolute_paths(id: &str) -> Vec<PathBuf> {
    let paths: &[&str] = match id {
        "claude-code" => &["absolute-override/settings.json"],
        "codex" => &["absolute-override/config.toml"],
        "gemini-cli" => &[
            "absolute-override/.gemini/settings.json",
            "absolute-override/.gemini/.env",
        ],
        "opencode" => &["absolute-override/opencode/opencode.json"],
        _ => panic!("unknown description {id}"),
    };
    paths.iter().map(PathBuf::from).collect()
}

#[test]
fn catalog_checks_collisions_and_keeps_builtins_first() {
    let with_extra = catalog(MIGRATED_AND_EXTRA_LIST).expect("extra catalog");
    let ids = with_extra
        .adapters()
        .into_iter()
        .map(|adapter| adapter.id())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "claude-code",
            "codex",
            "gemini-cli",
            "opencode",
            "compiled-extra"
        ]
    );
    assert!(catalog(CLAUDE_LIST).is_err());
    assert!(catalog(DUPLICATE_LIST).is_err());
}

#[test]
fn visible_models_and_route_none_filter_gateway_slots() {
    let target = target_with(&["shown", "hidden"]);

    let home = scratch();
    let session = Session::new(&home);
    Compiled::new(&ROUTE_NONE)
        .apply(&session, &target)
        .expect("route none apply");
    let value = read_json(&home.join(".route").join("config.json"));
    assert_eq!(value["model"], "shown");
    assert_eq!(value["literal"], "kept");
    assert!(value.get("origin").is_none());
    assert!(value.get("token").is_none());
    assert!(value.get("models").is_none());

    let home = scratch();
    let session = Session::new(&home);
    Compiled::new(&VISIBLE_ONLY)
        .apply(&session, &target)
        .expect("visible apply");
    let value = read_json(&home.join(".route").join("config.json"));
    let models = value["models"].as_object().expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models["shown"]["name"], "shown");
    assert!(!models.contains_key("hidden"));
}

fn assert_plan(
    default_plan: &hofund_agent::Plan,
    override_plan: &hofund_agent::Plan,
    default_home: &Path,
    override_home: &Path,
    context: &str,
) {
    let normalize = |plan: &hofund_agent::Plan, home: &Path| {
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
            .collect::<Vec<_>>()
    };
    assert_eq!(
        normalize(default_plan, default_home),
        normalize(override_plan, override_home),
        "{context}: plan bytes"
    );
}

fn target_with(models: &[&str]) -> GatewayTarget {
    GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "local-token",
        models.iter().map(|model| (*model).to_owned()).collect(),
    )
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("JSON file")).expect("JSON")
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hofund-compiled-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).expect("dir");
    path
}
