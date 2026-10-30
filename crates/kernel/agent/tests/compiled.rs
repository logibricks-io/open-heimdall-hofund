//! Tests for compiled descriptions and the shared editing engine.

use std::fs;
use std::path::{Path, PathBuf};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Session, TargetOs, catalog, resolve};
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, MatchNeedle, ModelField,
    OwnerTest, PluginId, Slot, TierId, Visible,
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
.with_model_file("config")
.with_model_prefix("hofund/");

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
    let session = Session::new(&*home);
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
    let session = Session::new(&*home).with_env("EXAMPLE_HOME", &*absolute);
    assert_eq!(session.env("EXAMPLE_HOME"), Some(absolute.as_os_str()));
    assert_eq!(
        resolve(&PATH_FILE, TargetOs::Linux, &session),
        absolute.join("config.json")
    );

    let session = Session::new(&*home).with_env("EXAMPLE_HOME", "relative");
    assert_eq!(
        resolve(&PATH_FILE, TargetOs::Linux, &session),
        home.join(".linux").join("config.json")
    );
    assert_eq!(
        resolve(&EXPLICIT_PATH_FILE, TargetOs::Linux, &session),
        home.join(".config").join("example").join(".env")
    );

    let session = Session::new(&*home).with_env("EXAMPLE_HOME", "");
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
                let default_session = Session::new(&*default_home);
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
    let session = Session::new(&*home);
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
    let session = Session::new(&*home);
    Compiled::new(&VISIBLE_ONLY)
        .apply(&session, &target)
        .expect("visible apply");
    let value = read_json(&home.join(".route").join("config.json"));
    let models = value["models"].as_object().expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models["shown"]["name"], "shown");
    assert!(!models.contains_key("hidden"));
}

static ARRAY_FIELDS: &[ModelField] = &[
    ModelField::id("id"),
    ModelField::literal("name", "Hofund"),
    ModelField::json("limits", "{\"context\": 8}"),
];
static ARRAY_SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "flag", Slot::Json("false")),
    AgentSet::from_static(
        "config",
        "models",
        Slot::ModelArray {
            fields: ARRAY_FIELDS,
        },
    ),
];
static ARRAY_JSON_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Json,
    "~/.array/config.json",
    "~/.array/config.json",
    "~/.array/config.json",
    None,
)];
static ARRAY_YAML_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Yaml,
    "~/.array/config.yaml",
    "~/.array/config.yaml",
    "~/.array/config.yaml",
    None,
)];
static ARRAY_JSON: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("array-json"),
    "Array Json",
    "array-json",
    ARRAY_JSON_FILES,
    ARRAY_SETS,
    None,
    None,
    Visible::Only(&["a", "", "b"]),
    AgentRoute::Gateway,
    &[],
);
static ARRAY_YAML: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("array-yaml"),
    "Array Yaml",
    "array-yaml",
    ARRAY_YAML_FILES,
    ARRAY_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static ARRAY_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("array-none"),
    "Array None",
    "array-none",
    ARRAY_JSON_FILES,
    ARRAY_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::None,
    &[],
);

#[test]
fn json_and_model_array_slots_write_raw_values() {
    let target = target_with(&["a", "", "hidden", "b", "a"]);
    let entry =
        |id: &str| serde_json::json!({"id": id, "name": "Hofund", "limits": {"context": 8}});

    let home = scratch();
    let session = Session::new(&*home);
    Compiled::new(&ARRAY_JSON)
        .apply(&session, &target)
        .expect("json apply");
    let value = read_json(&home.join(".array").join("config.json"));
    assert_eq!(value["flag"], serde_json::json!(false));
    assert_eq!(value["models"], serde_json::json!([entry("a"), entry("b")]));

    let home = scratch();
    let session = Session::new(&*home);
    Compiled::new(&ARRAY_YAML)
        .apply(&session, &target)
        .expect("yaml apply");
    let text = fs::read_to_string(home.join(".array").join("config.yaml")).expect("yaml");
    assert_eq!(
        text,
        "flag: false\nmodels:\n  - id: a\n    name: Hofund\n    limits:\n      context: 8\n  - id: hidden\n    name: Hofund\n    limits:\n      context: 8\n  - id: b\n    name: Hofund\n    limits:\n      context: 8\n"
    );

    let home = scratch();
    let session = Session::new(&*home);
    Compiled::new(&ARRAY_NONE)
        .apply(&session, &target)
        .expect("route none apply");
    let value = read_json(&home.join(".array").join("config.json"));
    assert_eq!(value["flag"], serde_json::json!(false));
    assert!(value.get("models").is_none());
}

static APPEND_OWNER: &[OwnerTest] = &[OwnerTest::prefix("id", "custom:hofund/")];
static APPEND_FIELDS: &[ModelField] = &[
    ModelField::id("model"),
    ModelField::prefixed("id", "custom:hofund/"),
    ModelField::literal("displayName", "Hofund"),
    ModelField::v1("baseUrl"),
    ModelField::origin("origin"),
    ModelField::token("apiKey"),
];
static APPEND_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "customModels",
    Slot::Append {
        owner: APPEND_OWNER,
        each_model: true,
        fields: APPEND_FIELDS,
    },
)];
static APPEND_JSON: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("append-json"),
    "Append Json",
    "append-json",
    ARRAY_JSON_FILES,
    APPEND_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static APPEND_YAML: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("append-yaml"),
    "Append Yaml",
    "append-yaml",
    ARRAY_YAML_FILES,
    APPEND_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static APPEND_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("append-none"),
    "Append None",
    "append-none",
    ARRAY_JSON_FILES,
    APPEND_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::None,
    &[],
);

