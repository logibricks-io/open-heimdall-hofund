//! Adapters that point a coding agent at the local hofund gateway.
//!
//! Each adapter is a declarative description: which files it owns, and which
//! keys those files must contain. [`AgentAdapter::plan`] previews the edit.
//! [`AgentAdapter::apply`] backs up each original file once, then replaces it
//! by writing a temporary file and renaming it. [`AgentAdapter::disconnect`]
//! puts the connect-time bytes back when the file is still exactly what apply
//! wrote. Otherwise each key apply touched is restored to its previous value,
//! unless the user changed it, in which case that value stays and a warning is
//! returned. [`AgentAdapter::revert`] puts the original bytes back. Unrelated
//! keys and comments in JSONC, YAML, and TOML stay in place on apply.
//!
//! This crate depends on `config` and `hofund-plugin-api`. It does not read
//! the token file and it does not start the gateway. The daemon resolves the
//! listen address and the bearer token and passes them in as a [`GatewayTarget`].
//!
//! The four catalog adapters, and the documents they were checked against:
//!
//! - Claude Code: `~/.claude/settings.json` (`CLAUDE_CONFIG_DIR` replaces that
//!   directory). `env.ANTHROPIC_BASE_URL` is the gateway origin and
//!   `env.ANTHROPIC_AUTH_TOKEN` is the bearer token. Opus, Sonnet, and Haiku
//!   tier env vars follow the selected model.
//!   <https://code.claude.com/docs/en/settings>,
//!   <https://code.claude.com/docs/en/llm-gateway-connect>,
//!   <https://code.claude.com/docs/en/env-vars>.
//! - Codex CLI: `~/.codex/config.toml` (`CODEX_HOME` replaces `~/.codex`).
//!   Provider id `hofund`, `wire_api = "responses"`, `base_url` ending in
//!   `/v1`, and `experimental_bearer_token`.
//!   <https://developers.openai.com/codex/config-reference>.
//! - Gemini CLI: `~/.gemini/settings.json` and `~/.gemini/.env`
//!   (`GEMINI_CLI_HOME` is the parent of `.gemini`). The model is `model.name`.
//!   <https://geminicli.com/docs/reference/configuration/>,
//!   <https://geminicli.com/docs/get-started/authentication/>,
//!   and the auth-type source in
//!   <https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/core/contentGenerator.ts>.
//! - OpenCode: `~/.config/opencode/opencode.json` (or `opencode.jsonc` when
//!   that file already exists), `provider.hofund` with npm
//!   `@ai-sdk/openai-compatible`. `models` lists the gateway routes.
//!   `XDG_CONFIG_HOME` replaces `~/.config` when it is set.
//!   <https://opencode.ai/docs/config/>,
//!   <https://opencode.ai/docs/providers/>.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_config as _;

pub use hofund_plugin_api::{
    AgentFactory, AgentIntegration, ConnectionStatus, Credential, CredentialError, CredentialStore,
    DisconnectOutcome, DynAgentFactory, FileChange, GatewayTarget, Plan,
};

mod active_provider;
mod catalog;
mod credential;
mod edit;
mod go_binary;
mod jsonc_edit;
mod request_agent;
mod store;
mod yaml_edit;

pub use catalog::{Catalog, Compiled, catalog, resolve};
pub use credential::CREDENTIAL_CHANGE_PREFIX;
pub use request_agent::request_agent_id;

/// The operating system path template selected by a compiled adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs {
    /// Linux and other Unix-like systems.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
}

impl TargetOs {
    /// The operating system running this process.
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            return Self::Macos;
        }
        #[cfg(windows)]
        {
            return Self::Windows;
        }
        #[cfg(all(not(target_os = "macos"), not(windows)))]
        {
            Self::Linux
        }
    }
}

