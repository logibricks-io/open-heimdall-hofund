//! Full agent-tree checkpoint rollback (main file and connect sidecars).

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use hofund_agent::{
    AgentAdapter, Compiled, Credential, CredentialError, CredentialStore, GatewayTarget, Session,
    SidecarCheckpoint, catalog_from, reseat_after_provider_change,
};
use hofund_plugin_api::{
    AgentCredential, AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId,
    SecretString, Slot, Visible,
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

static KEYED_FILES: &[AgentFile] = &[AgentFile::from_static(
    "settings",
    FileKind::Json,
    "~/.keyed/settings.json",
    "~/.keyed/settings.json",
    "~/.keyed/settings.json",
    None,
)];
static KEYED_SETS: &[AgentSet] = &[
    AgentSet::from_static("settings", "models.hofund.api_url", Slot::V1),
    AgentSet::from_static("settings", "model", Slot::model_if_unset(None)),
];
const CREDENTIAL_SPEC: AgentCredential =
    AgentCredential::from_static("settings", "{v1}", "Bearer", "keyed-app", "app:url={v1}");
static KEYED: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("keyed-model"),
    "Keyed model",
    "keyed-model",
    KEYED_FILES,
    KEYED_SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("settings")
.with_credential(CREDENTIAL_SPEC);

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

static COMPILED: &[&AgentDescription] = &[&KEYED, &CLAUDE, &CODEX];

const GATEWAY_TOKEN: &str = "secret-bearer-token-value";
const CREDENTIAL_URL: &str = "http://127.0.0.1:3425/v1";

#[derive(Default)]
struct FakeStore {
    items: Mutex<BTreeMap<String, (Credential, String)>>,
    writes: AtomicUsize,
    broken: AtomicBool,
}

impl FakeStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (Credential, String)>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn secret(&self, url: &str) -> Option<String> {
        self.lock().get(url).map(|(_, secret)| secret.clone())
    }

    fn check(&self) -> Result<(), CredentialError> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(CredentialError::new("store locked"));
        }
        Ok(())
    }
}

impl CredentialStore for FakeStore {
    fn read(&self, item: &Credential) -> Result<Option<SecretString>, CredentialError> {
        self.check()?;
        Ok(self
            .lock()
            .get(item.url())
            .map(|(_, secret)| SecretString::from(secret.clone())))
    }

    fn write(&self, item: &Credential, secret: &str) -> Result<(), CredentialError> {
        self.check()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.lock()
            .insert(item.url().to_owned(), (item.clone(), secret.to_owned()));
        Ok(())
    }

    fn delete(&self, item: &Credential) -> Result<(), CredentialError> {
        self.check()?;
        self.lock().remove(item.url());
        Ok(())
    }
}

