//! Persistent agent sign-in switching state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static TEST_AGENT_HOMES: RefCell<Vec<BTreeMap<String, PathBuf>>> = const { RefCell::new(Vec::new()) };
}

use serde::{Deserialize, Serialize};

use super::logic::LoginPolicy;
use crate::DaemonError;

/// Overrides [`agent_home`] for the current test until dropped.
#[cfg(test)]
pub(crate) struct TestHomeGuard;

#[cfg(test)]
pub(crate) fn push_test_agent_homes(map: BTreeMap<String, PathBuf>) -> TestHomeGuard {
    TEST_AGENT_HOMES.with(|stack| stack.borrow_mut().push(map));
    TestHomeGuard
}

#[cfg(test)]
impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        TEST_AGENT_HOMES.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

#[cfg(feature = "test-util")]
static TEST_UTIL_AGENT_HOMES: std::sync::Mutex<BTreeMap<String, PathBuf>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Overrides [`agent_home`] for integration tests. The last path for a given
/// environment name wins.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub fn set_test_agent_home(home_env: impl Into<String>, path: PathBuf) {
    TEST_UTIL_AGENT_HOMES
        .lock()
        .expect("test agent homes")
        .insert(home_env.into(), path);
}

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

/// Builds policy from subscription config and the plugin's sign-in flag.
#[must_use]
pub fn policy_from_config(
    routing: &str,
    options: &BTreeMap<String, hofund_config::Value>,
    sign_in_spends: bool,
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
        sign_in_spends,
    }
}

/// Resolves the agent home from a plugin declaration.
///
/// # Errors
///
/// Returns [`DaemonError`] when no home directory can be resolved.
pub fn agent_home(decl: &hofund_plugin_api::AgentSignIn) -> Result<PathBuf, DaemonError> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_HOMES.with(|stack| {
        stack
            .borrow()
            .last()
            .and_then(|top| top.get(decl.home_env()).cloned())
    }) {
        return Ok(path);
    }
    #[cfg(feature = "test-util")]
    if let Some(path) = TEST_UTIL_AGENT_HOMES
        .lock()
        .expect("test agent homes")
        .get(decl.home_env())
        .cloned()
    {
        return Ok(path);
    }
    home_from(decl, |key| std::env::var(key).ok())
}

/// [`agent_home`] over a variable lookup: the agent's own variable, then
/// `HOME`, then `USERPROFILE` (Windows), each only when non-empty.
fn home_from(
    decl: &hofund_plugin_api::AgentSignIn,
    var: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, DaemonError> {
    let set = |key: &str| var(key).filter(|value| !value.is_empty());
    if let Some(value) = set(decl.home_env()) {
        return Ok(PathBuf::from(value));
    }
    let home = set("HOME").or_else(|| set("USERPROFILE")).ok_or_else(|| {
        DaemonError::new(format!(
            "neither HOME nor USERPROFILE is set; set {} to the agent directory",
            decl.home_env()
        ))
    })?;
    Ok(PathBuf::from(home).join(decl.home_dir()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECL: hofund_plugin_api::AgentSignIn =
        hofund_plugin_api::AgentSignIn::new(&["demo"], "DEMO_HOME", ".demo");

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_agent_variable_wins_when_set() {
        let home = home_from(&DECL, lookup(&[("DEMO_HOME", "/agent"), ("HOME", "/h")]));
        assert_eq!(home.expect("home"), PathBuf::from("/agent"));
    }

    #[test]
    fn home_is_used_when_the_agent_variable_is_unset_or_empty() {
        for pairs in [
            &[("HOME", "/h"), ("USERPROFILE", "C:/u")][..],
            &[("DEMO_HOME", ""), ("HOME", "/h"), ("USERPROFILE", "C:/u")][..],
        ] {
            let home = home_from(&DECL, lookup(pairs));
            assert_eq!(home.expect("home"), PathBuf::from("/h").join(".demo"));
        }
    }

    /// #612: Windows sets USERPROFILE, usually not HOME.
    #[test]
    fn userprofile_is_the_fallback_when_home_is_unset_or_empty() {
        for pairs in [
            &[("USERPROFILE", "C:/u")][..],
            &[("HOME", ""), ("USERPROFILE", "C:/u")][..],
        ] {
            let home = home_from(&DECL, lookup(pairs));
            assert_eq!(home.expect("home"), PathBuf::from("C:/u").join(".demo"));
        }
    }

    #[test]
    fn no_home_at_all_names_the_agent_variable() {
        for pairs in [&[][..], &[("HOME", ""), ("USERPROFILE", "")][..]] {
            let error = home_from(&DECL, lookup(pairs)).expect_err("no home");
            assert!(error.to_string().contains("DEMO_HOME"), "{error}");
        }
    }
}
