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
/// `env.CLAUDE_CODE_SUBAGENT_MODEL` names a gateway model. A value with `/`,
/// and any other id that is not one of the client's own words, is returned
/// even when `is_route` is false, so the gateway can resolve a wire alias or
/// say the target is unserved. Comments and trailing commas are accepted. A
/// missing file is nothing.
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
/// Empty and relative overrides are ignored. Callers that have a directory already should
/// pass it to [`inheriting_subagent_model`] so tests never read a real home.
#[must_use]
pub fn inheriting_subagent_from_env(
    agent_id: &str,
    body: &[u8],
    gateway_origin: &str,
    is_route: impl Fn(&str) -> bool,
) -> Option<SubagentSetting> {
    let dir = settings_directory();
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
    let value = parse_settings(&bytes)?;
    let env = value.get("env")?.as_object()?;
    let origin = env.get("ANTHROPIC_BASE_URL")?.as_str()?;
    if !same_origin(origin, gateway_origin) {
        return None;
    }
    let raw = env.get("CLAUDE_CODE_SUBAGENT_MODEL")?.as_str()?;
    interpret(raw, &is_route)
}

fn settings_directory() -> PathBuf {
    settings_directory_from(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        &hofund_plugin_api::home_dir(),
    )
}

fn settings_directory_from(dir: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    if let Some(dir) = dir
        && hofund_plugin_api::usable_override(&dir)
    {
        return PathBuf::from(dir);
    }
    home.join(".claude")
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
    if is_route(&model) || !is_client_word(&model) {
        return Some(SubagentSetting { model, effort });
    }
    None
}

/// Parses settings JSON, including `//` comments, block comments, and trailing
/// commas outside strings. The settings editor accepts those forms.
fn parse_settings(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    json::from_str(&loosen_json(text)).ok()
}

fn loosen_json(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(char) = chars.next() {
        if in_string {
            out.push(char);
            if escape {
                escape = false;
            } else if char == '\\' {
                escape = true;
            } else if char == '"' {
                in_string = false;
            }
            continue;
        }
        match char {
            '"' => {
                in_string = true;
                out.push(char);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for next in chars.by_ref() {
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                }
                _ => out.push(char),
            },
            ',' => {
                let rest: String = chars.clone().collect();
                let trimmed = rest.trim_start();
                if !trimmed.starts_with('}') && !trimmed.starts_with(']') {
                    out.push(char);
                }
            }
            other => out.push(other),
        }
    }
    out
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

    use super::{
        OPENINGS, inheriting_subagent_model, inheriting_tool_turn, setting_in_directory,
        settings_directory_from,
    };

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
        let got = setting_in_directory(dir.path(), ORIGIN, |_| false).expect("bare id");
        assert_eq!(got.model, "not-a-route");
        assert_eq!(got.effort, None);
    }

    #[test]
    fn d267_jsonc_comments_and_trailing_commas_are_read() {
        let dir = tempfile::tempdir().expect("dir");
        let text = format!(
            r#"{{
  // kept by the settings editor
  "env": {{
    "ANTHROPIC_BASE_URL": "{ORIGIN}",
    "note": "keep // and /* inside */",
    "CLAUDE_CODE_SUBAGENT_MODEL": "sub",
  }},
}}"#
        );
        std::fs::write(dir.path().join("settings.json"), text).expect("settings");
        let got = setting_in_directory(dir.path(), ORIGIN, served).expect("jsonc");
        assert_eq!(got.model, "sub");
        assert_eq!(got.effort, None);
    }

    #[test]
    fn d267_bare_wire_alias_is_returned_for_resolve() {
        let dir = tempfile::tempdir().expect("dir");
        write_settings(dir.path(), "wire-alias", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, |_| false).expect("alias");
        assert_eq!(got.model, "wire-alias");
        assert_eq!(got.effort, None);
        write_settings(dir.path(), "wire-alias:high", ORIGIN);
        let got = setting_in_directory(dir.path(), ORIGIN, |_| false).expect("alias effort");
        assert_eq!(got.model, "wire-alias");
        assert_eq!(got.effort.as_deref(), Some("high"));
        write_settings(dir.path(), "inherit", ORIGIN);
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

    #[test]
    fn d267_relative_override_and_windows_home_match_connect() {
        let home = tempfile::tempdir().expect("home");
        let settings = home.path().join(".claude");
        std::fs::create_dir(&settings).expect("settings dir");
        write_settings(&settings, "sub", ORIGIN);
        let dir = settings_directory_from(Some(std::ffi::OsString::from("relative")), home.path());
        let got = inheriting_subagent_model("claude-code", &explore(), &dir, ORIGIN, served)
            .expect("relative override reads the home file");
        assert_eq!(got.model, "sub");

        let profile = tempfile::tempdir().expect("profile");
        let windows_home = hofund_plugin_api::home_dir_from_env(
            Some(home.path().as_os_str().to_os_string()),
            Some(profile.path().as_os_str().to_os_string()),
            true,
        );
        assert_eq!(windows_home, profile.path());
        let settings = windows_home.join(".claude");
        std::fs::create_dir(&settings).expect("Windows settings dir");
        write_settings(&settings, "sub", ORIGIN);
        let dir =
            settings_directory_from(Some(std::ffi::OsString::from("relative")), &windows_home);
        let got = inheriting_subagent_model("claude-code", &explore(), &dir, ORIGIN, served)
            .expect("Windows order reads the profile file");
        assert_eq!(got.model, "sub");
    }
}
