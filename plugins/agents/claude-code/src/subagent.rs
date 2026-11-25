//! Read `env.CLAUDE_CODE_SUBAGENT_MODEL` for Explore and Plan turns.
//!
//! Those built-in agents declare `model: inherit`, so the variable never
//! reaches them. The gateway moves their tool turns onto the model named
//! here when this file is routed through the gateway and the value names a
//! gateway model. The declarative integration fills an unset value when one
//! gateway route exists. This module does not write
//! `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`.

use std::path::{Path, PathBuf};

use hofund_plugin_sdk::json::{self, Value};

/// Agent id whose settings this reader understands.
const AGENT_ID: &str = "claude-code";

/// Openings of the built-in agents that declare `model: inherit`.
const OPENINGS: &[&str] = &[
    "You are a file search specialist for Claude Code",
    "You are a software architect and planning specialist for Claude Code",
];

/// Words the client uses for its own pick. They are not gateway model ids.
const CLIENT_WORDS: &[&str] = &["inherit", "opus", "sonnet", "haiku", "fable", "default"];

/// Effort labels that may follow `:` in the setting.
const EFFORTS: &[&str] = &[
    "none", "off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Largest settings file this reader will load.
const SETTINGS_LIMIT: u64 = 1_048_576;

/// A subagent model and the effort written beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentSetting {
    /// Model id, without the effort suffix.
    pub model: String,
    /// Effort label when the value was `<model>:<effort>`.
    pub effort: Option<String>,
}

/// The model set for an inheriting tool turn, read from `settings_dir`.
///
/// Returns nothing unless `agent_id` is this agent, `body` is an Explore or
/// Plan tool turn, `env.ANTHROPIC_BASE_URL` is `gateway_origin`, and
/// `env.CLAUDE_CODE_SUBAGENT_MODEL` names a gateway model. A value with `/`
/// is returned even when `is_route` is false, so the gateway can keep the
/// asked model and say the target is unserved. A missing file is nothing.
#[must_use]
pub fn inheriting_subagent_model(
    agent_id: &str,
    body: &[u8],
    settings_dir: &Path,
    gateway_origin: &str,
    is_route: impl Fn(&str) -> bool,
) -> Option<SubagentSetting> {
    if !agent_id.eq_ignore_ascii_case(AGENT_ID) || !inheriting_tool_turn(body) {
        return None;
    }
    setting_in_directory(settings_dir, gateway_origin, is_route)
}

/// [`inheriting_subagent_model`] using `$CLAUDE_CONFIG_DIR` or `~/.claude`.
///
/// An empty override is ignored. Callers that have a directory already should
/// pass it to [`inheriting_subagent_model`] so tests never read a real home.
#[must_use]
pub fn inheriting_subagent_from_env(
    agent_id: &str,
    body: &[u8],
    gateway_origin: &str,
    is_route: impl Fn(&str) -> bool,
) -> Option<SubagentSetting> {
    let dir = settings_directory()?;
    inheriting_subagent_model(agent_id, body, &dir, gateway_origin, is_route)
}

/// The model in `settings_dir/settings.json` when that file is routed through
/// `gateway_origin` and names a gateway model.
#[must_use]
fn setting_in_directory(
    settings_dir: &Path,
    gateway_origin: &str,
    is_route: impl Fn(&str) -> bool,
) -> Option<SubagentSetting> {
    let bytes = read_limited(&settings_dir.join("settings.json"))?;
    let value: Value = json::from_slice(&bytes).ok()?;
    let env = value.get("env")?.as_object()?;
    let origin = env.get("ANTHROPIC_BASE_URL")?.as_str()?;
    if !same_origin(origin, gateway_origin) {
        return None;
    }
    let raw = env.get("CLAUDE_CODE_SUBAGENT_MODEL")?.as_str()?;
    interpret(raw, &is_route)
}

fn settings_directory() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".claude"))
}

fn read_limited(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > SETTINGS_LIMIT {
        return None;
    }
    std::fs::read(path).ok()
}

fn same_origin(settings: &str, gateway: &str) -> bool {
    trim_origin(settings) == trim_origin(gateway) && !trim_origin(gateway).is_empty()
}

fn trim_origin(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

fn interpret(raw: &str, is_route: &impl Fn(&str) -> bool) -> Option<SubagentSetting> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if is_route(raw) {
        return Some(SubagentSetting {
            model: raw.to_owned(),
            effort: None,
        });
    }
    let (model, effort) = split_effort(raw);
    if model.is_empty() {
        return None;
    }
    if is_route(&model) {
        return Some(SubagentSetting { model, effort });
    }
    if model.contains('/') && !is_client_word(&model) {
        return Some(SubagentSetting { model, effort });
    }
    None
}

fn split_effort(raw: &str) -> (String, Option<String>) {
    let Some((model, level)) = raw.rsplit_once(':') else {
        return (raw.to_owned(), None);
    };
    if model.is_empty() || !is_effort(level) {
        return (raw.to_owned(), None);
    }
    let level = if level.eq_ignore_ascii_case("off") {
        "none".to_owned()
    } else {
        level.to_ascii_lowercase()
    };
    (model.to_owned(), Some(level))
}

fn is_effort(level: &str) -> bool {
    EFFORTS
        .iter()
        .any(|effort| effort.eq_ignore_ascii_case(level))
}

fn is_client_word(model: &str) -> bool {
    CLIENT_WORDS
        .iter()
        .any(|word| word.eq_ignore_ascii_case(model))
}