/// The home directory and optional path overrides an adapter reads.
///
/// Tests build a session with [`Session::new`] and never touch the real home
/// directory. [`Session::from_process_env`] is what the CLI uses.
#[derive(Clone)]
pub struct Session {
    home: PathBuf,
    path: Option<OsString>,
    env: BTreeMap<OsString, OsString>,
    env_case_insensitive: bool,
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    gemini_cli_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    credential_store: Option<Arc<dyn CredentialStore>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("home", &self.home)
            .field("path", &self.path)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("claude_config_dir", &self.claude_config_dir)
            .field("codex_home", &self.codex_home)
            .field("gemini_cli_home", &self.gemini_cli_home)
            .field("xdg_config_home", &self.xdg_config_home)
            .field("credential_store", &self.credential_store.is_some())
            .finish()
    }
}

impl Session {
    /// A session rooted at `home`, with no process environment.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            path: None,
            env: BTreeMap::new(),
            env_case_insensitive: cfg!(windows),
            claude_config_dir: None,
            codex_home: None,
            gemini_cli_home: None,
            xdg_config_home: None,
            credential_store: None,
        }
    }

    /// Captures the process environment and reads the home directory and `PATH`.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self::from_env_iter(home_dir(), std::env::vars_os())
    }

    /// Builds a session from `home` and environment pairs without reading the
    /// process environment. Empty and relative path overrides remain in the raw
    /// environment map but are treated as unset by adapters.
    #[must_use]
    pub fn from_env_iter<I, K, V>(home: impl Into<PathBuf>, variables: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut session = Self::new(home);
        for (name, value) in variables {
            session.set_env(name.as_ref(), value.as_ref());
        }
        session
    }

    /// Returns an environment value captured in this session.
    #[must_use]
    pub fn env(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        let name = name.as_ref();
        if self.env_case_insensitive {
            self.env
                .iter()
                .find(|(candidate, _)| env_name_eq(candidate, name, true))
                .map(|(_, value)| value.as_os_str())
        } else {
            self.env.get(name).map(OsString::as_os_str)
        }
    }

    /// Sets an environment value in this session. `with_env("PATH", ..)` sets
    /// the path used for binary detection.
    #[must_use]
    pub fn with_env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.set_env(name.as_ref(), value.as_ref());
        self
    }

    #[cfg(test)]
    fn with_env_case_insensitive(mut self, enabled: bool) -> Self {
        self.env_case_insensitive = enabled;
        self
    }

    fn set_env(&mut self, name: &OsStr, value: &OsStr) {
        if self.env_case_insensitive
            && let Some(existing) = self
                .env
                .keys()
                .find(|candidate| env_name_eq(candidate, name, true))
                .cloned()
        {
            self.env.remove(&existing);
        }
        self.env.insert(name.to_os_string(), value.to_os_string());
        let path = usable_override(value).then(|| PathBuf::from(value));
        if env_name_eq(name, OsStr::new("PATH"), self.env_case_insensitive) {
            self.path = Some(value.to_os_string());
        } else if env_name_eq(
            name,
            OsStr::new("CLAUDE_CONFIG_DIR"),
            self.env_case_insensitive,
        ) {
            self.claude_config_dir = path;
        } else if env_name_eq(name, OsStr::new("CODEX_HOME"), self.env_case_insensitive) {
            self.codex_home = path;
        } else if env_name_eq(
            name,
            OsStr::new("GEMINI_CLI_HOME"),
            self.env_case_insensitive,
        ) {
            self.gemini_cli_home = path;
        } else if env_name_eq(
            name,
            OsStr::new("XDG_CONFIG_HOME"),
            self.env_case_insensitive,
        ) {
            self.xdg_config_home = path;
        }
    }

    /// Sets the `PATH` used to detect an installed binary.
    #[must_use]
    pub fn with_path(mut self, path: impl AsRef<OsStr>) -> Self {
        self.path = Some(path.as_ref().to_os_string());
        self
    }

    /// Sets `CLAUDE_CONFIG_DIR`. Empty and relative paths are treated as unset.
    #[must_use]
    pub fn with_claude_config_dir(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.set_env(OsStr::new("CLAUDE_CONFIG_DIR"), path.as_os_str());
        self
    }

    /// Sets `CODEX_HOME`. Empty and relative paths are treated as unset.
    #[must_use]
    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.set_env(OsStr::new("CODEX_HOME"), path.as_os_str());
        self
    }

    /// Sets `GEMINI_CLI_HOME`. Empty and relative paths are treated as unset.
    #[must_use]
    pub fn with_gemini_cli_home(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.set_env(OsStr::new("GEMINI_CLI_HOME"), path.as_os_str());
        self
    }

    /// Sets `XDG_CONFIG_HOME`. Empty and relative paths are treated as unset.
    ///
    /// OpenCode reads `$XDG_CONFIG_HOME/opencode` when this is set. Other
    /// agents ignore it.
    #[must_use]
    pub fn with_xdg_config_home(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.set_env(OsStr::new("XDG_CONFIG_HOME"), path.as_os_str());
        self
    }

    /// Uses `store` for agents that read hofund's key from the OS credential
    /// store. Without one, connecting such an agent fails.
    #[must_use]
    pub fn with_credential_store(mut self, store: Arc<dyn CredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }

    pub(crate) fn credential_store(&self) -> Option<&Arc<dyn CredentialStore>> {
        self.credential_store.as_ref()
    }
}

