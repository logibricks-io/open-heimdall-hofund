//! Tests for compiled descriptions and the shared editing engine.

use std::fs;
use std::path::{Path, PathBuf};

use hofund_agent::{AgentAdapter, Compiled, GatewayTarget, Session, TargetOs, catalog, resolve};
use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, MatchNeedle, ModelField, PluginId,
    Slot, TierId, Visible,
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
