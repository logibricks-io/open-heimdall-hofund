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
//! keys are left in place on apply. TOML comments stay.
//! JSON comments do not survive an apply; revert restores them.
//!
//! This crate depends only on `config`. It does not read the token file and
//! it does not start the gateway. The daemon resolves the listen address and
//! the bearer token and passes them in as a [`GatewayTarget`].
//!
//! The four adapters, and the documents they were checked against:
//!
//! - Claude Code: `~/.claude/settings.json` (`CLAUDE_CONFIG_DIR` replaces that
//!   directory). `env.ANTHROPIC_BASE_URL` is the gateway origin and
//!   `env.ANTHROPIC_AUTH_TOKEN` is the bearer token.
//!   <https://code.claude.com/docs/en/settings>,
//!   <https://code.claude.com/docs/en/llm-gateway-connect>,
//!   <https://code.claude.com/docs/en/env-vars>.
//! - Codex CLI: `~/.codex/config.toml` (`CODEX_HOME` replaces `~/.codex`).
//!   Provider id `hofund`, `wire_api = "responses"`, `base_url` ending in
//!   `/v1`, and `experimental_bearer_token`.
//!   <https://developers.openai.com/codex/config-reference>.
//! - Gemini CLI: `~/.gemini/settings.json` and `~/.gemini/.env`
//!   (`GEMINI_CLI_HOME` is the parent of `.gemini`).
//!   <https://geminicli.com/docs/reference/configuration/>,
//!   <https://geminicli.com/docs/get-started/authentication/>,
//!   and the auth-type source in
//!   <https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/core/contentGenerator.ts>.
//! - OpenCode: `~/.config/opencode/opencode.json` (or `opencode.jsonc` when
//!   that file already exists), using the v2 `providers` object.
//!   `XDG_CONFIG_HOME` replaces `~/.config` when it is set.
//!   <https://opencode.ai/v2/docs/config/>,
//!   <https://opencode.ai/v2/docs/providers/>.

use std::ffi::OsString;
use std::path::PathBuf;

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_config as _;

mod catalog;
mod edit;
mod store;

pub use catalog::{adapters, find};

/// What [`AgentAdapter::disconnect`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisconnectOutcome {
    /// A backed-up file was merged or replaced.
    pub changed: bool,
    /// Keys the user changed after connect. Those values were left in place.
    pub warnings: Vec<String>,
}

/// Where the local gateway is listening, and the bearer token clients send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayTarget {
    /// Origin such as `http://127.0.0.1:3425`. A trailing slash is ignored.
    pub base_url: String,
    /// Bearer token. Adapters write it into the agent file that tool documents.
    pub token: String,
}

/// The home directory and optional path overrides an adapter reads.
///
/// Tests build a session with [`Session::new`] and never touch the real home
/// directory. [`Session::from_process_env`] is what the CLI uses.
#[derive(Clone, Debug)]
pub struct Session {
    home: PathBuf,
    path: Option<OsString>,
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    gemini_cli_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl Session {
    /// A session rooted at `home`, with no process environment.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            path: None,
            claude_config_dir: None,
            codex_home: None,
            gemini_cli_home: None,
            xdg_config_home: None,
        }
    }

    /// Reads `HOME` or `USERPROFILE`, `PATH`, `CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`, `GEMINI_CLI_HOME`, and `XDG_CONFIG_HOME`.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            home: home_dir(),
            path: std::env::var_os("PATH"),
            claude_config_dir: std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            codex_home: std::env::var_os("CODEX_HOME").map(PathBuf::from),
            gemini_cli_home: std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Sets the `PATH` used to detect an installed binary.
    #[must_use]
    pub fn with_path(mut self, path: impl AsRef<std::ffi::OsStr>) -> Self {
        self.path = Some(path.as_ref().to_os_string());
        self
    }

    /// Sets `CLAUDE_CONFIG_DIR`. A relative path is joined to the home directory.
    #[must_use]
    pub fn with_claude_config_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_config_dir = Some(path.into());
        self
    }

    /// Sets `CODEX_HOME`. A relative path is joined to the home directory.
    #[must_use]
    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(path.into());
        self
    }

    /// Sets `GEMINI_CLI_HOME`. A relative path is joined to the home directory.
    #[must_use]
    pub fn with_gemini_cli_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.gemini_cli_home = Some(path.into());
        self
    }

    /// Sets `XDG_CONFIG_HOME`. A relative path is joined to the home directory.
    ///
    /// OpenCode reads `$XDG_CONFIG_HOME/opencode` when this is set. Other
    /// agents ignore it.
    #[must_use]
    pub fn with_xdg_config_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.xdg_config_home = Some(path.into());
        self
    }
}

/// One managed file as it exists before an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    /// Absolute or session-relative path of the file.
    pub path: PathBuf,
    /// File bytes, or `None` when the file is absent.
    pub bytes: Option<Vec<u8>>,
}

/// A planned edit that has not been written unless [`AgentAdapter::apply`] ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Files whose bytes would change. Already-matching files are omitted.
    pub changes: Vec<FileChange>,
}

/// One file in a [`Plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path that would be replaced.
    pub path: PathBuf,
    /// Bytes before the edit. Empty when the file does not exist yet.
    pub before: Vec<u8>,
    /// Bytes after the edit.
    pub after: Vec<u8>,
    /// A line diff of `before` and `after`. Empty when they are identical.
    pub diff: String,
}

/// Whether an agent is installed and whether its files already match a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionStatus {
    /// Stable id, such as `claude-code`.
    pub id: &'static str,
    /// Name printed by the CLI.
    pub display_name: &'static str,
    /// The binary is on `PATH`, or a managed config file already exists.
    pub installed: bool,
    /// Every declared key equals the target. A missing file is not connected.
    pub connected: bool,
    /// Config paths this adapter would read or write.
    pub paths: Vec<PathBuf>,
}

/// Failure while reading, planning, applying, or reverting an agent file.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A readable explanation, including the path when one is known.
    #[error("{0}")]
    Message(String),
}

impl AgentError {
    pub(crate) fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
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
    /// [`DisconnectOutcome`] with `changed: false`.
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

    /// The model string the tool stored, when that key exists and is a string.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the file exists but cannot be parsed.
    fn configured_model(&self, session: &Session) -> Result<Option<String>, AgentError>;

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