static GROUP_OWNER: &[OwnerTest] = &[
    OwnerTest::equals("vendor", "custom"),
    OwnerTest::equals("name", "hofund"),
];
// The nested `models` field uses the block form the registry renders.
static GROUP_SETS: &[AgentSet] = &[AgentSet::from_static(
    "groups",
    ".",
    Slot::Append {
        owner: GROUP_OWNER,
        each_model: false,
        fields: &[
            ModelField::literal("vendor", "custom"),
            ModelField::literal("name", "hofund"),
            ModelField::models("models", {
                const FIELDS: &[ModelField] = &[ModelField::id("id"), ModelField::token("key")];
                FIELDS
            }),
        ],
    },
)];
static GROUP_FILES: &[AgentFile] = &[AgentFile::from_static(
    "groups",
    FileKind::Json,
    "~/.group/models.json",
    "~/.group/models.json",
    "~/.group/models.json",
    None,
)];
static GROUP_ROOT: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("group-root"),
    "Group Root",
    "group-root",
    GROUP_FILES,
    GROUP_SETS,
    None,
    None,
    Visible::Only(&["a", "b"]),
    AgentRoute::Gateway,
    &[],
);

fn append_path(home: &Path) -> PathBuf {
    home.join(".array").join("config.json")
}

fn ours(id: &str, token: &str) -> serde_json::Value {
    serde_json::json!({
        "model": id,
        "id": format!("custom:hofund/{id}"),
        "displayName": "Hofund",
        "baseUrl": "http://127.0.0.1:3425/v1",
        "origin": "http://127.0.0.1:3425",
        "apiKey": token,
    })
}

#[test]
fn append_keeps_user_entries_and_is_idempotent() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "b"]);
    let path = append_path(&home);
    let user = serde_json::json!({"id": "custom:mine/x", "model": "x"});
    let stale = ours("old", "stale");
    seed(
        &path,
        &serde_json::json!({"theme": "dark", "customModels": [stale, user]}).to_string(),
    );
    let agent = Compiled::new(&APPEND_JSON);
    assert!(!agent.status(&session, &target).expect("status").connected());
    let plan = agent.apply(&session, &target).expect("apply");
    assert_eq!(plan.changes().len(), 1);
    let value = read_json(&path);
    assert_eq!(value["theme"], "dark");
    assert_eq!(
        value["customModels"],
        serde_json::json!([user, ours("a", "local-token"), ours("b", "local-token")])
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    let before = fs::read(&path).expect("read");
    let again = agent.apply(&session, &target).expect("apply again");
    assert!(again.changes().is_empty());
    assert_eq!(fs::read(&path).expect("read"), before);

    let fewer = target_with(&["b"]);
    assert!(!agent.status(&session, &fewer).expect("status").connected());
    agent.apply(&session, &fewer).expect("apply fewer");
    assert_eq!(
        read_json(&path)["customModels"],
        serde_json::json!([user, ours("b", "local-token")])
    );

    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    let value = read_json(&path);
    assert_eq!(value["customModels"], serde_json::json!([stale, user]));
    assert_eq!(value["theme"], "dark");
}

#[test]
fn append_disconnect_reports_an_edited_hofund_entry() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = append_path(&home);
    seed(&path, "{\"theme\": \"dark\"}\n");
    let agent = Compiled::new(&APPEND_JSON);
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    edited["customModels"][0]["apiKey"] = serde_json::json!("user-token");
    edited["theme"] = serde_json::json!("light");
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(outcome.warnings().len(), 1, "{:?}", outcome.warnings());
    let warning = &outcome.warnings()[0];
    assert!(
        warning.contains("removed its entries from `customModels`"),
        "{warning}"
    );
    assert!(warning.contains("config.json"), "{warning}");
    let value = read_json(&path);
    assert!(value.get("customModels").is_none(), "{value}");
    assert_eq!(value["theme"], "light");
}

#[test]
fn append_disconnect_keeps_entries_added_after_connect() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = append_path(&home);
    seed(&path, "{\"theme\": \"dark\"}\n");
    let agent = Compiled::new(&APPEND_JSON);
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    let added = serde_json::json!({"id": "custom:later/y"});
    edited["customModels"]
        .as_array_mut()
        .expect("array")
        .push(added.clone());
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    let value = read_json(&path);
    assert_eq!(value["customModels"], serde_json::json!([added]));
    assert_eq!(value["theme"], "dark");

    let home = scratch();
    let session = Session::new(&*home);
    let path = append_path(&home);
    seed(&path, "{\"theme\": \"dark\"}\n");
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    edited["note"] = serde_json::json!("mine");
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    let value = read_json(&path);
    assert!(value.get("customModels").is_none(), "{value}");
    assert_eq!(value["note"], "mine");

    let home = scratch();
    let session = Session::new(&*home);
    let path = append_path(&home);
    seed(&path, "{\"customModels\": []}\n");
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    edited["note"] = serde_json::json!("mine");
    fs::write(&path, edited.to_string()).expect("user edit");
    agent.disconnect(&session, false).expect("disconnect");
    let value = read_json(&path);
    assert_eq!(value["customModels"], serde_json::json!([]), "{value}");
}

