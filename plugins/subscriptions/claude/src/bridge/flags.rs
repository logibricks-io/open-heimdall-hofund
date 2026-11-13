//! Effort, thinking, classifier-low, and CLI flag mapping (R-6f, §1.5 C–F).
//!
//! Gateway stages A–B write `hofund_bridge_effort`, `hofund_think_off`, and
//! optionally `hofund_auto_mode_classifier`. This module reads those
//! extensions, applies stage D on a resume miss, and builds argv flags.

use hofund_plugin_api::{ContentPart, Request, Role};

/// Trusted extension keys written by the gateway (stages A–B).
pub(crate) const BRIDGE_EFFORT: &str = "hofund_bridge_effort";
pub(crate) const THINK_OFF: &str = "hofund_think_off";
#[allow(dead_code)]
pub(crate) const AUTO_MODE_CLASSIFIER: &str = "hofund_auto_mode_classifier";

/// Snapshot of effort / think-off used for hashing and argv.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EffortFlags {
    /// Normalized effort string (`low` / `medium` / …); empty when unset.
    pub effort: String,
    /// Client asked for thinking off.
    pub think_off: bool,
}

impl EffortFlags {
    /// `true` when an effort level is set (the turnKey effort bit).
    #[must_use]
    pub(crate) fn bit(&self) -> bool {
        !self.effort.is_empty()
    }
}

/// Reads stages A+B (or later D) from `request` extensions.
///
/// Does not infer from raw `params.effort` alone.
#[must_use]
pub(crate) fn from_request(request: &Request) -> EffortFlags {
    EffortFlags {
        effort: extension_string(request, BRIDGE_EFFORT),
        think_off: extension_bool(request, THINK_OFF),
    }
}

/// Stage D: on a resume miss, a classifier with no effort is asked at `low`.
///
/// All inbound protocols. Does not run on a resume hit.
pub(crate) fn apply_stage_d(request: &mut Request) {
    let flags = from_request(request);
    if flags.effort.is_empty() && is_classifier(request) {
        request
            .extensions
            .insert(BRIDGE_EFFORT, serde_json::Value::String("low".into()));
    }
}

/// Claude Code's auto-mode classifier asking about an action.
///
/// True when the request has no tools, the system/developer text contains
/// `<block>` or `<severity>`, and some user text (trimmed) starts with
/// `<transcript>`. Prefers a trusted `hofund_auto_mode_classifier` boolean
/// when the gateway already computed it, but still requires the predicate
/// on the IR so a frozen-true extension after tools were added cannot stick.
#[must_use]
pub(crate) fn is_classifier(request: &Request) -> bool {
    auto_mode_classifier(request)
}

/// Reference `autoModeClassifier` on IR messages.
#[must_use]
pub(crate) fn auto_mode_classifier(request: &Request) -> bool {
    if !request.tools.is_empty() {
        return false;
    }
    let system = system_text(request);
    if !system.contains("<block>") && !system.contains("<severity>") {
        return false;
    }
    request.messages.iter().any(|message| {
        message.role == Role::User
            && message.content.iter().any(|part| match part {
                ContentPart::Text(text) => text.text.trim_start().starts_with("<transcript>"),
                _ => false,
            })
    })
}

/// `--effort` / `--thinking-display` / `--thinking disabled` / `--json-schema`.
#[must_use]
pub(crate) fn cli_flag_args(request: &Request) -> Vec<String> {
    let flags = from_request(request);
    let mut args = Vec::new();
    if !flags.effort.is_empty() {
        args.extend([
            "--effort".into(),
            flags.effort.clone(),
            "--thinking-display".into(),
            "summarized".into(),
        ]);
    }
    if flags.think_off {
        args.extend(["--thinking".into(), "disabled".into()]);
    }
    if let Some(schema) = json_schema(request) {
        args.push("--json-schema".into());
        args.push(cli_schema(&schema));
    }
    args
}

/// Drops a root `$schema` so Claude Code's Ajv (draft-07) will accept it.
#[must_use]
pub(crate) fn cli_schema(raw: &str) -> String {
    let Ok(serde_json::Value::Object(mut root)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        return raw.to_owned();
    };
    if root.remove("$schema").is_none() {
        return raw.to_owned();
    }
    serde_json::to_string(&root).unwrap_or_else(|_| raw.to_owned())
}

/// A `control_request` line that tells an idle Claude Code the next effort.
#[must_use]
pub(crate) fn set_effort_line(effort: &str, request_id: &str) -> String {
    let line = serde_json::json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "apply_flag_settings",
            "settings": { "effortLevel": effort }
        }
    });
    format!("{line}\n")
}

