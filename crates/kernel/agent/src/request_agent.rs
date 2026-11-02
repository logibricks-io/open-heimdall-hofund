//! Resolve which agent a gateway request came from.

use hofund_plugin_api::AgentDescription;

/// Resolves the agent id for a request from `headers` and compiled descriptions.
///
/// A description may declare a request header whose value is an agent id, product
/// prefixes for the User-Agent and `originator`, and headers that select that
/// agent when present. `key_name` is the authenticated gateway key's display
/// name. It matches an agent id when the headers do not. When no agent matches,
/// returns `None`.
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
    if let Some(value) = request_header_value(&map, descriptions) {
        return match_agent(value, descriptions);
    }
    if let Some(id) = match_nested_turn(&map, descriptions) {
        return Some(id);
    }
    let user_agent = header_value(&map, "user-agent").unwrap_or_default();
    let originator = header_value(&map, "originator").unwrap_or_default();
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

/// Non-empty value of the first declared request-agent header that is present.
fn request_header_value<'a>(
    headers: &'a [(String, String)],
    descriptions: &[&AgentDescription],
) -> Option<&'a str> {
    for description in descriptions {
        let Some(name) = description.request_agent_header() else {
            continue;
        };
        if let Some(value) = header_value(headers, name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
    }
    None
}

fn match_nested_turn(
    headers: &[(String, String)],
    descriptions: &[&AgentDescription],
) -> Option<String> {
    for description in descriptions {
        let declared = description.nested_turn_headers();
        if declared.is_empty() {
            continue;
        }
        if declared
            .iter()
            .any(|name| header_value(headers, name).is_some())
        {
            return Some(normalize_agent_id(description.id().as_str()));
        }
    }
    None
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let name = name.to_ascii_lowercase();
    headers
        .iter()
        .find(|(key, _)| key == &name)
        .map(|(_, value)| value.as_str())
}

fn match_user_agent(value: &str, descriptions: &[&AgentDescription]) -> Option<String> {
    let product = product_token(value)?;
    for description in descriptions {
        let id = description.id().as_str();
        if id.eq_ignore_ascii_case(&product) || description.binary().eq_ignore_ascii_case(&product)
        {
            return Some(normalize_agent_id(id));
        }
        let binary = description.binary().to_ascii_lowercase();
        if !binary.is_empty() && product.starts_with(&binary) {
            return Some(normalize_agent_id(id));
        }
        if description.client_prefixes().iter().any(|prefix| {
            let prefix = prefix.to_ascii_lowercase();
            !prefix.is_empty() && product.starts_with(&prefix)
        }) {
            return Some(normalize_agent_id(id));
        }
    }
    None
}

fn product_token(value: &str) -> Option<String> {
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
    (!product.is_empty()).then_some(product)
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

    const fn description(id: &'static str, binary: &'static str) -> AgentDescription {
        AgentDescription::from_static(
            PluginId::from_static(id),
            "Fixture",
            binary,
            &[],
            &[],
            None,
            None,
            hofund_plugin_api::Visible::All,
            hofund_plugin_api::AgentRoute::Gateway,
            &[],
        )
    }

    static HOST: AgentDescription =
        description("fixture-host", "host").with_request_agent_header("x-fixture-agent-id");

    static AGENT: AgentDescription = description("fixture-agent", "fixture")
        .with_client_prefixes(&["fixture", "fixture-extension"])
        .with_nested_turn_headers(&["x-fixture-window-id", "x-fixture-turn-metadata"]);

    #[test]
    fn g2_7_request_agent_reads_subagent_header() {
        let headers = [("X-Fixture-Agent-Id", "fixture-host")];
        assert_eq!(
            request_agent_id(headers, "gateway", &[&HOST, &AGENT]),
            Some("fixture-host".to_owned())
        );
        let unknown = [
            ("x-fixture-agent-id", "missing"),
            ("user-agent", "fixture/1.0"),
        ];
        assert_eq!(request_agent_id(unknown, "gateway", &[&HOST, &AGENT]), None);
    }

    #[test]
    fn g2_8_request_agent_matches_user_agent_prefix() {
        let headers = [("user-agent", "fixture/1.0 (test)")];
        assert_eq!(
            request_agent_id(headers, "gateway", &[&AGENT]),
            Some("fixture-agent".to_owned())
        );
        let extension = [("originator", "fixture-extension/2")];
        assert_eq!(
            request_agent_id(extension, "gateway", &[&HOST, &AGENT]),
            Some("fixture-agent".to_owned())
        );
    }

    #[test]
    fn g2_9_nested_turn_names_its_agent() {
        let window = [("user-agent", "host/1.0"), ("x-fixture-window-id", "w")];
        assert_eq!(
            request_agent_id(window, "gateway", &[&HOST, &AGENT]),
            Some("fixture-agent".to_owned())
        );
        let turn = [
            ("user-agent", "host/1.0"),
            ("x-fixture-turn-metadata", "{}"),
        ];
        assert_eq!(
            request_agent_id(turn, "gateway", &[&HOST, &AGENT]),
            Some("fixture-agent".to_owned())
        );
        let host_only = [("user-agent", "host/1.0")];
        assert_eq!(
            request_agent_id(host_only, "gateway", &[&HOST, &AGENT]),
            Some("fixture-host".to_owned())
        );
    }

    #[test]
    fn g2_10_gateway_key_name_matches_description() {
        assert_eq!(
            request_agent_id([], "Fixture-Agent", &[&AGENT]),
            Some("fixture-agent".to_owned())
        );
        assert_eq!(request_agent_id([], "other", &[&AGENT]), None);
        let unmatched = [("user-agent", "unmatched/1.0")];
        assert_eq!(request_agent_id(unmatched, "other", &[&AGENT]), None);
    }

    static PREFIXED: AgentDescription =
        description("prefixed-agent", "pa").with_client_prefixes(&["acp-ext"]);

    static BINARY: AgentDescription = description("binary-agent", "binx");

    static UPPER: AgentDescription =
        description("upper-agent", "upper").with_request_agent_header("X-Upper-Agent-Id");

    #[test]
    fn g2_18_prefixes_binary_header_case_and_blank_header() {
        let prefixed = [("user-agent", "acp-ext-tool/1")];
        assert_eq!(
            request_agent_id(prefixed, "gateway", &[&PREFIXED]),
            Some("prefixed-agent".to_owned())
        );
        let binary = [("user-agent", "binx-cli/2")];
        assert_eq!(
            request_agent_id(binary, "gateway", &[&BINARY]),
            Some("binary-agent".to_owned())
        );
        let upper = [("x-upper-agent-id", "upper-agent")];
        assert_eq!(
            request_agent_id(upper, "gateway", &[&UPPER]),
            Some("upper-agent".to_owned())
        );
        let blank = [("x-fixture-agent-id", "  "), ("user-agent", "fixture/1.0")];
        assert_eq!(
            request_agent_id(blank, "gateway", &[&HOST, &AGENT]),
            Some("fixture-agent".to_owned())
        );
    }
}