#[test]
fn append_without_a_file_creates_and_removes_it() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = append_path(&home);
    let agent = Compiled::new(&APPEND_JSON);
    agent.apply(&session, &target).expect("apply");
    assert_eq!(
        read_json(&path)["customModels"],
        serde_json::json!([ours("a", "local-token")])
    );
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());

    let home = scratch();
    let session = Session::new(&*home);
    Compiled::new(&APPEND_NONE)
        .apply(&session, &target)
        .expect("route none");
    let path = append_path(&home);
    assert!(
        !path.exists() || read_json(&path).get("customModels").is_none(),
        "{:?}",
        fs::read_to_string(&path)
    );
}

#[test]
fn append_refresh_token_rewrites_only_our_entries() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = append_path(&home);
    let user = serde_json::json!({"id": "custom:mine/x", "apiKey": "user-key"});
    seed(
        &path,
        &serde_json::json!({"customModels": [user]}).to_string(),
    );
    let agent = Compiled::new(&APPEND_JSON);
    agent.apply(&session, &target).expect("apply");
    let fresh = GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "fresh-token",
        vec!["a".to_owned()],
    );
    let updated = agent.refresh_token(&session, &fresh).expect("refresh");
    assert_eq!(updated, vec![path.clone()]);
    assert_eq!(
        read_json(&path)["customModels"],
        serde_json::json!([user, ours("a", "fresh-token")])
    );
    assert!(
        agent
            .refresh_token(&session, &fresh)
            .expect("again")
            .is_empty()
    );
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(
        read_json(&path),
        serde_json::json!({"customModels": [user]})
    );
}

#[test]
fn append_writes_yaml_sequences() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = home.join(".array").join("config.yaml");
    seed(&path, "customModels:\n  - id: mine\n");
    let agent = Compiled::new(&APPEND_YAML);
    agent.apply(&session, &target).expect("apply");
    let text = fs::read_to_string(&path).expect("yaml");
    assert!(
        text.starts_with(
            "customModels:\n  - id: mine\n  - model: a\n    id: \"custom:hofund/a\"\n"
        ),
        "{text}"
    );
    assert!(
        agent
            .apply(&session, &target)
            .expect("again")
            .changes()
            .is_empty()
    );
    fs::write(&path, format!("{text}note: x\n")).expect("user edit");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(
        fs::read_to_string(&path).expect("yaml"),
        "customModels:\n  - id: mine\nnote: x\n"
    );
}

#[test]
fn append_once_at_the_root_writes_one_group() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "hidden", "b"]);
    let path = home.join(".group").join("models.json");
    let user = serde_json::json!({"vendor": "custom", "name": "mine", "models": []});
    seed(&path, &serde_json::json!([user]).to_string());
    let agent = Compiled::new(&GROUP_ROOT);
    agent.apply(&session, &target).expect("apply");
    let group = serde_json::json!({
        "vendor": "custom",
        "name": "hofund",
        "models": [{"id": "a", "key": "local-token"}, {"id": "b", "key": "local-token"}],
    });
    assert_eq!(read_json(&path), serde_json::json!([user, group]));
    assert!(
        agent
            .apply(&session, &target)
            .expect("again")
            .changes()
            .is_empty()
    );
    assert!(agent.status(&session, &target).expect("status").connected());

    let mut edited = read_json(&path);
    edited.as_array_mut().expect("root").clear();
    edited.as_array_mut().expect("root").push(group.clone());
    fs::write(&path, edited.to_string()).expect("user edit");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(read_json(&path), serde_json::json!([]));

    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".group").join("models.json");
    agent.apply(&session, &target).expect("apply");
    assert_eq!(read_json(&path), serde_json::json!([group]));
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());

    // A file hofund created and the user only reformatted is removed too.
    agent.apply(&session, &target).expect("apply");
    let pretty = serde_json::to_string_pretty(&read_json(&path)).expect("pretty");
    fs::write(&path, pretty).expect("user reformat");
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());
}

static MAP_FIELDS: &[ModelField] = &[
    ModelField::id("model"),
    ModelField::json("max_context_size", "128000"),
    ModelField::v1("base_url"),
    ModelField::token("api_key"),
];
static MAP_TOML_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "models",
    Slot::ModelMap {
        prefix: "hof/",
        fields: MAP_FIELDS,
    },
)];
static MAP_TOML_FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Toml,
    "~/.map/config.toml",
    "~/.map/config.toml",
    "~/.map/config.toml",
    None,
)];
static MAP_TOML: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("map-toml"),
    "Map Toml",
    "map-toml",
    MAP_TOML_FILES,
    MAP_TOML_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static MAP_JSON_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "models",
    Slot::ModelMap {
        prefix: "hof/",
        fields: MAP_FIELDS,
    },
)];
static MAP_JSON: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("map-json"),
    "Map Json",
    "map-json",
    ARRAY_JSON_FILES,
    MAP_JSON_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static MAP_WHOLE_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "providers.compat.models",
    Slot::ModelMap {
        prefix: "",
        fields: MAP_FIELDS,
    },
)];
static MAP_WHOLE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("map-whole"),
    "Map Whole",
    "map-whole",
    ARRAY_JSON_FILES,
    MAP_WHOLE_SETS,
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);