/// Builds `effort-<12 hex chars>` for a setEffort control request.
#[must_use]
pub(crate) fn set_effort_request_id() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 6];
    let _ = SystemRandom::new().fill(&mut bytes);
    format!("effort-{}", hex_lower(&bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn json_schema(request: &Request) -> Option<String> {
    if let Some(value) = request.extensions.get("json_schema") {
        return Some(value.to_string());
    }
    let format = request
        .extensions
        .get("output_config")
        .and_then(serde_json::Value::as_object)
        .and_then(|config| config.get("format"))
        .and_then(serde_json::Value::as_object)?;
    let kind = format.get("type").and_then(serde_json::Value::as_str)?;
    if kind != "json_schema" {
        return None;
    }
    format
        .get("schema")
        .map(ToString::to_string)
        .filter(|s| s != "null")
}

fn system_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .filter(|message| matches!(message.role, Role::System | Role::Developer))
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extension_string(request: &Request, key: &str) -> String {
    match request.extensions.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.as_str().unwrap_or("").to_owned(),
        None => String::new(),
    }
}

fn extension_bool(request: &Request, key: &str) -> bool {
    request
        .extensions
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// `effortOf`: protocol literals to a Claude Code `--effort` level.
#[must_use]
#[allow(dead_code)]
pub(crate) fn effort_of(label: &str) -> String {
    match label.trim().to_ascii_lowercase().as_str() {
        "minimal" | "none" => "low".into(),
        "low" | "medium" | "high" | "xhigh" | "max" => label.trim().to_ascii_lowercase(),
        "ultra" => "max".into(),
        _ => String::new(),
    }
}

/// `effortOfBudget` for Anthropic `thinking.budget_tokens`.
#[must_use]
#[allow(dead_code)]
pub(crate) fn effort_of_budget(n: i64) -> String {
    if n <= 0 {
        String::new()
    } else if n <= 4096 {
        "low".into()
    } else if n <= 12000 {
        "medium".into()
    } else if n <= 24000 {
        "high".into()
    } else {
        "xhigh".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::Message;
    use serde_json::json;

    fn req() -> Request {
        Request::new("claude")
    }

    fn with_system_user(system: &str, user: &str) -> Request {
        let mut request = req();
        request.messages = vec![
            Message {
                role: Role::System,
                content: vec![ContentPart::text(system)],
                extensions: Default::default(),
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::text(user)],
                extensions: Default::default(),
            },
        ];
        request
    }

    #[test]
    fn effort_of_folds_none_and_minimal_to_low() {
        assert_eq!(effort_of("none"), "low");
        assert_eq!(effort_of("minimal"), "low");
        assert_eq!(effort_of("high"), "high");
        assert_eq!(effort_of("ultra"), "max");
        assert_eq!(effort_of("nope"), "");
    }

    #[test]
    fn effort_of_budget_buckets() {
        assert_eq!(effort_of_budget(0), "");
        assert_eq!(effort_of_budget(4096), "low");
        assert_eq!(effort_of_budget(4097), "medium");
        assert_eq!(effort_of_budget(24000), "high");
        assert_eq!(effort_of_budget(24001), "xhigh");
    }

    #[test]
    fn turn_key_bit_is_whether_bridge_effort_is_set_not_params() {
        let mut request = req();
        // params.effort is ignored: only hofund_bridge_effort sets the bit.
        assert!(
            !from_request(&request).bit(),
            "unset extensions must not set the bit"
        );
        request.extensions.insert(BRIDGE_EFFORT, json!("high"));
        assert!(from_request(&request).bit());
    }

    #[test]
    fn classifier_needs_system_marker_transcript_and_no_tools() {
        let request = with_system_user(
            "answer <block>yes</block> or <block>no</block>",
            "<transcript>\nls\n</transcript>",
        );
        assert!(auto_mode_classifier(&request));

        let mut with_tools = request.clone();
        with_tools.tools =
            serde_json::from_value(json!([{"name":"bash","input_schema":{"type":"object"}}]))
                .unwrap();
        assert!(!auto_mode_classifier(&with_tools));

        let no_marker = with_system_user("just a system", "<transcript>\nls\n</transcript>");
        assert!(!auto_mode_classifier(&no_marker));

        let no_transcript = with_system_user("answer <block>yes</block>", "please classify this");
        assert!(!auto_mode_classifier(&no_transcript));
    }

    #[test]
    fn stage_d_sets_low_only_when_effort_is_empty() {
        let mut request = with_system_user(
            "answer <block>yes</block>",
            "<transcript>\nls\n</transcript>",
        );
        apply_stage_d(&mut request);
        assert_eq!(from_request(&request).effort, "low");

        let mut already = request.clone();
        already.extensions.insert(BRIDGE_EFFORT, json!("high"));
        apply_stage_d(&mut already);
        assert_eq!(from_request(&already).effort, "high");
    }

    #[test]
    fn argv_pairs_effort_with_thinking_display_and_think_off() {
        let mut request = req();
        request.extensions.insert(BRIDGE_EFFORT, json!("low"));
        request.extensions.insert(THINK_OFF, json!(true));
        let args = cli_flag_args(&request);
        assert_eq!(
            args,
            vec![
                "--effort",
                "low",
                "--thinking-display",
                "summarized",
                "--thinking",
                "disabled",
            ]
        );
    }

    #[test]
    fn cli_schema_strips_only_the_root_schema_keyword() {
        let raw = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","$defs":{}}"#;
        let out = cli_schema(raw);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(value.get("$schema").is_none());
        assert_eq!(value["type"], "object");
        assert!(value.get("$defs").is_some());
    }

    #[test]
    fn set_effort_line_is_a_control_request() {
        let line = set_effort_line("high", "effort-abcdefabcdef");
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["type"], "control_request");
        assert_eq!(value["request_id"], "effort-abcdefabcdef");
        assert_eq!(value["request"]["subtype"], "apply_flag_settings");
        assert_eq!(value["request"]["settings"]["effortLevel"], "high");
        assert!(line.ends_with('\n'));
    }
}
