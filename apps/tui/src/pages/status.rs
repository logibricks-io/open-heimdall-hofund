//! Provider status column. The text follows the first matching rule and does
//! not use `ProviderSummary.ready`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use hofund_control_client::hofund_control_proto::{PresetEntry, PresetKind, ProviderSummary};

/// Whether this row is an account-backed provider.
#[must_use]
pub fn is_account_row(summary: &ProviderSummary) -> bool {
    summary.signed_out
        || !summary.account_label.is_empty()
        || summary.kind.eq_ignore_ascii_case("account")
        || summary.kind.eq_ignore_ascii_case("subscription")
}

/// Whether the preset stores a project instead of a single API key.
#[must_use]
pub fn is_project_preset(summary: &ProviderSummary, presets: &[PresetEntry]) -> bool {
    if summary.preset.as_deref() == Some("google-vertex") {
        return true;
    }
    matching_preset(summary, presets).is_some_and(|preset| preset.needs_project)
}

/// Whether `w` may edit `base_url`.
#[must_use]
pub fn address_is_editable(summary: &ProviderSummary, presets: &[PresetEntry]) -> bool {
    if is_project_preset(summary, presets) {
        return false;
    }
    let Some(preset) = matching_preset(summary, presets) else {
        return optional_network(&summary.base_url);
    };
    if preset.needs_project {
        return false;
    }
    preset.needs_address
        || matches!(
            preset.kind,
            PresetKind::Local | PresetKind::Remote | PresetKind::Custom
        )
}

/// Preset row for `summary`, when the catalog has been loaded.
#[must_use]
pub fn matching_preset<'a>(
    summary: &ProviderSummary,
    presets: &'a [PresetEntry],
) -> Option<&'a PresetEntry> {
    let id = summary.preset.as_deref()?;
    presets.iter().find(|preset| preset.id == id)
}

/// Whether a key is optional for this preset or address.
#[must_use]
pub fn key_not_required(needs_key: bool, base_url: &str) -> bool {
    !needs_key || optional_network(base_url)
}

/// Status text for one provider row.
#[must_use]
pub fn provider_status_line(summary: &ProviderSummary, presets: &[PresetEntry]) -> String {
    if !summary.enabled {
        return "switched off".to_owned();
    }
    if is_account_row(summary) && summary.signed_out {
        return if summary.account_label.is_empty() {
            "signed out".to_owned()
        } else {
            format!("signed out {}", summary.account_label)
        };
    }
    if is_account_row(summary) {
        return summary.account_label.clone();
    }
    if is_project_preset(summary, presets) {
        if !summary.project_address_ready {
            return "needs a project".to_owned();
        }
        return if summary.impersonate_account.is_empty() {
            "Google credentials".to_owned()
        } else {
            format!("Google credentials as {}", summary.impersonate_account)
        };
    }
    if !summary.key_masked.is_empty() {
        return summary.key_masked.clone();
    }
    let needs_key = matching_preset(summary, presets).is_none_or(|preset| preset.needs_key);
    if key_not_required(needs_key, &summary.base_url) {
        return "no key needed".to_owned();
    }
    "no key".to_owned()
}

fn optional_network(base_url: &str) -> bool {
    let Some(host) = host_of(base_url) else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || is_cgnat(address)
        }
        Ok(IpAddr::V6(address)) => {
            address.is_loopback()
                || address.is_unspecified()
                || is_ipv6_link_local(address)
                || address.to_ipv4_mapped().is_some_and(|mapped| {
                    mapped.is_loopback()
                        || mapped.is_private()
                        || mapped.is_link_local()
                        || mapped.is_unspecified()
                        || is_cgnat(mapped)
                })
        }
        Err(_) => false,
    }
}

fn is_cgnat(address: Ipv4Addr) -> bool {
    let [first, second, _, _] = address.octets();
    first == 100 && (64..128).contains(&second)
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xffc0) == 0xfe80
}

fn host_of(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let host_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[..end].to_owned());
    }
    Some(host_port.split(':').next().unwrap_or(host_port).to_owned())
}

#[cfg(test)]
mod tests {
    use super::{key_not_required, provider_status_line};
    use hofund_control_client::hofund_control_proto::{PresetEntry, PresetKind, ProviderSummary};

    fn summary(id: &str) -> ProviderSummary {
        let mut summary = ProviderSummary::default();
        summary.id = id.to_owned();
        summary.enabled = true;
        summary
    }

    fn preset(id: &str, needs_key: bool, needs_project: bool, kind: PresetKind) -> PresetEntry {
        let mut preset = PresetEntry::default();
        preset.id = id.to_owned();
        preset.needs_key = needs_key;
        preset.needs_project = needs_project;
        preset.kind = kind;
        preset
    }

    #[test]
    fn status_uses_first_match_and_ignores_ready() {
        let mut off = summary("off");
        off.enabled = false;
        off.ready = true;
        off.key_masked = "sk-****".to_owned();
        assert_eq!(provider_status_line(&off, &[]), "switched off");

        let mut signed_out = summary("acct");
        signed_out.kind = "account".to_owned();
        signed_out.signed_out = true;
        signed_out.account_label = "ada".to_owned();
        signed_out.ready = true;
        assert_eq!(provider_status_line(&signed_out, &[]), "signed out ada");

        let mut labeled = summary("acct2");
        labeled.account_label = "ada".to_owned();
        assert_eq!(provider_status_line(&labeled, &[]), "ada");

        let mut vertex = summary("vertex");
        vertex.preset = Some("google-vertex".to_owned());
        vertex.project_address_ready = false;
        vertex.key_masked = "must-not-show".to_owned();
        assert_eq!(provider_status_line(&vertex, &[]), "needs a project");

        vertex.project_address_ready = true;
        vertex.impersonate_account = "svc".to_owned();
        assert_eq!(
            provider_status_line(&vertex, &[]),
            "Google credentials as svc"
        );

        let mut keyed = summary("keyed");
        keyed.key_masked = "ab…yz".to_owned();
        assert_eq!(provider_status_line(&keyed, &[]), "ab…yz");

        let mut local = summary("local");
        local.base_url = "http://127.0.0.1:1234/v1".to_owned();
        assert_eq!(provider_status_line(&local, &[]), "no key needed");

        let mut missing = summary("missing");
        missing.base_url = "https://example.test/v1".to_owned();
        assert_eq!(provider_status_line(&missing, &[]), "no key");
    }

    #[test]
    fn optional_networks_do_not_require_a_key() {
        assert!(key_not_required(true, "http://localhost:11434"));
        assert!(key_not_required(true, "http://10.1.2.3/v1"));
        assert!(key_not_required(true, "http://100.64.0.1:1"));
        assert!(key_not_required(true, "http://host.local/v1"));
        assert!(key_not_required(false, "https://example.test"));
        assert!(!key_not_required(true, "https://example.test/v1"));
        let presets = vec![preset("ollama", false, false, PresetKind::Local)];
        let mut row = summary("ollama");
        row.preset = Some("ollama".to_owned());
        row.base_url = "https://example.test".to_owned();
        assert_eq!(provider_status_line(&row, &presets), "no key needed");
    }
}