static PAIR_SETS: &[AgentSet] = &[
    AgentSet::from_static(
        "config",
        "headers.Authorization",
        Slot::Template("Bearer {token}"),
    ),
    AgentSet::from_static(
        "config",
        "url",
        Slot::Template("{v1}/chat {origin} {nope} {"),
    ),
    AgentSet::from_static(
        "config",
        "provider",
        Slot::ModelPair {
            value: "hofund",
            unless: &["legacy", "other"],
            json: false,
        },
    ),
    AgentSet::from_static("config", "model", Slot::model_if_unset(None)),
];
static PAIR: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair"),
    "Pair",
    "pair",
    ARRAY_JSON_FILES,
    PAIR_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static PAIR_NONE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-none"),
    "Pair None",
    "pair-none",
    ARRAY_JSON_FILES,
    PAIR_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::None,
    &[],
);
static PAIR_ELSEWHERE_SETS: &[AgentSet] = &[AgentSet::from_static(
    "settings",
    "provider",
    Slot::ModelPair {
        value: "hofund",
        unless: &[],
        json: false,
    },
)];
static PAIR_ELSEWHERE: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-elsewhere"),
    "Pair Elsewhere",
    "pair-elsewhere",
    GEMINI_FILES,
    PAIR_ELSEWHERE_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("env");

static PAIR_PREFIXED: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-prefixed"),
    "Pair Prefixed",
    "pair-prefixed",
    ARRAY_JSON_FILES,
    PAIR_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_prefix("p/");
static PAIR_UNOWNED_SETS: &[AgentSet] = &[
    AgentSet::from_static(
        "config",
        "provider",
        Slot::ModelPair {
            value: "hofund",
            unless: &[],
            json: false,
        },
    ),
    AgentSet::from_static("config", "fallback", Slot::model_if_unset(None)),
];
static PAIR_UNOWNED: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-unowned"),
    "Pair Unowned",
    "pair-unowned",
    ARRAY_JSON_FILES,
    PAIR_UNOWNED_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);
static PAIR_FROM_SETS: &[AgentSet] = &[
    AgentSet::from_static(
        "config",
        "provider",
        Slot::ModelPair {
            value: "hofund",
            unless: &[],
            json: false,
        },
    ),
    AgentSet::from_static("config", "model", Slot::model_if_unset(Some("old"))),
];
static PAIR_FROM: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-from"),
    "Pair From",
    "pair-from",
    ARRAY_JSON_FILES,
    PAIR_FROM_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);

#[test]
fn model_pair_reads_the_model_the_file_ends_up_with() {
    let one = target_with(&["a"]);
    let cases: [(&AgentDescription, &str, Option<&str>); 7] = [
        (&PAIR_PREFIXED, "{\"model\":\"p/a\"}", Some("hofund")),
        (&PAIR_PREFIXED, "{}", Some("hofund")),
        (&PAIR_PREFIXED, "{\"model\":\"p/b\"}", None),
        (&PAIR_UNOWNED, "{}", None),
        (&PAIR_UNOWNED, "{\"model\":\"a\"}", Some("hofund")),
        (&PAIR_FROM, "{\"old\":\"gpt\"}", None),
        (&PAIR_FROM, "{\"old\":\"a\"}", Some("hofund")),
    ];
    for (description, original, provider) in cases {
        let home = scratch();
        let session = Session::new(&*home);
        let path = path_in(&home);
        seed(&path, original);
        let agent = Compiled::new(description);
        agent.apply(&session, &one).expect("apply");
        let json = read_json(&path);
        assert_eq!(json["provider"].as_str(), provider, "{original}: {json}");
    }
}

static PAIR_SPLIT_SETS: &[AgentSet] = &[
    AgentSet::from_static(
        "settings",
        "provider",
        Slot::ModelPair {
            value: "hofund",
            unless: &[],
            json: false,
        },
    ),
    AgentSet::from_static("env", "model", Slot::model_if_unset(None)),
];
static PAIR_SPLIT: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-split"),
    "Pair Split",
    "pair-split",
    GEMINI_FILES,
    PAIR_SPLIT_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("settings");

static PAIR_SELF_SETS: &[AgentSet] = &[AgentSet::from_static(
    "settings",
    "model",
    Slot::ModelPair {
        value: "hofund",
        unless: &[],
        json: false,
    },
)];
static PAIR_SELF: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-self"),
    "Pair Self",
    "pair-self",
    GEMINI_FILES,
    PAIR_SELF_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("settings");

static PAIR_URL_SETS: &[AgentSet] = &[AgentSet::from_static(
    "config",
    "modelUrl",
    Slot::ModelPair {
        value: "{v1}",
        unless: &[],
        json: false,
    },
)];
static PAIR_URL: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("pair-url"),
    "Pair Url",
    "pair-url",
    ARRAY_JSON_FILES,
    PAIR_URL_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
);

#[test]
fn model_pair_fills_the_gateway_address() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".array").join("config.json");
    seed(&path, "{\"modelUrl\":\"https://mine.test/v1\"}");
    let target = target_with(&["a"]);
    let agent = Compiled::new(&PAIR_URL);
    agent.set_model(&session, &target, Some("a")).expect("set");
    let json = read_json(&path);
    assert_eq!(json["model"], "a");
    assert_eq!(json["modelUrl"], "http://127.0.0.1:3425/v1");
    agent.set_model(&session, &target, None).expect("clear");
    assert_eq!(read_json(&path)["modelUrl"], "https://mine.test/v1");
}

