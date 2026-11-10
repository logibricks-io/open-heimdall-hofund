//! Agent host discovery and its token-scoped cache.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use hofund_plugin_api::{HostContext, SecretString};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::url::Url;
use hofund_plugin_sdk::{Call, timeout};

use crate::endpoints::Endpoints;
use crate::proto;

const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct Cached {
    url: String,
    listed: bool,
    read_at: Duration,
}

/// Resolves and caches the agent origin without retaining token text.
#[derive(Default)]
pub(crate) struct AgentHost {
    cache: Mutex<HashMap<String, Cached>>,
}

impl AgentHost {
    /// Returns a listed agent URL, or the configured default when config cannot be read.
    pub(crate) async fn resolve(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        token: &SecretString,
        fresh: bool,
        team_id: Option<&str>,
    ) -> String {
        let key = proto::to_hex(&sha256(secret_text(token).as_bytes()));
        let now = host.clock().monotonic();
        if !fresh {
            let cached = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned();
            if let Some(cached) = cached
                && (cached.listed || now.saturating_sub(cached.read_at) < FALLBACK_TTL)
            {
                return cached.url;
            }
        }

        let listed = fetch(host, endpoints, token, team_id).await;
        let url = listed
            .clone()
            .unwrap_or_else(|| endpoints.agent.trim_end_matches('/').to_owned());
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                Cached {
                    url: url.clone(),
                    listed: listed.is_some(),
                    read_at: host.clock().monotonic(),
                },
            );
        url
    }
}

async fn fetch(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    team_id: Option<&str>,
) -> Option<String> {
    let mut call = Call::post(endpoints.server_config())
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .header("user-agent", "")
        .sensitive_header("authorization", format!("Bearer {}", secret_text(token)))
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .header("x-ghost-mode", "true")
        .bytes(b"{}");
    if let Some(team_id) = team_id.filter(|value| !value.is_empty()) {
        call = call.header("x-cursor-team-id", team_id);
    }
    let reply = timeout(CONFIG_TIMEOUT, call.fetch(host.http()))
        .await?
        .ok()?;
    if !(200..300).contains(&reply.status()) {
        return None;
    }
    let body = hofund_plugin_sdk::json::from_slice::<Value>(reply.body()).ok()?;
    let config = body.get("agentUrlConfig")?;
    ["agentUrl", "agentnUrl"]
        .into_iter()
        .filter_map(|name| config.get(name).and_then(Value::as_str))
        .find_map(valid_agent_url)
}

fn valid_agent_url(value: &str) -> Option<String> {
    let parsed = Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return None;
    }
    Some(value.trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_kit_subscription::{TestHost, advance_time, block_on, block_on_paused};
    use hofund_plugin_api::{PluginId, PluginKind, SecretString};

    use super::AgentHost;
    use crate::endpoints::Endpoints;

    fn host() -> TestHost {
        TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("cursor"),
            tempfile::tempdir().expect("tempdir").keep(),
        )
    }

    fn endpoints() -> Endpoints {
        Endpoints {
            api: "https://api.test".to_owned(),
            agent: "https://fallback.test".to_owned(),
            ..Endpoints::default()
        }
    }

    fn token(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    #[test]
    fn failed_config_uses_the_default_and_records_the_connect_json_request() {
        block_on(async {
            let host = host();
            host.http().enqueue_connect("unreadable");
            let resolved = AgentHost::default()
                .resolve(host.context(), &endpoints(), &token("dummy-a"), false, None)
                .await;
            assert_eq!(resolved, "https://fallback.test");
            let calls = host.http().calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].uri(),
                "https://api.test/aiserver.v1.ServerConfigService/GetServerConfig"
            );
            assert_eq!(calls[0].body(), b"{}");
            assert_eq!(calls[0].header("content-type"), Some("application/json"));
            assert_eq!(calls[0].header("connect-protocol-version"), Some("1"));
            assert_eq!(calls[0].header("x-cursor-client-type"), Some("cli"));
            assert_eq!(calls[0].header("x-ghost-mode"), Some("true"));
            assert_eq!(calls[0].header("x-cursor-team-id"), None);
            assert_eq!(calls[0].sensitive_headers(), ["authorization"]);
        });
    }

    #[test]
    fn listed_url_is_trimmed_and_reused_until_fresh() {
        block_on(async {
            let host = host();
            host.http().enqueue(
                br#"{"agentUrlConfig":{"agentUrl":"https://one.test///","agentnUrl":"https://two.test"}}"#
                    .to_vec(),
            );
            let resolver = AgentHost::default();
            let endpoints = endpoints();
            let token = token("dummy-a");
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, false, None)
                    .await,
                "https://one.test"
            );
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, false, Some("42"))
                    .await,
                "https://one.test"
            );
            assert_eq!(host.http().calls().len(), 1);

            host.http()
                .enqueue(br#"{"agentUrlConfig":{"agentnUrl":"https://fresh.test/"}}"#.to_vec());
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, true, Some("42"))
                    .await,
                "https://fresh.test"
            );
            let calls = host.http().calls();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1].header("x-cursor-team-id"), Some("42"));
        });
    }

    #[test]
    fn unlisted_fallback_expires_on_the_paused_monotonic_clock() {
        block_on_paused(async {
            let host = host();
            let resolver = AgentHost::default();
            let endpoints = endpoints();
            let token = token("dummy-a");
            host.http().enqueue_connect("unavailable");
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, false, None)
                    .await,
                "https://fallback.test"
            );
            host.http()
                .enqueue(br#"{"agentUrlConfig":{"agentUrl":"https://listed.test"}}"#.to_vec());
            advance_time(Duration::from_secs(59)).await;
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, false, None)
                    .await,
                "https://fallback.test"
            );
            assert_eq!(host.http().calls().len(), 1);
            advance_time(Duration::from_secs(2)).await;
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token, false, None)
                    .await,
                "https://listed.test"
            );
            assert_eq!(host.http().calls().len(), 2);
        });
    }

    #[test]
    fn cache_is_keyed_by_the_token_digest() {
        block_on(async {
            let host = host();
            let resolver = AgentHost::default();
            let endpoints = endpoints();
            host.http()
                .enqueue(br#"{"agentUrlConfig":{"agentUrl":"https://a.test"}}"#.to_vec());
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token("dummy-a"), false, None)
                    .await,
                "https://a.test"
            );
            host.http().enqueue_connect("unavailable");
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token("dummy-b"), false, None)
                    .await,
                "https://fallback.test"
            );
            assert_eq!(
                resolver
                    .resolve(host.context(), &endpoints, &token("dummy-a"), false, None)
                    .await,
                "https://a.test"
            );
            assert_eq!(host.http().calls().len(), 2);
        });
    }

    #[test]
    fn invalid_listed_urls_fall_through_to_agentn_or_the_default() {
        for (body, expected) in [
            (
                r#"{"agentUrlConfig":{"agentUrl":"file:///tmp/agent","agentnUrl":"https://n.test/"}}"#,
                "https://n.test",
            ),
            (
                r#"{"agentUrlConfig":{"agentUrl":"https://","agentnUrl":""}}"#,
                "https://fallback.test",
            ),
            (r#"{"agentUrlConfig":null}"#, "https://fallback.test"),
        ] {
            block_on(async {
                let host = host();
                host.http().enqueue(body.as_bytes().to_vec());
                assert_eq!(
                    AgentHost::default()
                        .resolve(host.context(), &endpoints(), &token("dummy"), false, None)
                        .await,
                    expected
                );
            });
        }
    }
}
