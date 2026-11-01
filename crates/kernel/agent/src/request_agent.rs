//! Resolve which agent a gateway request came from.

use hofund_plugin_api::AgentDescription;

const SUBAGENT_HEADER: &str = "x-claude-code-agent-id";
const STREAM_CLIENT_PREFIXES: [&str; 2] = ["codex", "acp-extension-codex"];
const PLUGIN_HOST_PREFIX: &str = "claude code";
const STREAM_WINDOW_HEADER: &str = "x-codex-window-id";
const STREAM_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";

/// Resolves the agent id for a request from `headers` and compiled descriptions.
///
/// `key_name` is the authenticated gateway key's display name. It matches an
/// agent id when the headers do not. When no agent matches, returns `None`.
#[must_use]
pub fn request_agent_id<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    key_name: &str,
    descriptions: &[&AgentDescription],
) -> Option<String> {
    let map: Vec<(String, String)> = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    let header = |name: &str| -> Option<&str> {
        map.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if let Some(agent) = header(SUBAGENT_HEADER)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return match_agent(agent, descriptions);
    }
    if codex_for_claude_code(&map) {
        return match_agent("codex", descriptions);
    }
    let user_agent = header("user-agent").unwrap_or_default();
    let originator = header("originator").unwrap_or_default();
    if let Some(id) = match_user_agent(user_agent, descriptions) {
        return Some(id);
    }
    if !originator.is_empty()
        && let Some(id) = match_user_agent(originator, descriptions)
    {
        return Some(id);
    }
    let key_name = key_name.trim();
    if !key_name.is_empty() {
        return match_agent(key_name, descriptions);
    }
    None
}

fn codex_for_claude_code(headers: &[(String, String)]) -> bool {
    let header = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if header(STREAM_WINDOW_HEADER).is_none() && header(STREAM_TURN_METADATA_HEADER).is_none() {
        return false;
    }
    ["user-agent", "originator"].into_iter().any(|name| {
        header(name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with(PLUGIN_HOST_PREFIX))
    })
}

fn match_user_agent(value: &str, descriptions: &[&AgentDescription]) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let product = trimmed
        .split('/')
        .next()
        .unwrap_or(trimmed)
        .split_whitespace()
        .next()
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    if product == "cursor" {
        return match_agent("cursor-local", descriptions).or_else(|| Some("cursor-local".into()));
    }
    for description in descriptions {
        let id = description.id().as_str();
        if id.eq_ignore_ascii_case(&product) {
            return Some(normalize_agent_id(id));
        }
        if description.binary().eq_ignore_ascii_case(&product) {
            return Some(normalize_agent_id(id));
        }
        if product.starts_with(description.binary()) {
            return Some(normalize_agent_id(id));
        }
        if STREAM_CLIENT_PREFIXES
            .iter()
            .any(|prefix| product.starts_with(prefix))
            && (id == "codex" || description.binary() == "codex")
        {
            return Some(normalize_agent_id(id));
        }
    }
    None
}

fn match_agent(candidate: &str, descriptions: &[&AgentDescription]) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }
    for description in descriptions {
        let id = description.id().as_str();
        if id.eq_ignore_ascii_case(candidate)
            || description.binary().eq_ignore_ascii_case(candidate)
        {
            return Some(normalize_agent_id(id));
        }
    }
    None
}

fn normalize_agent_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{AgentDescription, PluginId};

    use super::request_agent_id;

    static TEST_AGENT: AgentDescription = AgentDescription::from_static(
        PluginId::from_static("codex"),
        "Codex",
        "codex",
        &[],
        &[],
        None,
        None,
        hofund_plugin_api::Visible::All,
        hofund_plugin_api::AgentRoute::Gateway,
        &[],
    );

    static CLAUDE: AgentDescription = AgentDescription::from_static(
        PluginId::from_static("claude-code"),
        "Claude Code",
        "claude",
        &[],
        &[],
        None,
        None,
        hofund_plugin_api::Visible::All,
        hofund_plugin_api::AgentRoute::Gateway,
        &[],
    );

    #[test]
    fn g2_7_request_agent_reads_subagent_header() {
        let headers = [("x-claude-code-agent-id", "claude-code")];
        assert_eq!(
            request_agent_id(headers, "hofund", &[&CLAUDE]),
            Some("claude-code".to_owned())
        );
    }

    #[test]
    fn g2_8_request_agent_matches_user_agent_prefix() {
        let headers = [("user-agent", "codex/1.0 (test)")];
        assert_eq!(
            request_agent_id(headers, "hofund", &[&TEST_AGENT]),
            Some("codex".to_owned())
        );
    }

    #[test]
    fn g2_9_codex_turn_under_claude_code_names_codex() {
        let headers = [
            ("user-agent", "Claude Code/1.0"),
            ("x-codex-window-id", "w"),
        ];
        assert_eq!(
            request_agent_id(headers, "hofund", &[&TEST_AGENT, &CLAUDE]),
            Some("codex".to_owned())
        );
    }

    #[test]
    fn g2_10_gateway_key_name_matches_description() {
        assert_eq!(
            request_agent_id([], "Codex", &[&TEST_AGENT]),
            Some("codex".to_owned())
        );
        assert_eq!(request_agent_id([], "other", &[&TEST_AGENT]), None);
    }
}