#[test]
fn template_fills_gateway_values_in_one_pass() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".array").join("config.json");
    seed(&path, "{\"headers\":{\"X-Mine\":\"keep\"}}");
    let target = GatewayTarget::new("http://127.0.0.1:3425/", "t{v1}", vec!["a".to_owned()]);
    let agent = Compiled::new(&PAIR);
    agent.apply(&session, &target).expect("apply");
    let json = read_json(&path);
    assert_eq!(json["headers"]["Authorization"], "Bearer t{v1}");
    assert_eq!(json["headers"]["X-Mine"], "keep");
    assert_eq!(
        json["url"],
        "http://127.0.0.1:3425/v1/chat http://127.0.0.1:3425 {nope} {"
    );
    assert!(agent.status(&session, &target).expect("status").connected());

    let fresh = GatewayTarget::new("http://127.0.0.1:3425/", "fresh", vec!["a".to_owned()]);
    assert_eq!(
        agent.refresh_token(&session, &fresh).expect("refresh"),
        vec![path.clone()]
    );
    assert_eq!(read_json(&path)["headers"]["Authorization"], "Bearer fresh");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(
        read_json(&path),
        serde_json::json!({"headers": {"X-Mine": "keep"}})
    );

    Compiled::new(&PAIR_NONE)
        .apply(&session, &target)
        .expect("apply none");
    let json = read_json(&path);
    assert!(json["headers"].get("Authorization").is_none(), "{json}");
    assert!(json.get("url").is_none(), "{json}");
    assert!(json.get("provider").is_none(), "{json}");
}

#[test]
fn model_pair_follows_the_model_hofund_picks() {
    let target = target_with(&["a"]);
    let agent = Compiled::new(&PAIR);
    let cases = [
        ("{}", Some("a"), Some("hofund")),
        ("{\"model\":\"a\"}", Some("a"), Some("hofund")),
        ("{\"model\":\"gpt\"}", Some("gpt"), None),
        ("{\"provider\":\"openai\"}", Some("a"), Some("openai")),
        ("{\"provider\":\"\"}", Some("a"), Some("hofund")),
        ("{\"other\":\"openai\"}", Some("a"), None),
        ("{\"legacy\":\"openai\"}", Some("a"), None),
    ];
    for (original, model, provider) in cases {
        let home = scratch();
        let session = Session::new(&*home);
        let path = home.join(".array").join("config.json");
        seed(&path, original);
        agent.apply(&session, &target).expect("apply");
        let json = read_json(&path);
        assert_eq!(json["model"].as_str(), model, "{original}: {json}");
        assert_eq!(json["provider"].as_str(), provider, "{original}: {json}");
        agent.disconnect(&session, false).expect("disconnect");
        assert_eq!(
            read_json(&path),
            serde_json::from_str::<serde_json::Value>(original).expect("json")
        );
    }

    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".array").join("config.json");
    let two = target_with(&["a", "b"]);
    agent.apply(&session, &two).expect("apply");
    let json = read_json(&path);
    assert!(
        json.get("model").is_none() && json.get("provider").is_none(),
        "{json}"
    );
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());
}

#[test]
fn model_pair_is_set_and_cleared_with_the_model() {
    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".array").join("config.json");
    let original = "{\"model\":\"gpt\",\"provider\":\"openai\",\"legacy\":\"openai\"}";
    seed(&path, original);
    let two = target_with(&["a", "b"]);
    let agent = Compiled::new(&PAIR);
    agent.set_model(&session, &two, Some("b")).expect("set");
    let json = read_json(&path);
    assert_eq!(json["model"], "b");
    assert_eq!(json["provider"], "hofund");
    assert!(
        agent
            .apply(&session, &two)
            .expect("again")
            .changes()
            .is_empty()
    );
    agent.set_model(&session, &two, None).expect("clear");
    let json = read_json(&path);
    // Clear drops the model and puts back the provider picked before connect.
    assert!(json.get("model").is_none(), "{json}");
    assert_eq!(json["provider"], "openai");
    assert_eq!(json["legacy"], "openai");
    assert!(
        agent
            .apply(&session, &two)
            .expect("after clear")
            .changes()
            .is_empty()
    );
    agent
        .set_model(&session, &two, Some("a"))
        .expect("set again");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(
        read_json(&path),
        serde_json::from_str::<serde_json::Value>(original).expect("json")
    );

    let home = scratch();
    let session = Session::new(&*home);
    let one = target_with(&["a"]);
    seed(&path_in(&home), "{\"provider\":\"openai\"}");
    agent.set_model(&session, &one, Some("a")).expect("set one");
    agent.set_model(&session, &one, None).expect("clear one");
    let json = read_json(&path_in(&home));
    assert_eq!(json["model"], "a");
    assert_eq!(json["provider"], "hofund");

    let home = scratch();
    let session = Session::new(&*home);
    let elsewhere = Compiled::new(&PAIR_ELSEWHERE);
    let settings = home.join(".gemini").join("settings.json");
    seed(&settings, "{\"model\":\"a\"}");
    elsewhere.apply(&session, &one).expect("apply elsewhere");
    assert_eq!(read_json(&settings), serde_json::json!({"model": "a"}));

    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".gemini").join("settings.json");
    seed(&settings, "{}");
    Compiled::new(&PAIR_SPLIT)
        .apply(&session, &one)
        .expect("apply split");
    assert_eq!(read_json(&settings), serde_json::json!({}));

    // A pair on the model key itself has no owner to read and writes nothing.
    let home = scratch();
    let session = Session::new(&*home);
    let settings = home.join(".gemini").join("settings.json");
    seed(&settings, "{}");
    Compiled::new(&PAIR_SELF)
        .apply(&session, &one)
        .expect("apply self");
    assert_eq!(read_json(&settings), serde_json::json!({}));
}

