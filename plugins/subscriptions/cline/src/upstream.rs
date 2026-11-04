//! Cline chat completions as an OpenAI Chat upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::client;
use crate::tokens::{self, Bearer};

pub(crate) struct ClineUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl ClineUpstream {
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
        let target = Target::new(ProtocolId::OPENAI_CHAT, self.endpoints.api.clone())
            .credential(CredentialStyle::Bearer)
            .headers(client::headers(None))
            .unwrap(Arc::new(unwrap_envelope));
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

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ClineUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let bearer = self.bearer().await?;
        match self.send(&bearer, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
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

/// Lifts `{"data": {"choices": …}}` to the inner completion object.
fn unwrap_envelope(value: Value) -> Value {
    let Value::Object(map) = value else {
        return value;
    };
    let Some(data) = map.get("data") else {
        return Value::Object(map);
    };
    if data.get("choices").and_then(Value::as_array).is_some() {
        return data.clone();
    }
    Value::Object(map)
}
