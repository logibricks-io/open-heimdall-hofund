//! Anthropic's advisor server tool: model-id spelling and reported calls.
//!
//! Claude Code writes a dotted Claude version with dashes, and a Messages
//! reply lists each advisor call under `usage.iterations`. Both belong to
//! this protocol. The router asks this module to rewrite or drop the tool
//! for one hop; it does not name the spelling itself.

use hofund_ir::{AdvisorCall, ContentPart, Message, Request, Tool};
use serde_json::{Map, Value};

use crate::jsonutil::Object;

const FAMILIES: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "mythos"];
const NOTE: &str = "# Advisor Tool\n";
const GONE: &str =
    "The advisor tool is no longer available; disregard the earlier advisor instructions.";

/// One model the gateway can route, and the name its provider expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedModel {
    /// Text a caller may use: the route key, `provider/model`, or the bare model.
    pub id: String,
    /// Route key the router resolves.
    pub route: String,
    /// Bare model id, used as the usage-row model.
    pub bare: String,
    /// Name written upstream, the same name the main model is given.
    pub wire: String,
    /// Provider id that serves `route`.
    pub provider: String,
}

/// A resolved advisor model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisorWire {
    /// Route key to dispatch.
    pub route: String,
    /// Bare model id for a usage row.
    pub bare: String,
    /// Name the provider expects on the tool.
    pub wire: String,
    /// Provider that serves it.
    pub provider: String,
}

/// Drops a trailing `[1m]` or `[2m]` mark and surrounding space.
#[must_use]
pub fn trim_marks(model: &str) -> &str {
    let mut model = model.trim();
    loop {
        let stripped = model
            .strip_suffix("[1m]")
            .or_else(|| model.strip_suffix("[2m]"));
        let Some(stripped) = stripped else {
            return model;
        };
        model = stripped.trim();
    }
}

/// The served model `raw` names, with the provider's own spelling.
///
/// An id that is served as written wins when every such entry is the same
/// route and provider. A dashed Claude version matches the served entries
/// whose dotted form dashes to it. Two entries on different routes or
/// providers match neither, including the same bare id listed twice.
#[must_use]
pub fn advisor_wire(raw: &str, served: &[ServedModel]) -> Option<AdvisorWire> {
    let model = trim_marks(raw);
    if model.is_empty() {
        return None;
    }
    let exact: Vec<&ServedModel> = served.iter().filter(|entry| entry.id == model).collect();
    if !exact.is_empty() {
        return agreed(&exact);
    }
    if !dashedish(model) {
        return None;
    }
    let dashed: Vec<&ServedModel> = served
        .iter()
        .filter(|entry| dash_claude_versions(&entry.id).eq_ignore_ascii_case(model))
        .collect();
    agreed(&dashed)
}

/// One wire when every entry is the same route and provider.
fn agreed(entries: &[&ServedModel]) -> Option<AdvisorWire> {
    let first = *entries.first()?;
    let same = entries
        .iter()
        .all(|entry| entry.route == first.route && entry.provider == first.provider);
    same.then(|| wire_of(first))
}

fn wire_of(entry: &ServedModel) -> AdvisorWire {
    AdvisorWire {
        route: entry.route.clone(),
        bare: entry.bare.clone(),
        wire: entry.wire.clone(),
        provider: entry.provider.clone(),
    }
}

/// Rewrites or drops advisor server tools for one hop.
///
/// An Anthropic Messages hop that is itself a Claude model, and that serves
/// the advisor model, keeps the tool under the provider's name. Any other
/// Messages hop drops the tool and voids the instructions that mention it.
/// A subscription hop keeps the tool so the bridge can answer it. Other
/// protocols drop it, because they have no such server tool.
pub fn prepare_advisor_for_hop(
    request: &mut Request,
    protocol: &str,
    provider: &str,
    upstream_model: &str,
    served: &[ServedModel],
) {
    if protocol == "subscription" || !request_has_advisor(request) {
        return;
    }
    if protocol != "anthropic-messages" {
        drop_advisor(request);
        return;
    }
    let claude = claude_model(upstream_model);
    let mut kept = Vec::with_capacity(request.tools.len());
    let mut dropped = false;
    for tool in std::mem::take(&mut request.tools) {
        if !is_advisor_tool(&tool) {
            kept.push(tool);
            continue;
        }
        let raw = tool_model(&tool);
        let resolved = advisor_wire(&raw, served);
        let same_provider = resolved
            .as_ref()
            .is_some_and(|wire| wire.provider == provider);
        if claude
            && same_provider
            && let Some(wire) = resolved
        {
            let mut tool = tool;
            tool.extensions
                .insert("model", Value::String(wire.wire.clone()));
            kept.push(tool);
            continue;
        }
        dropped = true;
    }
    request.tools = kept;
    if request.tools.is_empty() {
        request.tool_choice = None;
    }
    if dropped {
        void_advisor_instructions(request);
    }
}