fn path_in(home: &Path) -> PathBuf {
    home.join(".array").join("config.json")
}

fn map_entry(id: &str, token: &str) -> String {
    format!(
        "model = \"{id}\"\nmax_context_size = 128000\nbase_url = \"http://127.0.0.1:3425/v1\"\napi_key = \"{token}\"\n"
    )
}

#[test]
fn model_map_adds_prefixed_toml_tables_and_restores_the_user_file() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a.1", "b"]);
    let path = home.join(".map").join("config.toml");
    let original = "# mine\ndefault_model = \"mine\"\n\n[models.mine]\nmodel = \"x\" # keep\n\n[models.\"hof/old\"]\nmodel = \"old\"\n";
    seed(&path, original);
    let agent = Compiled::new(&MAP_TOML);
    assert!(!agent.status(&session, &target).expect("status").connected());
    agent.apply(&session, &target).expect("apply");
    let text = fs::read_to_string(&path).expect("toml");
    assert_eq!(
        text,
        format!(
            "# mine\ndefault_model = \"mine\"\n\n[models.mine]\nmodel = \"x\" # keep\n\n[models.\"hof/a.1\"]\n{}\n[models.\"hof/b\"]\n{}",
            map_entry("a.1", "local-token"),
            map_entry("b", "local-token")
        )
    );
    assert!(agent.status(&session, &target).expect("status").connected());
    assert!(
        agent
            .apply(&session, &target)
            .expect("again")
            .changes()
            .is_empty()
    );
    let fewer = target_with(&["b"]);
    assert!(!agent.status(&session, &fewer).expect("status").connected());

    let fresh = GatewayTarget::new(
        "http://127.0.0.1:3425/",
        "fresh-token",
        vec!["a.1".to_owned(), "b".to_owned()],
    );
    assert_eq!(
        agent.refresh_token(&session, &fresh).expect("refresh"),
        vec![path.clone()]
    );
    let text = fs::read_to_string(&path).expect("toml");
    assert!(text.contains(&map_entry("b", "fresh-token")), "{text}");
    assert!(!text.contains("local-token"), "{text}");
    assert!(
        agent
            .refresh_token(&session, &fresh)
            .expect("again")
            .is_empty()
    );

    fs::write(&path, format!("{text}\n[extra]\nnote = 1\n")).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(
        fs::read_to_string(&path).expect("toml"),
        "# mine\ndefault_model = \"mine\"\n\n[models.mine]\nmodel = \"x\" # keep\n\n[extra]\nnote = 1\n"
    );

    let home = scratch();
    let session = Session::new(&*home);
    let path = home.join(".map").join("config.toml");
    agent.apply(&session, &target).expect("apply fresh");
    assert!(
        fs::read_to_string(&path)
            .expect("toml")
            .starts_with("[models.\"hof/a.1\"]\n")
    );
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());
}

#[test]
fn model_map_with_an_empty_prefix_owns_the_whole_map() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "", "b", "a"]);
    let path = append_path(&home);
    let mine = serde_json::json!({"mine": {"model": "x"}});
    seed(
        &path,
        &serde_json::json!({"theme": "dark", "providers": {"compat": {"models": mine, "name": "n"}}})
            .to_string(),
    );
    let agent = Compiled::new(&MAP_WHOLE);
    agent.apply(&session, &target).expect("apply");
    let entry = |id: &str| {
        serde_json::json!({
            "model": id,
            "max_context_size": 128000,
            "base_url": "http://127.0.0.1:3425/v1",
            "api_key": "local-token",
        })
    };
    let value = read_json(&path);
    assert_eq!(
        value["providers"]["compat"]["models"],
        serde_json::json!({"a": entry("a"), "b": entry("b")})
    );
    assert_eq!(value["providers"]["compat"]["name"], "n");
    assert!(agent.status(&session, &target).expect("status").connected());
    assert!(
        agent
            .apply(&session, &target)
            .expect("again")
            .changes()
            .is_empty()
    );
    let mut edited = read_json(&path);
    edited["note"] = serde_json::json!("mine");
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    let value = read_json(&path);
    assert_eq!(value["providers"]["compat"]["models"], mine);
    assert_eq!(value["theme"], "dark");
    assert_eq!(value["note"], "mine");

    let home = scratch();
    let session = Session::new(&*home);
    let path = append_path(&home);
    agent.apply(&session, &target).expect("apply fresh");
    agent.disconnect(&session, false).expect("disconnect");
    assert!(!path.exists());
}

#[test]
fn model_map_disconnect_drops_a_map_hofund_created() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a"]);
    let path = append_path(&home);
    seed(&path, "{\"theme\": \"dark\"}\n");
    let agent = Compiled::new(&MAP_JSON);
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    assert_eq!(
        edited["models"]
            .as_object()
            .expect("map")
            .keys()
            .collect::<Vec<_>>(),
        ["hof/a"]
    );
    edited["note"] = serde_json::json!("mine");
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = agent.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(
        read_json(&path),
        serde_json::json!({"theme": "dark", "note": "mine"})
    );

    let home = scratch();
    let session = Session::new(&*home);
    let path = append_path(&home);
    seed(&path, "{\"models\": {\"mine\": 1}}\n");
    agent.apply(&session, &target).expect("apply");
    let mut edited = read_json(&path);
    edited["models"]["later"] = serde_json::json!(2);
    fs::write(&path, edited.to_string()).expect("user edit");
    agent.disconnect(&session, false).expect("disconnect");
    assert_eq!(
        read_json(&path),
        serde_json::json!({"models": {"mine": 1, "later": 2}})
    );
}