fn sidecar_path(main: &Path, suffix: &str) -> PathBuf {
    let mut name = main.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn read_optional_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentTreeSnapshot {
    main: Option<Vec<u8>>,
    bak: Option<Vec<u8>>,
    absent: Option<Vec<u8>>,
    wrote: Option<Vec<u8>>,
    credential: Option<Vec<u8>>,
}

impl AgentTreeSnapshot {
    fn capture(main: &Path) -> Self {
        Self {
            main: read_optional_bytes(main),
            bak: read_optional_bytes(&sidecar_path(main, ".hofund.bak")),
            absent: read_optional_bytes(&sidecar_path(main, ".hofund.absent")),
            wrote: read_optional_bytes(&sidecar_path(main, ".hofund.wrote")),
            credential: read_optional_bytes(&sidecar_path(main, ".hofund.credential")),
        }
    }
}

fn connect_agents(
    session: &Session,
    target: &GatewayTarget,
    keyed: &Path,
    claude: &Path,
    codex: &Path,
) {
    Compiled::new(&KEYED)
        .apply(session, target)
        .expect("connect keyed");
    assert!(keyed.exists());
    Compiled::new(&CLAUDE)
        .apply(session, target)
        .expect("connect claude");
    assert!(claude.exists());
    Compiled::new(&CODEX)
        .apply(session, target)
        .expect("connect codex");
    assert!(codex.exists());
}

fn two_agent_session(
    dir: &tempfile::TempDir,
    store: Arc<FakeStore>,
) -> (Session, PathBuf, PathBuf, PathBuf, Arc<FakeStore>) {
    let keyed = dir.path().join("home").join(".keyed").join("settings.json");
    fs::create_dir_all(keyed.parent().expect("keyed parent")).expect("keyed dir");
    fs::write(&keyed, r#"{"model":"on-spare"}"#).expect("keyed settings");
    let claude = dir
        .path()
        .join("home")
        .join(".claude")
        .join("settings.json");
    fs::create_dir_all(claude.parent().expect("claude parent")).expect("claude dir");
    fs::write(&claude, r#"{"model":"on-spare"}"#).expect("claude settings");
    let codex = dir.path().join("home").join(".codex").join("config.toml");
    fs::create_dir_all(codex.parent().expect("codex parent")).expect("codex dir");
    fs::write(
        &codex,
        "model = \"on-spare\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let session = Session::new(dir.path().join("home")).with_credential_store(store.clone());
    (session, keyed, claude, codex, store)
}

fn gateway_target() -> GatewayTarget {
    GatewayTarget::new(
        "http://127.0.0.1:3425/",
        GATEWAY_TOKEN,
        vec!["normal".to_owned()],
    )
}

#[test]
fn r7a_reseat_restores_full_agent_tree_when_second_write_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(FakeStore::default());
    let (session, keyed, claude, codex, store) = two_agent_session(&dir, store);
    let target = gateway_target();
    connect_agents(&session, &target, &keyed, &claude, &codex);

    let before_keyed = AgentTreeSnapshot::capture(&keyed);
    let before_claude = AgentTreeSnapshot::capture(&claude);
    let store_before = store.secret(CREDENTIAL_URL);

    fs::set_permissions(&codex, fs::Permissions::from_mode(0o000)).expect("codex inaccessible");

    let catalog = catalog_from(COMPILED).expect("catalog");
    let mut checkpoint = SidecarCheckpoint::new();
    reseat_after_provider_change(
        &catalog,
        &session,
        &target,
        "spare",
        false,
        &["on-spare".to_owned()],
        &["normal".to_owned()],
        |_agent: &str| vec!["normal".to_owned()],
        &mut checkpoint,
    )
    .expect_err("codex write must fail");

    checkpoint.restore().expect("restore checkpoint");

    assert_eq!(AgentTreeSnapshot::capture(&keyed), before_keyed);
    assert_eq!(AgentTreeSnapshot::capture(&claude), before_claude);
    assert_eq!(store.secret(CREDENTIAL_URL), store_before);
}

#[test]
fn r7a_rename_group_refs_restores_full_agent_tree_when_second_write_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(FakeStore::default());
    let (session, keyed, claude, codex, store) = two_agent_session(&dir, store);
    fs::write(&keyed, r#"{"model":"group/old"}"#).expect("keyed group model");
    fs::write(&claude, r#"{"model":"group/old"}"#).expect("claude group model");
    fs::write(
        &codex,
        "model = \"group/old\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex group model");

    let target = gateway_target();
    connect_agents(&session, &target, &keyed, &claude, &codex);

    let before_keyed = AgentTreeSnapshot::capture(&keyed);
    let before_claude = AgentTreeSnapshot::capture(&claude);
    let store_before = store.secret(CREDENTIAL_URL);

    fs::set_permissions(&codex, fs::Permissions::from_mode(0o000)).expect("codex inaccessible");

    let catalog = catalog_from(COMPILED).expect("catalog");
    let mut checkpoint = SidecarCheckpoint::new();
    catalog
        .rename_group_refs(&session, "old", "new", &mut checkpoint)
        .expect_err("codex rename must fail");

    checkpoint.restore().expect("restore checkpoint");

    assert_eq!(AgentTreeSnapshot::capture(&keyed), before_keyed);
    assert_eq!(AgentTreeSnapshot::capture(&claude), before_claude);
    assert_eq!(store.secret(CREDENTIAL_URL), store_before);
}
