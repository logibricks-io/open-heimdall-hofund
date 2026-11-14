//! Per-account `fetchAvailableModels` tier fallback and 24-hour refusal memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::SecretObject;

use crate::{Endpoints, project};

const REFUSED_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const TIERS_FOR: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct TierReading {
    tier: String,
    paid: String,
    at: SystemTime,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RefusedKey {
    account: AccountId,
    project: String,
    tier: String,
}

#[derive(Default)]
struct State {
    tiers: HashMap<AccountId, TierReading>,
    refused: HashMap<RefusedKey, SystemTime>,
}

/// Per-instance tier readings and refused tiers, shared by quota and model lists.
#[derive(Default)]
pub(crate) struct TierProbe {
    state: Mutex<State>,
}

impl TierProbe {
    pub(crate) fn forget(&self, account: &AccountId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tiers.remove(account);
        state.refused.retain(|key, _| &key.account != account);
    }

    pub(crate) fn forget_project(&self, account: &AccountId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tiers.remove(account);
        state.refused.retain(|key, _| &key.account != account);
    }

    /// Names each entitlement in order, falls back to no tier, and remembers
    /// tiers Google refused with HTTP 400 for [`REFUSED_FOR`].
    pub(crate) async fn probe(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        account: &AccountId,
        access: &SecretString,
        project: &str,
    ) -> Result<(), PluginError> {
        let (current, paid) = self.tiers(host, endpoints, account, access).await;
        let tiers = self.ask_tiers(host, account, project, &current, &paid);
        let mut last_error = None;
        for tier in tiers {
            let mut body = json!({ "project": project });
            if !tier.is_empty() {
                body["entitlement"] = json!({"userTier": tier});
            }
            match project::call(host, endpoints, access, "fetchAvailableModels", &body).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    if !tier.is_empty() && error.status() == Some(400) {
                        self.record_refused(host, account, project, &tier);
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PluginError::status_error(502, "Code Assist returned no model list")
        }))
    }

    async fn tiers(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        account: &AccountId,
        access: &SecretString,
    ) -> (String, String) {
        let now = host.clock().now();
        if let Some(reading) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .get(account)
            .cloned()
            && now.duration_since(reading.at).unwrap_or_default() < TIERS_FOR
        {
            return (reading.tier, reading.paid);
        }
        let stored = host.accounts().secret(account).await.ok().flatten();
        let stored = stored.and_then(|secret| SecretObject::parse(&secret).ok());
        if !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .contains_key(account)
            && let Some(blob) = &stored
            && (blob.text(project::TIER).is_some() || blob.text(project::PAID).is_some())
        {
            let reading = TierReading {
                tier: blob.text(project::TIER).unwrap_or_default().to_owned(),
                paid: blob.text(project::PAID).unwrap_or_default().to_owned(),
                at: now,
            };
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tiers
                .insert(account.clone(), reading.clone());
            return (reading.tier, reading.paid);
        }

        let body = json!({ "metadata": project::client_metadata(None) });
        let loaded = project::call(host, endpoints, access, "loadCodeAssist", &body)
            .await
            .map(|reply| project::tier_ids(&reply));
        let previous = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .get(account)
            .cloned();
        let (tier, paid) = match (loaded, previous, stored) {
            (Ok(ids), _, _) => ids,
            (Err(_), Some(reading), _) => (reading.tier, reading.paid),
            (Err(_), None, Some(blob)) => (
                blob.text(project::TIER).unwrap_or_default().to_owned(),
                blob.text(project::PAID).unwrap_or_default().to_owned(),
            ),
            (Err(_), None, None) => (String::new(), String::new()),
        };
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .insert(
                account.clone(),
                TierReading {
                    tier: tier.clone(),
                    paid: paid.clone(),
                    at: now,
                },
            );
        (tier, paid)
    }

    fn ask_tiers(
        &self,
        host: &HostContext,
        account: &AccountId,
        project: &str,
        current: &str,
        paid: &str,
    ) -> Vec<String> {
        let now = host.clock().now();
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for tier in [paid, current] {
            if tier.is_empty()
                || matches!(tier, "free-tier" | "legacy-tier")
                || out.iter().any(|known| known == tier)
            {
                continue;
            }
            let refused = state
                .refused
                .get(&RefusedKey {
                    account: account.clone(),
                    project: project.to_owned(),
                    tier: tier.to_owned(),
                })
                .is_some_and(|at| now.duration_since(*at).unwrap_or_default() < REFUSED_FOR);
            if !refused {
                out.push(tier.to_owned());
            }
        }
        out.push(String::new());
        out
    }

    fn record_refused(&self, host: &HostContext, account: &AccountId, project: &str, tier: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refused
            .insert(
                RefusedKey {
                    account: account.clone(),
                    project: project.to_owned(),
                    tier: tier.to_owned(),
                },
                host.clock().now(),
            );
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{AccountId, PluginId, PluginKind, SecretString, SubscriptionFactory};
    use hofund_plugin_sdk::json::{self, Value};
    use hofund_plugin_sdk::oauth::SecretObject;

    use super::{REFUSED_FOR, TierProbe};
    use crate::{Endpoints, factory_with_endpoints, project};

    fn test_host(dir: &tempfile::TempDir, now: SystemTime) -> TestHost {
        TestHost::open_for_at(
            PluginKind::Subscription,
            PluginId::from_static("gemini"),
            dir.path(),
            now,
        )
    }

    fn endpoints() -> Endpoints {
        Endpoints {
            authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth.test/token".to_owned(),
            userinfo: "https://oauth.test/userinfo".to_owned(),
            code_assist: "https://code-assist.test".to_owned(),
            onboard_poll: Duration::from_millis(1),
        }
    }

    fn method_url(method: &str) -> String {
        format!("https://code-assist.test/v1internal:{method}")
    }

    fn token_reply() -> Vec<u8> {
        br#"{"access_token":"access-example","token_type":"Bearer","expires_in":3600}"#.to_vec()
    }

    #[test]
    fn r49_refused_tier_window_is_one_day() {
        assert_eq!(REFUSED_FOR, Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn r49_paid_tier_400_is_skipped_until_the_window_passes() {
        let dir = tempfile::tempdir().expect("dir");
        let started = SystemTime::UNIX_EPOCH;
        let host = test_host(&dir, started);
        let blob = SecretObject::new()
            .with_number("v", 1)
            .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
            .with_text("email", "u@example.test")
            .with_text(project::PROJECT, "proj-1")
            .with_text(project::TIER, "free-tier")
            .with_text(project::PAID, "g1-pro-tier");
        let account = AccountId::from_static("a-0000000000000001");
        block_on(async {
            let accounts = host.context().accounts();
            let row = accounts
                .account(account.clone(), "u@example.test")
                .expect("account");
            accounts
                .upsert(row, blob.to_secret())
                .await
                .expect("upsert");
        });
        let factory = factory_with_endpoints(endpoints());
        let cfg = factory
            .validate(&hofund_plugin_api::RawTable::new())
            .expect("config");
        let built = factory.build(&cfg, host.context()).expect("build");
        host.http().enqueue(token_reply());
        host.http()
            .enqueue_response(400, &[], br#"{"error":{"message":"bad tier"}}"#.to_vec());
        host.http().enqueue(br#"{"models":{}}"#.to_vec());
        host.http().enqueue(br#"{"buckets":[]}"#.to_vec());
        host.http().enqueue(br#"{"flags":[]}"#.to_vec());
        block_on(built.quota(&account, host.context())).expect("quota");

        host.http().enqueue(br#"{"buckets":[]}"#.to_vec());
        host.http().enqueue(br#"{"flags":[]}"#.to_vec());
        block_on(built.quota(&account, host.context())).expect("quota again");

        let calls = host
            .http()
            .calls()
            .iter()
            .filter(|call| call.uri() == method_url("fetchAvailableModels"))
            .count();
        assert_eq!(calls, 3, "the second probe names only the no-tier ask");
        let named: usize = host
            .http()
            .calls()
            .iter()
            .filter(|call| call.uri() == method_url("fetchAvailableModels"))
            .filter_map(|call| json::from_slice::<Value>(call.body()).ok())
            .filter(|body| body.get("entitlement").is_some())
            .count();
        assert_eq!(named, 1);
    }

    #[test]
    fn r49_refused_tier_is_named_again_after_a_day() {
        let dir = tempfile::tempdir().expect("dir");
        let started = SystemTime::UNIX_EPOCH;
        let host = test_host(&dir, started);
        let probe = TierProbe::default();
        let account = AccountId::from_static("a-0000000000000001");
        let access = SecretString::from("access-example".to_owned());
        let blob = SecretObject::new()
            .with_text(project::PAID, "g1-pro-tier")
            .with_text(project::TIER, "free-tier");
        block_on(async {
            let accounts = host.context().accounts();
            let row = accounts
                .account(account.clone(), "u@example.test")
                .expect("account");
            accounts
                .upsert(row, blob.to_secret())
                .await
                .expect("upsert");
        });
        host.http()
            .enqueue_response(400, &[], br#"{"error":{"message":"bad tier"}}"#.to_vec());
        host.http().enqueue(br#"{"models":{}}"#.to_vec());
        block_on(probe.probe(host.context(), &endpoints(), &account, &access, "proj-1"))
            .expect("probe");

        let later = started + REFUSED_FOR + Duration::from_secs(60);
        let host_later = test_host(&dir, later);
        host_later.http().enqueue_response(
            400,
            &[],
            br#"{"error":{"message":"bad tier"}}"#.to_vec(),
        );
        host_later.http().enqueue(br#"{"models":{}}"#.to_vec());
        block_on(probe.probe(
            host_later.context(),
            &endpoints(),
            &account,
            &access,
            "proj-1",
        ))
        .expect("probe later");
        let named: usize = host_later
            .http()
            .calls()
            .iter()
            .filter(|call| call.uri() == method_url("fetchAvailableModels"))
            .filter_map(|call| json::from_slice::<Value>(call.body()).ok())
            .filter(|body| {
                body.get("entitlement")
                    .and_then(|value| value.get("userTier"))
                    .and_then(Value::as_str)
                    == Some("g1-pro-tier")
            })
            .count();
        assert_eq!(named, 1, "a newly entitled tier is asked again");
    }

    #[test]
    fn r49_non_400_tier_failures_are_not_rested() {
        let dir = tempfile::tempdir().expect("dir");
        let host = test_host(&dir, SystemTime::UNIX_EPOCH);
        let probe = TierProbe::default();
        let account = AccountId::from_static("a-0000000000000001");
        let blob = SecretObject::new()
            .with_number("v", 1)
            .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
            .with_text(project::PROJECT, "proj-1")
            .with_text(project::PAID, "g1-pro-tier");
        block_on(async {
            let accounts = host.context().accounts();
            let row = accounts
                .account(account.clone(), "u@example.test")
                .expect("account");
            accounts
                .upsert(row, blob.to_secret())
                .await
                .expect("upsert");
        });
        host.http()
            .enqueue_response(503, &[], br#"{"error":{"message":"unavailable"}}"#.to_vec());
        host.http().enqueue(br#"{"models":{}}"#.to_vec());
        let access = SecretString::from("access-example".to_owned());
        block_on(probe.probe(host.context(), &endpoints(), &account, &access, "proj-1"))
            .expect("probe");
        host.http()
            .enqueue_response(503, &[], br#"{"error":{"message":"unavailable"}}"#.to_vec());
        host.http().enqueue(br#"{"models":{}}"#.to_vec());
        block_on(probe.probe(host.context(), &endpoints(), &account, &access, "proj-1"))
            .expect("probe again");
        let named: usize = host
            .http()
            .calls()
            .iter()
            .filter(|call| call.uri() == method_url("fetchAvailableModels"))
            .filter_map(|call| json::from_slice::<Value>(call.body()).ok())
            .filter(|body| body.get("entitlement").is_some())
            .count();
        assert_eq!(named, 2, "only HTTP 400 rests a tier");
    }
}