fn codex_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

fn claude_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn seed(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("seed");
}

fn toml_value(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).expect("TOML file");
    let document: toml_edit::DocumentMut = text.parse().expect("TOML");
    document
        .get(key)
        .and_then(|item| item.as_str())
        .map(str::to_owned)
}

#[test]
fn set_model_connects_codex_and_disconnect_restores_the_original() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["m", "n"]);
    let path = codex_path(&home);
    seed(&path, "model = \"mine\"\n");
    let codex = Compiled::new(&CODEX);
    let plan = codex
        .set_model(&session, &target, Some("n"))
        .expect("set model");
    assert_eq!(plan.changes().len(), 1);
    assert_eq!(toml_value(&path, "model").as_deref(), Some("n"));
    assert_eq!(
        toml_value(&path, "model_provider").as_deref(),
        Some("hofund")
    );
    assert_eq!(
        codex.configured_model(&session).expect("model").as_deref(),
        Some("n")
    );
    let again = codex
        .set_model(&session, &target, Some("n"))
        .expect("set again");
    assert!(again.changes().is_empty());

    let edited = format!("{}extra = 1\n", fs::read_to_string(&path).expect("read"));
    fs::write(&path, edited).expect("user edit");
    let outcome = codex.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(toml_value(&path, "model").as_deref(), Some("mine"));
    assert!(toml_value(&path, "model_provider").is_none());
    assert!(
        fs::read_to_string(&path)
            .expect("read")
            .contains("extra = 1")
    );
}

#[test]
fn codex_model_edits_after_a_plain_connect_stay_quiet() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["m", "n"]);
    let path = codex_path(&home);
    seed(&path, "model = \"mine\"\n");
    let codex = Compiled::new(&CODEX);
    codex.apply(&session, &target).expect("connect");
    let edited = fs::read_to_string(&path)
        .expect("read")
        .replace("model = \"mine\"", "model = \"theirs\"");
    fs::write(&path, edited).expect("user edit");
    let outcome = codex.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(toml_value(&path, "model").as_deref(), Some("theirs"));
}

#[test]
fn codex_model_edit_survives_a_plain_reconnect() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["m", "n"]);
    let path = codex_path(&home);
    seed(&path, "model = \"mine\"\n");
    let codex = Compiled::new(&CODEX);
    codex.apply(&session, &target).expect("connect");
    let edited = fs::read_to_string(&path)
        .expect("read")
        .replace("model = \"mine\"", "model = \"theirs\"");
    fs::write(&path, edited).expect("user edit");
    codex.apply(&session, &target).expect("reconnect");
    assert_eq!(toml_value(&path, "model").as_deref(), Some("theirs"));
    let outcome = codex.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(toml_value(&path, "model").as_deref(), Some("theirs"));
}

#[test]
fn codex_model_edited_after_set_is_kept_with_a_warning() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["m", "n"]);
    let path = codex_path(&home);
    seed(&path, "model = \"mine\"\n");
    let codex = Compiled::new(&CODEX);
    codex
        .set_model(&session, &target, Some("m"))
        .expect("set model");
    let edited = fs::read_to_string(&path)
        .expect("read")
        .replace("model = \"m\"", "model = \"theirs\"");
    fs::write(&path, edited).expect("user edit");
    let outcome = codex.disconnect(&session, false).expect("disconnect");
    assert_eq!(outcome.warnings().len(), 1, "{:?}", outcome.warnings());
    assert!(
        outcome.warnings()[0].contains("`model`"),
        "{:?}",
        outcome.warnings()
    );
    assert_eq!(toml_value(&path, "model").as_deref(), Some("theirs"));
}