fn inheriting_tool_turn(body: &[u8]) -> bool {
    let Ok(value) = json::from_slice::<Value>(body) else {
        return false;
    };
    let tools = value.get("tools").and_then(Value::as_array);
    if tools.is_none_or(Vec::is_empty) {
        return false;
    }
    system_texts(value.get("system"))
        .iter()
        .any(|text| OPENINGS.iter().any(|opening| text.contains(opening)))
}

fn system_texts(system: Option<&Value>) -> Vec<&str> {
    let Some(system) = system else {
        return Vec::new();
    };
    if let Some(text) = system.as_str() {
        return vec![text];
    }
    let Some(blocks) = system.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            block
                .as_str()
                .or_else(|| block.get("text").and_then(Value::as_str))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json;

    use super::{OPENINGS, inheriting_subagent_model, inheriting_tool_turn, setting_in_directory};

    const ORIGIN: &str = "http://127.0.0.1:3425";

    fn explore() -> Vec<u8> {
        body(OPENINGS[0], true)
    }

    fn plan() -> Vec<u8> {
        body(OPENINGS[1], true)
    }

    fn body(opening: &str, tools: bool) -> Vec<u8> {
        let mut value = json::json!({
            "model": "lead",
            "max_tokens": 8,
            "system": [
                {"type": "text", "text": "billing"},
                {"type": "text", "text": opening}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        if tools {
            value["tools"] = json::json!([
                {"name": "read", "input_schema": {"type": "object"}}
            ]);
        }
        json::to_vec(&value).expect("body")
    }

    fn write_settings(dir: &std::path::Path, model: &str, origin: &str) {
        let path = dir.join("settings.json");
        let text = format!(
            r#"{{"env":{{"ANTHROPIC_BASE_URL":"{origin}","CLAUDE_CODE_SUBAGENT_MODEL":"{model}"}}}}"#
        );
        std::fs::write(path, text).expect("settings");
    }

    fn served(model: &str) -> bool {
        model == "lead" || model == "sub"
    }

    #[test]
    fn d267_reads_the_set_model() {
        let dir = tempfile::tempdir().expect("dir");
        write_settings(dir.path(), "sub", ORIGIN);
        let got = inheriting_subagent_model("claude-code", &explore(), dir.path(), ORIGIN, served)
            .expect("setting");
        assert_eq!(got.model, "sub");
        assert_eq!(got.effort, None);
        assert!(
            inheriting_subagent_model("claude-desktop", &explore(), dir.path(), ORIGIN, served)
                .is_none()
        );
        write_settings(dir.path(), "inherit", ORIGIN);
        assert!(setting_in_directory(dir.path(), ORIGIN, served).is_none());
        write_settings(dir.path(), "sub", "https://api.example.com");
        assert!(setting_in_directory(dir.path(), ORIGIN, served).is_none());
        write_settings(dir.path(), "sub", &format!("{ORIGIN}/"));
        let got = setting_in_directory(dir.path(), ORIGIN, served).expect("slash");
        assert_eq!(got.model, "sub");
        std::fs::write(dir.path().join("settings.json"), b"{}").expect("clear");
        assert!(setting_in_directory(dir.path(), ORIGIN, served).is_none());
    }

    #[test]
    fn d267_effort_suffix_and_unserved_id() {
        let dir = tempfile::tempdir().expect("dir");
        write_settings(dir.path(), "sub:high", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, served).expect("effort");
        assert_eq!(got.model, "sub");
        assert_eq!(got.effort.as_deref(), Some("high"));
        write_settings(dir.path(), "sub:off", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, served).expect("off");
        assert_eq!(got.effort.as_deref(), Some("none"));
        write_settings(dir.path(), "sub:high", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, |model| model == "sub:high")
            .expect("whole id");
        assert_eq!(got.model, "sub:high");
        assert_eq!(got.effort, None);
        write_settings(dir.path(), "route/missing:low", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, |_| false).expect("unserved");
        assert_eq!(got.model, "route/missing");
        assert_eq!(got.effort.as_deref(), Some("low"));
        write_settings(dir.path(), "missing", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, |model| model == "missing")
            .expect("configured but unserved");
        assert_eq!(got.model, "missing");
        write_settings(dir.path(), "not-a-route", ORIGIN);
        assert!(setting_in_directory(dir.path(), ORIGIN, |_| false).is_none());
    }

    #[test]
    fn d267_explore_and_plan_match_and_others_do_not() {
        let dir = tempfile::tempdir().expect("dir");
        write_settings(dir.path(), "sub", ORIGIN);
        assert!(inheriting_tool_turn(&explore()));
        assert!(inheriting_tool_turn(&plan()));
        assert!(
            inheriting_subagent_model("claude-code", &plan(), dir.path(), ORIGIN, served).is_some()
        );
        let general = body(
            "You are an agent for Claude Code, Anthropic's official CLI for Claude.",
            true,
        );
        let custom = body("You are my own test agent. Say done.", true);
        let lead = body(OPENINGS[0], false);
        assert!(!inheriting_tool_turn(&general));
        assert!(!inheriting_tool_turn(&custom));
        assert!(!inheriting_tool_turn(&lead));
        assert!(
            inheriting_subagent_model("claude-code", &general, dir.path(), ORIGIN, served)
                .is_none()
        );
        assert!(
            inheriting_subagent_model("claude-code", &custom, dir.path(), ORIGIN, served).is_none()
        );
        let missing = dir.path().join("absent");
        assert!(
            inheriting_subagent_model("claude-code", &general, &missing, ORIGIN, served).is_none()
        );
    }
}