fn env_name_eq(a: &OsStr, b: &OsStr, case_insensitive: bool) -> bool {
    if case_insensitive {
        a.as_encoded_bytes()
            .eq_ignore_ascii_case(b.as_encoded_bytes())
    } else {
        a == b
    }
}

pub(crate) fn usable_override(value: &OsStr) -> bool {
    !value.is_empty()
        && (Path::new(value).is_absolute()
            || (cfg!(windows) && matches!(value.as_encoded_bytes().first(), Some(b'\\' | b'/'))))
}

/// One managed file as it exists before an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    /// Absolute or session-relative path of the file.
    pub path: PathBuf,
    /// File bytes, or `None` when the file is absent.
    pub bytes: Option<Vec<u8>>,
}

/// Failure while reading, planning, applying, or reverting an agent file.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A readable explanation, including the path when one is known.
    #[error("{0}")]
    Message(String),
    /// The request does not fit this agent, such as a model it cannot use.
    /// Retrying the same request cannot succeed.
    #[error("{0}")]
    Rejected(String),
}

impl AgentError {
    pub(crate) fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    pub(crate) fn rejected(text: impl Into<String>) -> Self {
        Self::Rejected(text.into())
    }
}

/// One coding agent's config files.
///
/// Implementations are declarative descriptions, not a pile of closures.
pub trait AgentAdapter: Sync {
    /// Stable id used by `hofund agent connect <name>`.
    fn id(&self) -> &str;

    /// Human-readable name.
    fn display_name(&self) -> &str;

    /// Whether the binary is on `PATH` or a managed config file exists.
    fn detect(&self, session: &Session) -> bool;

    /// Reads the current managed files without changing them.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a file exists but cannot be read.
    fn read(&self, session: &Session) -> Result<Vec<ConfigFile>, AgentError>;