/// Advisor calls listed in a Messages `usage` object.
///
/// Removes `iterations` so a later encode does not send the list back.
/// A list with no advisor entry yields an empty vec, which does not replace
/// an earlier list.
#[must_use]
pub fn calls_from(object: &mut Object) -> Vec<AdvisorCall> {
    let Some(value) = object.remove("iterations") else {
        return Vec::new();
    };
    let Value::Array(entries) = value else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let Value::Object(mut entry) = entry else {
                return None;
            };
            let kind = entry
                .remove("type")
                .and_then(|value| match value {
                    Value::String(text) => Some(text),
                    _ => None,
                })
                .unwrap_or_default();
            if kind != "advisor_message" {
                return None;
            }
            let model = entry
                .remove("model")
                .and_then(|value| match value {
                    Value::String(text) => Some(text),
                    _ => None,
                })
                .unwrap_or_default();
            let input_tokens = number(&mut entry, "input_tokens");
            let output_tokens = number(&mut entry, "output_tokens");
            let cache_read_tokens = number(&mut entry, "cache_read_input_tokens");
            let reported_write = number(&mut entry, "cache_creation_input_tokens");
            let (five, hour) = match entry.remove("cache_creation") {
                Some(Value::Object(mut creation)) => (
                    number(&mut creation, "ephemeral_5m_input_tokens"),
                    number(&mut creation, "ephemeral_1h_input_tokens"),
                ),
                _ => (0, 0),
            };
            let cache_write_tokens = reported_write.max(five.saturating_add(hour));
            Some(AdvisorCall {
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cache_write_5m_tokens: five,
                cache_write_1h_tokens: hour,
            })
        })
        .collect()
}

/// Whether `tool` is Anthropic's advisor server tool.
#[must_use]
pub fn is_advisor_tool(tool: &Tool) -> bool {
    tool.extensions
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("advisor_"))
}

fn request_has_advisor(request: &Request) -> bool {
    request.tools.iter().any(is_advisor_tool)
}

