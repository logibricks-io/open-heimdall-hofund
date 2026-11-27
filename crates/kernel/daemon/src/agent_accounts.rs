//! Accounts by agent, read from the published configuration.

use hofund_config::Config;

use crate::provider_keys;
use crate::subscriptions::Subscriptions;

/// `provider/id` pins for `agent`.
///
/// A provider is consulted only when that agent has a saved pin, so an unset
/// agent does not rebuild providers. A pin whose key or account is gone is
/// omitted and that provider is used as before. A key that is off stays in
/// the list, so the request does not move to another key of that provider.
pub(crate) fn pins(config: &Config, agent: &str, subscriptions: &Subscriptions) -> Vec<String> {
    let agent = agent.trim().to_lowercase();
    if agent.is_empty() {
        return Vec::new();
    }
    let mut pins = Vec::new();
    for provider in &config.providers {
        let Some(reference) = provider.agent_accounts.get(&agent) else {
            continue;
        };
        let Ok(keys) = provider_keys::resolve(provider) else {
            continue;
        };
        let named = keys
            .iter()
            .map(|key| (key.id.as_str(), key.entry.name.as_str()))
            .collect::<Vec<_>>();
        if let Some(id) = match_named(reference, &named) {
            pins.push(format!("{}/{}", provider.id, id));
        }
    }
    for state in subscriptions.values() {
        if let Some(reference) = state.config.agent_accounts.get(&agent)
            && let Some(pin) = state.agent_account_pin(reference)
        {
            pins.push(pin);
        }
    }
    pins
}

/// The account id `reference` names, matched by id then by a unique name.
#[must_use]
pub(crate) fn match_named(reference: &str, known: &[(&str, &str)]) -> Option<String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    if let Some((id, _)) = known
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(reference))
    {
        return Some((*id).to_owned());
    }
    let names = known
        .iter()
        .filter(|(_, name)| !name.trim().is_empty() && name.eq_ignore_ascii_case(reference))
        .collect::<Vec<_>>();
    if names.len() == 1 {
        return Some(names[0].0.to_owned());
    }
    None
}

/// `plugin/account` when `reference` is a current account id.
#[must_use]
pub(crate) fn subscription_pin(plugin: &str, accounts: &[&str], reference: &str) -> Option<String> {
    let reference = reference.trim();
    accounts.iter().find_map(|account| {
        account
            .eq_ignore_ascii_case(reference)
            .then(|| format!("{plugin}/{account}"))
    })
}

#[cfg(test)]
mod tests {
    use super::{match_named, subscription_pin};

    #[test]
    fn d278_a_saved_name_resolves_to_one_fingerprint_and_a_missing_key_does_not() {
        let known = [("aaaaaaaaaa", "First"), ("bbbbbbbbbb", "Second")];
        assert_eq!(match_named("second", &known).as_deref(), Some("bbbbbbbbbb"));
        assert_eq!(
            match_named("AAAAAAAAAA", &known).as_deref(),
            Some("aaaaaaaaaa")
        );
        assert_eq!(match_named("third", &known), None);
        assert_eq!(match_named("", &known), None);
        let tied = [("aaaaaaaaaa", "Same"), ("bbbbbbbbbb", "Same")];
        assert_eq!(match_named("same", &tied), None);
    }

    #[test]
    fn d278_a_missing_subscription_account_is_not_a_pin() {
        assert_eq!(
            subscription_pin("relay-sub", &["work"], "work").as_deref(),
            Some("relay-sub/work")
        );
        assert_eq!(subscription_pin("relay-sub", &["work"], "gone"), None);
    }
}