    /// Computes the edit. Writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a file cannot be read or parsed.
    fn plan(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError>;

    /// Applies [`Self::plan`].
    ///
    /// The original bytes are copied to a sibling `*.hofund.bak` the first
    /// time. A file that did not exist gets an empty `*.hofund.absent` marker
    /// instead. A later apply does not overwrite that backup. The new bytes
    /// are written to a temporary sibling and renamed into place.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when planning or writing fails. A parse error
    /// happens before any backup is created.
    fn apply(&self, session: &Session, target: &GatewayTarget) -> Result<Plan, AgentError>;

    /// Restores every managed file that has a backup or an absent marker.
    ///
    /// This replaces the current file with the connect-time bytes. Prefer
    /// [`Self::disconnect`] when the user may have edited the file since.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when nothing was backed up, or a write fails.
    fn revert(&self, session: &Session) -> Result<(), AgentError>;

    /// Restores each file apply changed.
    ///
    /// When the file is still byte-for-byte what apply wrote, the connect-time
    /// backup is written back. Otherwise each key apply touched is compared
    /// with that snapshot: an unchanged key returns to its pre-connect value,
    /// or is removed when it did not exist, and a key the user edited is kept.
    /// Keys the user added are kept. `force` writes the backup bytes and
    /// discards later edits. The backup is deleted only after the restored
    /// bytes are read back and match. When no backup exists the call returns
    /// [`DisconnectOutcome`] with `changed: false`. Goose instead follows its
    /// clear-active-provider convention: the active and legacy provider/model
    /// keys and hofund's providers entry are removed, other providers kept.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a managed file cannot be parsed. The backup
    /// is left in place so a later `force` call can still restore it.
    fn disconnect(&self, session: &Session, force: bool) -> Result<DisconnectOutcome, AgentError>;

    /// Writes the token slots in `target` without taking a new backup.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a present file cannot be parsed or written.
    fn refresh_token(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<Vec<PathBuf>, AgentError>;

    /// Points the agent's model setting at `model`, a gateway route model.
    ///
    /// An agent that is not connected is connected first, as [`Self::apply`]
    /// does, so the model is reachable. `None` clears the route pick: a
    /// connected agent restores its pre-connect model when declared, otherwise
    /// it gets what a plain connect picks (the sole route model, when there is
    /// exactly one). A disconnected one only loses the key. A later
    /// [`Self::disconnect`] restores the pre-connect model.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the agent has no model setting, `model` is
    /// not a route model it can use, or a file cannot be parsed or written.
    fn set_model(
        &self,
        session: &Session,
        target: &GatewayTarget,
        model: Option<&str>,
    ) -> Result<Plan, AgentError>;

    /// The model string the tool stored, when that key exists and is a string.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the file exists but cannot be parsed.
    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError>;

    /// Prefix the agent uses for gateway route names, if any.
    fn model_prefix(&self) -> Option<&str> {
        None
    }

    /// Reports install and connection state for `target`.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a present file cannot be read or parsed.
    fn status(
        &self,
        session: &Session,
        target: &GatewayTarget,
    ) -> Result<ConnectionStatus, AgentError>;
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let found = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let found = std::env::var_os("HOME");
    found.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::Path;

    use hofund_plugin_api::{AgentFile, FileKind};

    use super::Session;
    use crate::{TargetOs, resolve};

    #[test]
    fn from_env_iter_ignores_empty_and_relative_path_overrides() {
        let names = [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GEMINI_CLI_HOME",
            "XDG_CONFIG_HOME",
        ];
        for value in ["", "relative"] {
            let session = Session::from_env_iter("home", names.map(|name| (name, value)));
            assert_eq!(session.claude_config_dir, None);
            assert_eq!(session.codex_home, None);
            assert_eq!(session.gemini_cli_home, None);
            assert_eq!(session.xdg_config_home, None);
            for name in names {
                assert_eq!(session.env(name), Some(OsStr::new(value)));
            }
        }
    }

    #[test]
    fn debug_shows_environment_names_without_values() {
        let session = Session::new("home")
            .with_env("SECRET_NAME", "do-not-print-this")
            .with_codex_home("relative");
        let debug = format!("{session:?}");
        assert!(debug.contains("SECRET_NAME"), "{debug}");
        assert!(debug.contains("codex_home"), "{debug}");
        assert!(!debug.contains("do-not-print-this"), "{debug}");
    }

    #[test]
    fn windows_environment_names_ignore_ascii_case() {
        let home = scratch("names");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).expect("binary directory");
        let file_name = if cfg!(windows) { "codex.exe" } else { "codex" };
        fs::write(bin.join(file_name), "").expect("fake binary");
        let override_dir = home.join("codex-override");
        let session = Session::new(&*home)
            .with_env_case_insensitive(true)
            .with_env("Path", bin.as_os_str())
            .with_env("Codex_Home", override_dir.as_os_str());
        assert_mixed_case_environment_override(&session, &bin, &override_dir);

        let sensitive = Session::new(&*home)
            .with_env_case_insensitive(false)
            .with_env("Path", bin.as_os_str())
            .with_env("Codex_Home", override_dir.as_os_str());
        assert_eq!(sensitive.path, None);
        assert_eq!(sensitive.codex_home, None);
        assert_eq!(sensitive.env("PATH"), None);
        assert_eq!(sensitive.env("CODEX_HOME"), None);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn windows_environment_writes_replace_case_variants() {
        let session = Session::new("home")
            .with_env_case_insensitive(true)
            .with_env("Path", "first")
            .with_env("PATH", "second");
        assert_eq!(session.env.len(), 1);
        assert_eq!(session.env("path"), Some(OsStr::new("second")));
        assert_eq!(session.path, Some(OsString::from("second")));
        assert_eq!(session.env.keys().next(), Some(&OsString::from("PATH")));
    }

    #[test]
    fn compiled_resolve_honors_windows_mixed_case_env_override() {
        static FILE: AgentFile = AgentFile::from_static(
            "config",
            FileKind::Toml,
            "~/.fixture/config.toml",
            "~/.fixture/config.toml",
            "~/.fixture/config.toml",
            Some("FIXTURE_TOML_HOME"),
        );

        let home = scratch("resolve");
        let override_dir = home.join("compiled-override");
        let session = Session::new(&*home)
            .with_env_case_insensitive(true)
            .with_env("Fixture_Toml_Home", override_dir.as_os_str());
        assert_eq!(
            session.env("FIXTURE_TOML_HOME"),
            Some(override_dir.as_os_str())
        );
        assert_eq!(
            resolve(&FILE, TargetOs::Linux, &session),
            override_dir.join("config.toml")
        );

        let sensitive = Session::new(&*home)
            .with_env_case_insensitive(false)
            .with_env("Fixture_Toml_Home", override_dir.as_os_str());
        assert_eq!(sensitive.env("FIXTURE_TOML_HOME"), None);
        assert_eq!(
            resolve(&FILE, TargetOs::Linux, &sensitive),
            home.join(".fixture").join("config.toml")
        );
        let _ = fs::remove_dir_all(home);
    }

    #[cfg(windows)]
    #[test]
    fn public_from_env_iter_uses_windows_environment_casing() {
        let home = scratch("public-windows");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).expect("binary directory");
        fs::write(bin.join("codex.exe"), "").expect("fake binary");
        let override_dir = home.join("codex-override");
        let session = Session::from_env_iter(
            &*home,
            [
                (OsStr::new("Path"), bin.as_os_str()),
                (OsStr::new("Codex_Home"), override_dir.as_os_str()),
            ],
        );
        assert_mixed_case_environment_override(&session, &bin, &override_dir);
        let _ = fs::remove_dir_all(home);
    }

    fn assert_mixed_case_environment_override(session: &Session, bin: &Path, override_dir: &Path) {
        assert_eq!(session.path, Some(bin.as_os_str().to_os_string()));
        assert_eq!(session.codex_home, Some(override_dir.to_path_buf()));
        assert_eq!(session.env("PATH"), Some(bin.as_os_str()));
        assert_eq!(session.env("CODEX_HOME"), Some(override_dir.as_os_str()));
    }

    struct Scratch(tempfile::TempDir);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            self.0.path()
        }
    }

    fn scratch(_name: &str) -> Scratch {
        Scratch(tempfile::tempdir().expect("session test directory"))
    }
}
