//! Persistent agent sign-in switching state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::logic::LoginPolicy;
use crate::DaemonError;

/// One agent's remembered sign-in switching state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentState {
    /// Account key the agent is signed in to, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active: String,
    /// Account the agent moved off and may return to under In order routing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub return_to: String,
    /// User-chosen names by account key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
}

/// Every agent row in the state file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileState {
    #[serde(default)]
    agents: BTreeMap<String, AgentState>,
}

/// Path to the state file beside other daemon data.
pub fn state_path(root: &Path) -> PathBuf {
    root.join("agent-logins.json")
}

/// Reads state, or the default when the file is missing.
///
/// # Errors
///
/// Returns [`DaemonError`] when the file exists but cannot be read or parsed.
pub fn read(root: &Path) -> Result<BTreeMap<String, AgentState>, DaemonError> {
    let path = state_path(root);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| DaemonError::new(format!("cannot read {}: {error}", path.display())))?;
    let parsed: FileState = serde_json::from_str(&text).map_err(|error| {
        DaemonError::new(format!(
            "cannot parse agent sign-in state {}: {error}",
            path.display()
        ))
    })?;
    Ok(parsed.agents)
}

/// Writes state atomically.
///
/// # Errors
///
/// Returns [`DaemonError`] when the file cannot be written.
pub fn write(root: &Path, agents: &BTreeMap<String, AgentState>) -> Result<(), DaemonError> {
    let path = state_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            DaemonError::new(format!(
                "cannot create {} for agent sign-in state: {error}",
                parent.display()
            ))
        })?;
    }
    let file = FileState {
        agents: agents.clone(),
    };
    let text = serde_json::to_string_pretty(&file)
        .map_err(|error| DaemonError::new(format!("cannot encode agent sign-in state: {error}")))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|error| {
        DaemonError::new(format!(
            "cannot write agent sign-in state {}: {error}",
            tmp.display()
        ))
    })?;
    fs::rename(&tmp, &path).map_err(|error| {
        DaemonError::new(format!(
            "cannot replace agent sign-in state {}: {error}",
            path.display()
        ))
    })
}

/// Builds policy from subscription config.
#[must_use]
pub fn policy_from_config(
    routing: &str,
    options: &BTreeMap<String, hofund_config::Value>,
) -> LoginPolicy {
    let bool_opt = |key: &str| {
        options
            .get(key)
            .is_some_and(|value| matches!(value, hofund_config::Value::Boolean(true)))
    };
    LoginPolicy {
        routing: routing.to_owned(),
        keep_login: bool_opt("keep_login"),
        switch_only: bool_opt("switch_only"),
        sign_in_spends: bool_opt("sign_in_spends"),
    }
}

/// Maps a CLI agent name to a subscription id.
#[must_use]
pub fn subscription_for_agent(agent: &str) -> Option<&'static str> {
    match agent.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "cc" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// Agent home directory from the environment.
///
/// # Errors
///
/// Returns [`DaemonError`] when no home directory can be resolved.
pub fn agent_home(subscription: &str) -> Result<PathBuf, DaemonError> {
    match subscription {
        "claude" => std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|_| home_join(".claude")),
        "codex" => std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|_| home_join(".codex")),
        _ => Err(DaemonError::new(format!(
            "subscription `{subscription}` has no agent sign-in to switch"
        ))),
    }
}

fn home_join(suffix: &str) -> Result<PathBuf, DaemonError> {
    let home = std::env::var("HOME")
        .map_err(|_| DaemonError::new("HOME is not set; pass CLAUDE_CONFIG_DIR or CODEX_HOME"))?;
    Ok(PathBuf::from(home).join(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d27_9_subscription_for_agent_maps_cli_names() {
        assert_eq!(subscription_for_agent("claude-code"), Some("claude"));
        assert_eq!(subscription_for_agent("codex"), Some("codex"));
        assert!(subscription_for_agent("gemini").is_none());
    }
}
