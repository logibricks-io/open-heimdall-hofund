//! Equivalence tests for compiled descriptions and const-backed adapters.

use std::collections::BTreeMap;
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

fn const_adapter(id: &str) -> &'static dyn AgentAdapter {
    match id {
        "claude-code" => hofund_agent::legacy::claude_code(),
        "codex" => hofund_agent::legacy::codex(),
        "gemini-cli" => hofund_agent::legacy::gemini_cli(),
        "opencode" => hofund_agent::legacy::opencode(),
        _ => panic!("no const adapter for {id}"),
    }
}

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
fn environment_overrides_match_for_const_and_compiled_adapters() {
    let target = target_with(&["route-a"]);
    for description in DESCRIPTIONS {
        let id = description.id().as_str();
        let const_adapter = const_adapter(id);
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
                let const_default_home = scratch();
                let compiled_default_home = scratch();
                let const_override_home = scratch();
                let compiled_override_home = scratch();
                let const_default = Session::new(&const_default_home);
                let compiled_default = Session::new(&compiled_default_home);
                let const_value = override_value(&const_override_home, value_kind);
                let compiled_value = override_value(&compiled_override_home, value_kind);
                let const_override =
                    override_session(&const_override_home, id, &const_value, source);
                let compiled_override =
                    override_session(&compiled_override_home, id, &compiled_value, source);

                let const_default_plan = const_adapter
                    .plan(&const_default, &target)
                    .expect("const default plan");
                let compiled_default_plan = compiled
                    .plan(&compiled_default, &target)
                    .expect("compiled default plan");
                let const_override_plan = const_adapter
                    .plan(&const_override, &target)
                    .expect("const override plan");
                let compiled_override_plan = compiled
                    .plan(&compiled_override, &target)
                    .expect("compiled override plan");
                if matches!(value_kind, OverrideValue::Absolute) {
                    assert_eq!(
                        planned_paths(&const_override_plan, &const_override_home),
                        expected_absolute_paths(id),
                        "{context}: const absolute paths"
                    );
                    assert_eq!(
                        planned_paths(&compiled_override_plan, &compiled_override_home),
                        expected_absolute_paths(id),
                        "{context}: compiled absolute paths"
                    );
                } else {
                    assert_plan(
                        &const_default_plan,
                        &const_override_plan,
                        &const_default_home,
                        &const_override_home,
                        &context,
                    );
                    assert_plan(
                        &compiled_default_plan,
                        &compiled_override_plan,
                        &compiled_default_home,
                        &compiled_override_home,
                        &context,
                    );
                }
                assert_plan(
                    &const_override_plan,
                    &compiled_override_plan,
                    &const_override_home,
                    &compiled_override_home,
                    &context,
                );
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

#[cfg(feature = "legacy-agent-consts")]
#[test]
fn catalog_without_compiled_codex_uses_the_legacy_fallback() {
    let catalog = catalog(&[]).expect("legacy codex fallback");
    assert_eq!(catalog.adapters().len(), 4);
    assert!(catalog.find("codex").is_some());
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
    assert!(catalog(CLAUDE_LIST).is_ok());
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

#[derive(Clone, Copy)]
enum Existing {
    Absent,
    Empty,
    User,
    Model,
    Fable,
}

struct MatrixCase {
    name: &'static str,
    existing: Existing,
    models: &'static [&'static str],
    override_dir: Option<Override>,
    jsonc: bool,
}

#[test]
fn compiled_matches_const_adapters_across_the_edit_lifecycle() {
    let cases = [
        MatrixCase {
            name: "absent-no-routes",
            existing: Existing::Absent,
            models: &[],
            override_dir: None,
            jsonc: false,
        },
        MatrixCase {
            name: "empty-single-route",
            existing: Existing::Empty,
            models: &["route-a"],
            override_dir: None,
            jsonc: false,
        },
        MatrixCase {
            name: "user-keys-multiple-routes",
            existing: Existing::User,
            models: &["route-a", "route-b"],
            override_dir: None,
            jsonc: false,
        },
        MatrixCase {
            name: "existing-model",
            existing: Existing::Model,
            models: &["route-a"],
            override_dir: None,
            jsonc: false,
        },
        MatrixCase {
            name: "fable-model",
            existing: Existing::Fable,
            models: &["route-a", "route-b"],
            override_dir: None,
            jsonc: false,
        },
        MatrixCase {
            name: "relative-env-override",
            existing: Existing::Absent,
            models: &["route-a"],
            override_dir: Some(Override::Relative("relative-root")),
            jsonc: false,
        },
        MatrixCase {
            name: "override-user-keys",
            existing: Existing::User,
            models: &[],
            override_dir: Some(Override::UnderHome("another-root")),
            jsonc: false,
        },
        MatrixCase {
            name: "jsonc-present",
            existing: Existing::User,
            models: &["route-a", "route-a", "route-b"],
            override_dir: None,
            jsonc: true,
        },
    ];

    for description in DESCRIPTIONS {
        let const_adapter = const_adapter(description.id().as_str());
        let compiled = Compiled::new(description).with_os(TargetOs::Linux);
        for case in &cases {
            let context = format!("{} / {}", description.id().as_str(), case.name);
            let const_home = scratch();
            let compiled_home = scratch();
            prepare(&const_home, description, case);
            prepare(&compiled_home, description, case);
            let before = tree(&const_home);
            assert_eq!(before, tree(&compiled_home), "{context}: initial files");
            let const_session = session_for(&const_home, description, case.override_dir);
            let compiled_session = session_for(&compiled_home, description, case.override_dir);
            let target = target_with(case.models);

            assert_eq!(
                model_result(const_adapter, &const_session, &const_home),
                model_result(&compiled, &compiled_session, &compiled_home),
                "{context}: initial configured model"
            );
            assert_status(
                const_adapter,
                &const_session,
                &compiled,
                &compiled_session,
                &target,
                &const_home,
                &compiled_home,
                &context,
            );
            assert_plan(
                &const_adapter
                    .plan(&const_session, &target)
                    .expect("static plan"),
                &compiled
                    .plan(&compiled_session, &target)
                    .expect("compiled plan"),
                &const_home,
                &compiled_home,
                &context,
            );
            let const_apply = const_adapter
                .apply(&const_session, &target)
                .expect("static apply");
            let compiled_apply = compiled
                .apply(&compiled_session, &target)
                .expect("compiled apply");
            assert_plan(
                &const_apply,
                &compiled_apply,
                &const_home,
                &compiled_home,
                &context,
            );
            let applied = tree(&const_home);
            let compiled_applied = tree(&compiled_home);
            assert_eq!(applied, compiled_applied, "{context}: applied files");
            assert!(
                compiled_applied
                    .keys()
                    .any(|path| { path.as_os_str().to_string_lossy().contains(".hofund.") }),
                "{context}: markers missing from comparison"
            );
            assert_override_placement(case.override_dir, &before, &applied, &context);

            let const_force = scratch();
            let compiled_force = scratch();
            copy_tree(&const_home, &const_force);
            copy_tree(&compiled_home, &compiled_force);

            let rotated = target.clone().with_token("rotated-token");
            let const_updated = const_adapter
                .refresh_token(&const_session, &rotated)
                .expect("static refresh");
            let compiled_updated = compiled
                .refresh_token(&compiled_session, &rotated)
                .expect("compiled refresh");
            assert_eq!(
                relative_paths(&const_home, &const_updated),
                relative_paths(&compiled_home, &compiled_updated),
                "{context}: refreshed paths"
            );
            assert_eq!(
                tree(&const_home),
                tree(&compiled_home),
                "{context}: refresh"
            );
            edit_token(&const_home, &const_updated);
            edit_token(&compiled_home, &compiled_updated);
            assert_eq!(
                tree(&const_home),
                tree(&compiled_home),
                "{context}: user edit"
            );

            let const_outcome = const_adapter
                .disconnect(&const_session, false)
                .expect("static disconnect");
            let compiled_outcome = compiled
                .disconnect(&compiled_session, false)
                .expect("compiled disconnect");
            assert_eq!(
                const_outcome.changed(),
                compiled_outcome.changed(),
                "{context}: disconnect changed"
            );
            assert_eq!(
                normalized_warnings(&const_home, const_outcome.warnings()),
                normalized_warnings(&compiled_home, compiled_outcome.warnings()),
                "{context}: disconnect warnings"
            );
            assert_eq!(
                tree(&const_home),
                tree(&compiled_home),
                "{context}: non-forced disconnect"
            );
            assert_status(
                const_adapter,
                &const_session,
                &compiled,
                &compiled_session,
                &rotated,
                &const_home,
                &compiled_home,
                &context,
            );
            assert_eq!(
                model_result(const_adapter, &const_session, &const_home),
                model_result(&compiled, &compiled_session, &compiled_home),
                "{context}: final configured model"
            );

            let const_force_session = session_for(&const_force, description, case.override_dir);
            let compiled_force_session =
                session_for(&compiled_force, description, case.override_dir);
            let const_forced = const_adapter
                .disconnect(&const_force_session, true)
                .expect("static forced disconnect");
            let compiled_forced = compiled
                .disconnect(&compiled_force_session, true)
                .expect("compiled forced disconnect");
            assert_eq!(const_forced, compiled_forced, "{context}: forced outcome");
            assert_eq!(tree(&const_force), before, "{context}: forced restore");
            assert_eq!(
                tree(&compiled_force),
                before,
                "{context}: compiled forced restore"
            );
        }
    }
}

fn model_result(
    adapter: &dyn AgentAdapter,
    session: &Session,
    home: &Path,
) -> Result<Option<String>, String> {
    adapter.configured_model(session).map_err(|error| {
        error
            .to_string()
            .replace(&home.to_string_lossy().to_string(), "<home>")
    })
}

fn prepare(home: &Path, description: &AgentDescription, case: &MatrixCase) {
    let root = override_root(home, description.id().as_str(), case.override_dir);
    match description.id().as_str() {
        "claude-code" => {
            let path = root.join("settings.json");
            match case.existing {
                Existing::Absent => {}
                Existing::Empty => write(&path, ""),
                Existing::User => write(&path, r#"{"ui":{"theme":"dark"}}"#),
                Existing::Model => write(&path, r#"{"model":"user-model"}"#),
                Existing::Fable => write(&path, r#"{"model":"claude-fable-5"}"#),
            }
        }
        "codex" => {
            let path = root.join("config.toml");
            match case.existing {
                Existing::Absent => {}
                Existing::Empty => write(&path, ""),
                Existing::User => write(&path, "# keep\ntemperature = 0.2\n"),
                Existing::Model => write(&path, "model = \"user-model\"\n"),
                Existing::Fable => write(&path, "model = \"claude-fable-5\"\n"),
            }
        }
        "gemini-cli" => {
            let settings = root.join(".gemini").join("settings.json");
            let env = root.join(".gemini").join(".env");
            match case.existing {
                Existing::Absent => {}
                Existing::Empty => {
                    write(&settings, "");
                    write(&env, "");
                }
                Existing::User => {
                    write(&settings, r#"{"ui":{"theme":"dark"}}"#);
                    write(&env, "KEEP=1\n");
                }
                Existing::Model => {
                    write(&settings, r#"{"model":"user-model"}"#);
                    write(&env, "KEEP=1\n");
                }
                Existing::Fable => {
                    write(&settings, r#"{"model":"claude-fable-5"}"#);
                    write(&env, "KEEP=1\n");
                }
            }
        }
        "opencode" => {
            let name = if case.jsonc {
                "opencode.jsonc"
            } else {
                "opencode.json"
            };
            let path = root.join("opencode").join(name);
            match case.existing {
                Existing::Absent => {}
                Existing::Empty => write(&path, ""),
                Existing::User => write(&path, r#"{"plugin":["a"]}"#),
                Existing::Model => write(&path, r#"{"model":"user-model"}"#),
                Existing::Fable => write(&path, r#"{"model":"claude-fable-5"}"#),
            }
        }
        id => panic!("unknown description {id}"),
    }
}

/// Environment override used by a lifecycle matrix case.
#[derive(Clone, Copy)]
enum Override {
    /// A relative value, which is unusable and must fall back to the default.
    Relative(&'static str),
    /// An absolute directory under the test home.
    UnderHome(&'static str),
}

impl Override {
    fn path(self, home: &Path) -> PathBuf {
        match self {
            Self::Relative(path) => PathBuf::from(path),
            Self::UnderHome(path) => home.join(path),
        }
    }
}

fn override_root(home: &Path, id: &str, override_dir: Option<Override>) -> PathBuf {
    if let Some(Override::UnderHome(path)) = override_dir {
        return home.join(path);
    }
    match id {
        "claude-code" => home.join(".claude"),
        "codex" => home.join(".codex"),
        "gemini-cli" => home.to_path_buf(),
        "opencode" => home.join(".config"),
        _ => panic!("unknown description {id}"),
    }
}

fn session_for(
    home: &Path,
    description: &AgentDescription,
    override_dir: Option<Override>,
) -> Session {
    let session = Session::new(home);
    match (
        description.id().as_str(),
        override_dir.map(|dir| dir.path(home)),
    ) {
        ("claude-code", Some(path)) => session.with_claude_config_dir(path),
        ("codex", Some(path)) => session.with_codex_home(path),
        ("gemini-cli", Some(path)) => session.with_gemini_cli_home(path),
        ("opencode", Some(path)) => session.with_xdg_config_home(path),
        _ => session,
    }
}

/// Checks that apply edits the prepared user files under an absolute
/// override root and never writes under a relative override value.
fn assert_override_placement(
    override_dir: Option<Override>,
    before: &BTreeMap<PathBuf, Vec<u8>>,
    applied: &BTreeMap<PathBuf, Vec<u8>>,
    context: &str,
) {
    let changed: Vec<&PathBuf> = applied
        .iter()
        .filter(|(path, contents)| before.get(*path) != Some(*contents))
        .map(|(path, _)| path)
        .collect();
    assert!(!changed.is_empty(), "{context}: apply changed nothing");
    match override_dir {
        Some(Override::UnderHome(dir)) => {
            assert!(
                !before.is_empty() && before.keys().all(|path| path.starts_with(dir)),
                "{context}: user files are not planted under the override root"
            );
            for path in &changed {
                assert!(
                    path.starts_with(dir),
                    "{context}: {} written outside the override root",
                    path.display()
                );
            }
            assert!(
                changed.iter().any(|path| before.contains_key(*path)),
                "{context}: no user file under the override root was edited"
            );
        }
        Some(Override::Relative(dir)) => {
            for path in &changed {
                assert!(
                    !path.starts_with(dir),
                    "{context}: {} written under a relative override",
                    path.display()
                );
            }
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn assert_status(
    const_adapter: &dyn AgentAdapter,
    const_session: &Session,
    compiled: &Compiled,
    compiled_session: &Session,
    target: &GatewayTarget,
    const_home: &Path,
    compiled_home: &Path,
    context: &str,
) {
    let const_status = const_adapter
        .status(const_session, target)
        .expect("static status");
    let compiled_status = compiled
        .status(compiled_session, target)
        .expect("compiled status");
    assert_eq!(
        const_status.id(),
        compiled_status.id(),
        "{context}: status id"
    );
    assert_eq!(
        const_status.display_name(),
        compiled_status.display_name(),
        "{context}: status name"
    );
    assert_eq!(
        const_status.installed(),
        compiled_status.installed(),
        "{context}: installed"
    );
    assert_eq!(
        const_status.connected(),
        compiled_status.connected(),
        "{context}: connected"
    );
    assert_eq!(
        relative_paths(const_home, const_status.paths()),
        relative_paths(compiled_home, compiled_status.paths()),
        "{context}: status paths"
    );
}

fn assert_plan(
    const_plan: &hofund_agent::Plan,
    compiled_plan: &hofund_agent::Plan,
    const_home: &Path,
    compiled_home: &Path,
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
        normalize(const_plan, const_home),
        normalize(compiled_plan, compiled_home),
        "{context}: plan bytes"
    );
}

fn edit_token(home: &Path, updated: &[PathBuf]) {
    for path in updated {
        let bytes = fs::read(path).expect("token file");
        let text = String::from_utf8(bytes).expect("token UTF-8");
        let relative = path.strip_prefix(home).expect("relative").to_path_buf();
        write(
            &home.join(relative),
            &text.replace("rotated-token", "user-token"),
        );
    }
}

fn normalized_warnings(home: &Path, warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .map(|warning| warning.replace(&home.to_string_lossy().to_string(), "<home>"))
        .collect()
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
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
            files.insert(
                path.strip_prefix(root).expect("relative").to_path_buf(),
                fs::read(&path).expect("file"),
            );
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    for (relative, bytes) in tree(source) {
        let path = destination.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, bytes).expect("copy");
    }
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

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hofund-compiled-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).expect("dir");
    path
}
