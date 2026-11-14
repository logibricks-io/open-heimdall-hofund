//! Factory chat upstream routed per model wire.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::HeaderMap;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::url::Url;
use hofund_plugin_sdk::wire::{self, StatusClassify, Target};

use crate::Endpoints;
use crate::client;
use crate::models;
use crate::tokens::{self, Bearer};

pub(crate) struct FactoryUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl FactoryUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn bearer(&self) -> Result<Bearer, PluginError> {
        tokens::bearer(&self.host, &self.endpoints, &self.account).await
    }

    async fn send(
        &self,
        bearer: &Bearer,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let (wire, path) = models::wire_target(&request.model);
        let api_base = llm_base(&self.endpoints, bearer)?;
        let mut headers = client::chat_headers();
        headers.extend(models::outbound_llm_headers(&request.model));
        headers.push((
            "x-provider-routing-source".to_owned(),
            "registry_default".to_owned(),
        ));
        if models::x_api_provider(&request.model) == Some("openai") {
            headers.push((
                "openai-platform".to_owned(),
                "org-bHuLtG1fGmYk5YaOihAAXFBw".to_owned(),
            ));
        }
        if wire == ProtocolId::ANTHROPIC_MESSAGES {
            headers.push(("x-api-key".to_owned(), "placeholder".to_owned()));
        }
        if let Some(org) = &bearer.active_org {
            headers.push(("x-factory-org-id".to_owned(), org.clone()));
        }
        let target = Target::new(wire, format!("{api_base}{path}"))
            .credential(CredentialStyle::Bearer)
            .headers(headers)
            .generate_in_body(wire == ProtocolId::GENERATIVE_LANGUAGE)
            .rewrite(models::request_rewrite(wire, &request.model))
            .classify_status(classify_status());
        wire::execute(
            self.host.http(),
            &target,
            Some(&bearer.token),
            request,
            inbound,
        )
        .await
    }
}

fn classify_status() -> StatusClassify {
    Arc::new(|status, headers, body, error| {
        if !content_refused(status, headers, body) {
            return error;
        }
        PluginError::status_error(
            400,
            format!(
                "{} — Factory's usage policy check reads the whole conversation, so this session's requests are refused again until the text that tripped it is gone: start a new session, or remove or reword what the conversation quotes (another agent's system prompt or instructions pasted in a message or a tool's output are the usual cause)",
                error.message()
            ),
        )
        .without_cooldown()
        .skip_failover_same_plugin()
    })
}

fn content_refused(status: u16, headers: &HeaderMap, body: &[u8]) -> bool {
    if status != 403 || headers.contains_key("x-factory-upstream-error") {
        return false;
    }
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(body) else {
        return false;
    };
    value.get("code").and_then(Value::as_str) == Some("content_policy_rejected")
        || value
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            == Some("content_policy_rejected")
}

fn llm_base(endpoints: &Endpoints, bearer: &Bearer) -> Result<String, PluginError> {
    let Some(host) = bearer
        .prem_base_host
        .as_deref()
        .filter(|host| !host.is_empty())
    else {
        return Ok(endpoints
            .api_base_for_region(bearer.region.as_deref())
            .to_owned());
    };
    normalize_prem_host(host)
}

fn normalize_prem_host(host: &str) -> Result<String, PluginError> {
    let invalid = || PluginError::config("Factory organization host is not a valid HTTPS host");
    if host.trim() != host || host.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        return Err(invalid());
    }
    let absolute = if host.contains("://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    let url = Url::parse(&absolute).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for FactoryUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let bearer = self.bearer().await?;
        match self.send(&bearer, request.clone(), inbound).await {
            Err(error) if bearer.oauth && error.status() == Some(401) => {
                tokens::refresh_account(&self.host, &self.endpoints, &self.account).await?;
                let renewed = self.bearer().await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_prem_host;

    #[test]
    fn r52_prem_hosts_normalize_and_invalid_hosts_fail_closed() {
        assert_eq!(
            normalize_prem_host("prem.factory.test///").expect("host"),
            "https://prem.factory.test"
        );
        assert_eq!(
            normalize_prem_host("https://Prem.Factory.test/").expect("host"),
            "https://prem.factory.test"
        );
        for host in [
            "http://prem.factory.test",
            "https://user@prem.factory.test",
            "https://prem.factory.test/path",
            "https://prem.factory.test/?query=yes",
            "https://prem.factory.test/#fragment",
            "prem.factory.test path",
            "///",
        ] {
            assert!(normalize_prem_host(host).is_err(), "{host} accepted");
        }
    }
}