fn tool_model(tool: &Tool) -> String {
    tool.extensions
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn drop_advisor(request: &mut Request) {
    let before = request.tools.len();
    request.tools.retain(|tool| !is_advisor_tool(tool));
    if request.tools.is_empty() {
        request.tool_choice = None;
    }
    if request.tools.len() != before {
        void_advisor_instructions(request);
    }
}

fn void_advisor_instructions(request: &mut Request) {
    for message in &mut request.messages {
        void_message(message);
    }
}

fn void_message(message: &mut Message) {
    for part in &mut message.content {
        if let ContentPart::Text(text) = part {
            text.text = void_text(&text.text);
        }
    }
}

fn void_text(text: &str) -> String {
    let Some(start) = text.find(NOTE) else {
        return text.to_owned();
    };
    let rest = &text[start..];
    let end = rest
        .find("</system-reminder>")
        .map_or(text.len(), |index| start + index);
    let note = &text[start..end];
    let trail_at = note.trim_end().len();
    let trail = &note[trail_at..];
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(GONE);
    out.push_str(trail);
    out.push_str(&text[end..]);
    out
}

fn number(object: &mut Map<String, Value>, key: &str) -> u64 {
    match object.remove(key) {
        Some(Value::Number(value)) => value.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn claude_model(name: &str) -> bool {
    let lower = trim_marks(name).to_ascii_lowercase();
    let mut index = 0;
    while index < lower.len() {
        if let Some(end) = family_at(&lower, index)
            && version_at(&lower, end).is_some()
        {
            return true;
        }
        index += lower[index..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn dashedish(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(family_end) = family_at(&lower, index)
            && let Some(version_end) = dashed_version_at(&lower, family_end)
        {
            let _ = version_end;
            return true;
        }
        index += lower[index..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn dash_claude_versions(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let mut out = String::with_capacity(id.len());
    let mut rest = id;
    let mut lower_rest = lower.as_str();
    while let Some(start) = find_dotted(lower_rest) {
        out.push_str(&rest[..start]);
        let end = dotted_end(lower_rest, start);
        out.push_str(&rest[start..end].replace('.', "-"));
        rest = &rest[end..];
        lower_rest = &lower_rest[end..];
    }
    out.push_str(rest);
    out
}

fn find_dotted(lower: &str) -> Option<usize> {
    let mut index = 0;
    while index < lower.len() {
        if let Some(family_end) = family_at(lower, index)
            && dotted_version_at(lower, family_end).is_some()
        {
            return Some(index);
        }
        index += lower[index..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn dotted_end(lower: &str, start: usize) -> usize {
    let family_end = family_at(lower, start).unwrap_or(start);
    dotted_version_at(lower, family_end).unwrap_or(start)
}

fn family_at(lower: &str, index: usize) -> Option<usize> {
    let rest = lower.get(index..)?;
    let rest = rest.strip_prefix("claude-")?;
    let prefix = lower.len() - rest.len();
    for family in FAMILIES {
        if let Some(after) = rest.strip_prefix(family)
            && after.starts_with('-')
        {
            return Some(prefix + family.len());
        }
    }
    None
}

fn version_at(lower: &str, family_end: usize) -> Option<usize> {
    dotted_version_at(lower, family_end).or_else(|| dashed_version_at(lower, family_end))
}

fn dotted_version_at(lower: &str, family_end: usize) -> Option<usize> {
    let rest = lower.get(family_end..)?;
    let rest = rest.strip_prefix('-')?;
    let mut seen_dot = false;
    let mut length = 0;
    let bytes = rest.as_bytes();
    if bytes.first().is_none_or(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    while length < bytes.len() && (bytes[length].is_ascii_digit() || bytes[length] == b'.') {
        if bytes[length] == b'.' {
            seen_dot = true;
            if length + 1 >= bytes.len() || !bytes[length + 1].is_ascii_digit() {
                return None;
            }
        }
        length += 1;
    }
    seen_dot.then_some(family_end + 1 + length)
}

fn dashed_version_at(lower: &str, family_end: usize) -> Option<usize> {
    let rest = lower.get(family_end..)?;
    let rest = rest.strip_prefix('-')?;
    let bytes = rest.as_bytes();
    let mut length = 0;
    while length < bytes.len() && bytes[length].is_ascii_digit() {
        length += 1;
    }
    if length == 0 || bytes.get(length) != Some(&b'-') {
        return None;
    }
    let mut more = length + 1;
    if bytes.get(more).is_none_or(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    while more < bytes.len() && bytes[more].is_ascii_digit() {
        more += 1;
    }
    Some(family_end + 1 + more)
}

#[cfg(test)]
mod tests {
    use super::{ServedModel, advisor_wire, calls_from, dash_claude_versions, void_text};
    use hofund_ir::{AdvisorCall, Usage};

    fn served() -> Vec<ServedModel> {
        vec![
            ServedModel {
                id: "relay/claude-fable-5.2".to_owned(),
                route: "relay/claude-fable-5.2".to_owned(),
                bare: "claude-fable-5.2".to_owned(),
                wire: "claude-fable-5.2".to_owned(),
                provider: "relay".to_owned(),
            },
            ServedModel {
                id: "claude-fable-5.2".to_owned(),
                route: "relay/claude-fable-5.2".to_owned(),
                bare: "claude-fable-5.2".to_owned(),
                wire: "claude-fable-5.2".to_owned(),
                provider: "relay".to_owned(),
            },
        ]
    }

    #[test]
    fn d227_dashed_advisor_id_uses_the_provider_name() {
        let served = served();
        let dashed = advisor_wire("relay/claude-fable-5-2", &served).expect("dashed");
        assert_eq!(dashed.route, "relay/claude-fable-5.2");
        assert_eq!(dashed.wire, "claude-fable-5.2");
        assert_eq!(dashed.bare, "claude-fable-5.2");
        let marked = advisor_wire("relay/claude-fable-5-2[2m]", &served).expect("mark");
        assert_eq!(marked.wire, "claude-fable-5.2");
        let spelled = advisor_wire("relay/claude-fable-5.2", &served).expect("spelled");
        assert_eq!(spelled.route, "relay/claude-fable-5.2");
        assert_eq!(
            dash_claude_versions("relay/claude-opus-4.6-us"),
            "relay/claude-opus-4-6-us"
        );
    }

    #[test]
    fn d227_two_dashed_ids_match_neither() {
        let served = vec![
            ServedModel {
                id: "relay/claude-fable-5.2".to_owned(),
                route: "relay/claude-fable-5.2".to_owned(),
                bare: "claude-fable-5.2".to_owned(),
                wire: "claude-fable-5.2".to_owned(),
                provider: "relay".to_owned(),
            },
            ServedModel {
                id: "Relay/claude-fable-5.2".to_owned(),
                route: "Relay/claude-fable-5.2".to_owned(),
                bare: "claude-fable-5.2".to_owned(),
                wire: "claude-fable-5.2".to_owned(),
                provider: "Relay".to_owned(),
            },
        ];
        assert!(advisor_wire("relay/claude-fable-5-2", &served).is_none());
    }

    #[test]
    fn d227_same_bare_id_on_two_providers_matches_neither() {
        let bare = |provider: &str| ServedModel {
            id: "claude-opus-5.5".to_owned(),
            route: format!("{provider}/claude-opus-5.5"),
            bare: "claude-opus-5.5".to_owned(),
            wire: "claude-opus-5.5".to_owned(),
            provider: provider.to_owned(),
        };
        let served = vec![bare("alpha"), bare("beta")];
        assert!(advisor_wire("claude-opus-5-5", &served).is_none());
        assert!(advisor_wire("claude-opus-5.5", &served).is_none());
        let one = vec![bare("alpha"), bare("alpha")];
        let wire = advisor_wire("claude-opus-5-5", &one).expect("one provider");
        assert_eq!(wire.route, "alpha/claude-opus-5.5");
        assert_eq!(wire.provider, "alpha");
        let mut spelled = bare("alpha");
        spelled.id = spelled.route.clone();
        let mixed = vec![spelled, bare("beta")];
        let exact = advisor_wire("alpha/claude-opus-5.5", &mixed).expect("exact route");
        assert_eq!(exact.provider, "alpha");
    }

    #[test]
    fn d227_iterations_replace_when_the_list_has_an_advisor_call() {
        let raw = r#"{"input_tokens":1760,"cache_read_input_tokens":412,"cache_creation_input_tokens":0,"output_tokens":531,"iterations":[{"type":"message","input_tokens":412,"output_tokens":89},{"type":"advisor_message","model":"claude-opus-5-5","input_tokens":823,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1612},{"type":"message","input_tokens":1348,"output_tokens":442}]}"#;
        let mut object = serde_json::from_str(raw).expect("usage");
        let calls = calls_from(&mut object);
        assert!(object.get("iterations").is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            AdvisorCall {
                model: "claude-opus-5-5".to_owned(),
                input_tokens: 823,
                output_tokens: 1612,
                ..AdvisorCall::default()
            }
        );
        let mut usage = Usage {
            input_tokens: Some(1760),
            output_tokens: Some(531),
            advisor_calls: calls.clone(),
            ..Usage::default()
        };
        let again = Usage {
            advisor_calls: calls,
            ..Usage::default()
        };
        usage.merge_from(&again);
        assert_eq!(usage.advisor_calls.len(), 1);
        assert_eq!(usage.input_tokens, Some(1760));
        let mut plain = serde_json::from_str(r#"{"input_tokens":5,"output_tokens":2,"iterations":[{"type":"message","input_tokens":5,"output_tokens":2}]}"#).expect("plain");
        assert!(calls_from(&mut plain).is_empty());
        usage.merge_from(&Usage {
            output_tokens: Some(2),
            ..Usage::default()
        });
        assert_eq!(usage.advisor_calls.len(), 1);
        assert_eq!(usage.output_tokens, Some(2));
        let note = "# Advisor Tool\nCall it.\n</system-reminder> after";
        assert_eq!(
            void_text(note),
            "The advisor tool is no longer available; disregard the earlier advisor instructions.\n</system-reminder> after"
        );
    }
}