#[test]
fn set_model_moves_claude_tiers_and_rejects_unusable_models() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "b"]);
    let path = claude_path(&home);
    let claude = Compiled::new(&CLAUDE);
    claude
        .set_model(&session, &target, Some("b"))
        .expect("set model");
    let value = read_json(&path);
    assert_eq!(value["model"], "b");
    assert_eq!(value["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "b");
    assert_eq!(value["env"]["ANTHROPIC_SMALL_FAST_MODEL"], "b");
    assert!(value["env"].get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
    assert!(
        claude
            .status(&session, &target)
            .expect("status")
            .connected()
    );
    claude
        .set_model(&session, &target, Some("a"))
        .expect("switch model");
    let value = read_json(&path);
    assert_eq!(value["model"], "a");
    assert_eq!(value["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "a");

    let error = claude
        .set_model(&session, &target, Some("zzz"))
        .expect_err("unknown model");
    assert!(error.to_string().contains("hofund model list"), "{error}");
    assert_eq!(read_json(&path)["model"], "a");

    let home = scratch();
    let session = Session::new(&*home);
    let error = Compiled::new(&ROUTE_NONE)
        .set_model(&session, &target_with(&["shown", "hidden"]), Some("hidden"))
        .expect_err("hidden model");
    assert!(error.to_string().contains("`hidden`"), "{error}");
    let error = Compiled::new(&VISIBLE_ONLY)
        .set_model(&session, &target_with(&["shown"]), Some("shown"))
        .expect_err("no model key");
    assert_eq!(error.to_string(), "`visible-only` has no model setting");
    assert!(!home.join(".route").exists());
}

#[test]
fn clear_model_on_a_connected_agent_falls_back_and_disconnect_restores() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "b"]);
    let path = claude_path(&home);
    seed(&path, "{\"model\": \"orig\", \"theme\": \"dark\"}\n");
    let claude = Compiled::new(&CLAUDE);
    claude
        .set_model(&session, &target, Some("b"))
        .expect("set model");
    let plan = claude.set_model(&session, &target, None).expect("clear");
    assert_eq!(plan.changes().len(), 1);
    let value = read_json(&path);
    assert!(value.get("model").is_none(), "{value}");
    assert!(
        value["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none(),
        "{value}"
    );
    assert!(value["env"].get("ANTHROPIC_AUTH_TOKEN").is_some());
    assert!(claude.configured_model(&session).expect("model").is_none());

    let mut edited = read_json(&path);
    edited["note"] = serde_json::json!("mine");
    fs::write(&path, edited.to_string()).expect("user edit");
    let outcome = claude.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    let value = read_json(&path);
    assert_eq!(value["model"], "orig");
    assert_eq!(value["note"], "mine");
    assert!(value.get("env").is_none(), "{value}");
}

#[test]
fn clear_model_with_one_route_keeps_the_connect_default() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["only"]);
    let path = claude_path(&home);
    seed(&path, "{\"model\": \"orig\"}\n");
    let claude = Compiled::new(&CLAUDE);
    claude.apply(&session, &target).expect("connect");
    assert_eq!(read_json(&path)["model"], "orig");
    claude.set_model(&session, &target, None).expect("clear");
    let value = read_json(&path);
    assert_eq!(value["model"], "only");
    assert_eq!(value["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "only");
    assert!(
        claude
            .status(&session, &target)
            .expect("status")
            .connected()
    );
    let outcome = claude.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        "{\"model\": \"orig\"}\n"
    );
}

#[test]
fn plain_connect_after_set_or_clear_keeps_restoring_the_original() {
    for model in [Some("b"), None] {
        let home = scratch();
        let session = Session::new(&*home);
        let target = target_with(&["a", "b"]);
        let path = codex_path(&home);
        seed(&path, "model = \"mine\"\n");
        let codex = Compiled::new(&CODEX);
        codex
            .set_model(&session, &target, Some("a"))
            .expect("set model");
        codex.set_model(&session, &target, model).expect("change");
        codex.apply(&session, &target).expect("reconnect");
        assert_eq!(toml_value(&path, "model").as_deref(), model);
        let edited = format!("{}extra = 1\n", fs::read_to_string(&path).expect("read"));
        fs::write(&path, edited).expect("user edit");
        let outcome = codex.disconnect(&session, false).expect("disconnect");
        assert!(
            outcome.warnings().is_empty(),
            "{model:?}: {:?}",
            outcome.warnings()
        );
        assert_eq!(
            toml_value(&path, "model").as_deref(),
            Some("mine"),
            "{model:?}"
        );
    }
}

#[test]
fn clear_model_on_a_disconnected_agent_only_removes_the_key() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "b"]);
    let path = claude_path(&home);
    let claude = Compiled::new(&CLAUDE);
    let plan = claude.set_model(&session, &target, None).expect("no file");
    assert!(plan.changes().is_empty());
    assert!(!path.exists());

    seed(
        &path,
        "{\"model\": \"x\", \"env\": {\"ANTHROPIC_DEFAULT_OPUS_MODEL\": \"y\"}}\n",
    );
    let plan = claude.set_model(&session, &target, None).expect("clear");
    assert_eq!(plan.changes().len(), 1);
    let value = read_json(&path);
    assert!(value.get("model").is_none(), "{value}");
    assert_eq!(value["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "y");
    let siblings: Vec<_> = fs::read_dir(path.parent().expect("dir"))
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(siblings.len(), 1, "{siblings:?}");
    let before = fs::read(&path).expect("read");
    let plan = claude
        .set_model(&session, &target, None)
        .expect("clear again");
    assert!(plan.changes().is_empty());
    assert_eq!(fs::read(&path).expect("read"), before);
    assert!(
        !claude
            .status(&session, &target)
            .expect("status")
            .connected()
    );

    let path = home.join(".gemini").join("settings.json");
    seed(&path, "{\"model\": \"legacy\", \"theme\": \"t\"}\n");
    Compiled::new(&GEMINI)
        .set_model(&session, &target, None)
        .expect("clear gemini");
    let value = read_json(&path);
    assert!(value.get("model").is_none(), "{value}");
    assert_eq!(value["theme"], "t");
}

#[test]
fn set_model_writes_prefixed_and_nested_keys() {
    let home = scratch();
    let session = Session::new(&*home);
    let target = target_with(&["a", "b"]);
    Compiled::new(&OPENCODE)
        .set_model(&session, &target, Some("b"))
        .expect("opencode");
    let value = read_json(&home.join(".config").join("opencode").join("opencode.json"));
    assert_eq!(value["model"], "hofund/b");
    assert!(value["provider"]["hofund"]["models"]["a"].is_object());

    let path = home.join(".gemini").join("settings.json");
    seed(&path, "{\"model\": \"legacy\"}\n");
    let gemini = Compiled::new(&GEMINI);
    gemini
        .set_model(&session, &target, Some("a"))
        .expect("gemini");
    assert_eq!(read_json(&path)["model"]["name"], "a");
    let outcome = gemini.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(read_json(&path)["model"], "legacy");
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

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

fn scratch() -> Scratch {
    Scratch(tempfile::tempdir().expect("temp dir"))
}
