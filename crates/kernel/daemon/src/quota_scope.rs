//! Gateway-key quota scope for model-listener quota routes.

use std::collections::HashMap;
use std::sync::Arc;

use hofund_config::Config;
use hofund_gateway::{QuotaAccess, QuotaScope};
use hofund_secrets::{Identity, allows_account, model_allowed};

/// Error code returned when a held key may not read another machine's accounts.
pub const KEY_HELD_CODE: &str = "gateway_key_held";

/// What a held key is told when it may read no account cards (same text as the model gateway).
#[expect(dead_code)]
pub const HELD_REFUSED: &str = "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts: GET /v1/hofund/limit tells it its own limit";

/// What a held key is told when its owner has not turned quota visibility on (same text as the model gateway).
#[expect(dead_code)]
pub const HELD_QUOTAS_OFF: &str = "this gateway key is held to a budget, models or accounts, so it is told nothing of this gateway's accounts until its owner turns on Quotas for it (Gateway keys, or `hofund gateway-key quotas <id> on`); it is then told the accounts it may use of each provider whose every model it may use. GET /v1/hofund/limit tells it its own limit and what holds it";

/// Catalog data used to decide whether a key may use every model of a provider.
#[derive(Clone, Default)]
pub(crate) struct QuotaCatalog {
    models: HashMap<String, Vec<String>>,
}

impl QuotaCatalog {
    /// Builds provider scope from config and exposed model ids per provider.
    #[must_use]
    pub fn build(config: &Config, models: &HashMap<String, Vec<String>>) -> Self {
        let mut out = HashMap::new();
        for provider in &config.providers {
            out.insert(
                provider.id.clone(),
                models.get(&provider.id).cloned().unwrap_or_default(),
            );
        }
        for subscription in &config.subscriptions {
            out.insert(subscription.id.clone(), Vec::new());
        }
        Self { models: out }
    }
}

/// Whether the caller is a gateway key held to a budget, models, or accounts.
#[must_use]
pub fn held_key(identity: &Identity) -> bool {
    !identity.key_id.is_empty()
        && (!identity.limits.is_empty()
            || !identity.models.is_empty()
            || !identity.accounts.is_empty())
}

/// Resolves quota access for a caller on the model listener.
#[must_use]
pub fn quota_access(identity: Option<&Identity>, catalog: &QuotaCatalog) -> QuotaAccess {
    let Some(identity) = identity.filter(|id| held_key(id)) else {
        return QuotaAccess::All;
    };
    if !identity.quotas {
        return QuotaAccess::Refused;
    }
    QuotaAccess::Scoped(Arc::new(KeyQuotaScope::new(identity, catalog)))
}

struct KeyQuotaScope {
    identity: Identity,
    catalog: QuotaCatalog,
}

impl KeyQuotaScope {
    fn new(identity: &Identity, catalog: &QuotaCatalog) -> Self {
        Self {
            identity: identity.clone(),
            catalog: catalog.clone(),
        }
    }

    fn provider_ok(&self, provider: &str) -> bool {
        if self.identity.models.is_empty() {
            return true;
        }
        let Some(listed) = self.catalog.models.get(provider) else {
            return true;
        };
        if listed.is_empty() {
            return true;
        }
        listed.iter().all(|model| {
            model_allowed(&self.identity.models, &format!("{provider}/{model}"))
                || model_allowed(&self.identity.models, &format!("{provider}/*"))
                || model_allowed(&self.identity.models, provider)
        })
    }
}

impl QuotaScope for KeyQuotaScope {
    fn card(&self, provider: &str, user: &str) -> bool {
        if !self.provider_ok(provider) {
            return false;
        }
        if self.identity.accounts.is_empty() {
            return true;
        }
        if user.trim().is_empty() {
            return allows_account(&self.identity.accounts, provider, "");
        }
        allows_account(&self.identity.accounts, provider, user)
            || allows_account(
                &self.identity.accounts,
                provider,
                &user.to_ascii_lowercase(),
            )
    }
}

#[cfg(test)]
mod tests {
    use hofund_config::load;

    use super::*;

    #[test]
    fn qs1_a_held_key_without_quotas_is_refused() {
        let identity = Identity {
            key_id: "k".to_owned(),
            limits: vec![hofund_secrets::KeyLimit {
                period: "day".to_owned(),
                tokens: 1,
                ..hofund_secrets::KeyLimit::default()
            }],
            quotas: false,
            ..Identity::default()
        };
        assert!(matches!(
            quota_access(Some(&identity), &QuotaCatalog::default()),
            QuotaAccess::Refused
        ));
    }

    #[test]
    fn qs2_scoped_key_matches_provider_and_account() {
        let config = load(
            "[[providers]]\nid = \"alpha\"\npreset = \"relay\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"k\"\n",
        )
        .expect("config");
        let catalog = QuotaCatalog::build(
            &config,
            &HashMap::from([("alpha".to_owned(), vec!["m1".to_owned()])]),
        );
        let identity = Identity {
            key_id: "k".to_owned(),
            models: vec!["alpha/*".to_owned()],
            accounts: vec!["alpha/two".to_owned()],
            quotas: true,
            ..Identity::default()
        };
        let QuotaAccess::Scoped(scope) = quota_access(Some(&identity), &catalog) else {
            panic!("expected scoped access");
        };
        assert!(scope.card("alpha", "two"));
        assert!(!scope.card("alpha", "one"));
    }
}
