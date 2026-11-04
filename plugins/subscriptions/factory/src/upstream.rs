//! Factory chat completions as an OpenAI Chat upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::client;
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
        let api_base = self.endpoints.api_base_for_region(bearer.region.as_deref());
        let target = Target::new(ProtocolId::OPENAI_CHAT, format!("{api_base}/api/llm/c/v1"))
            .credential(CredentialStyle::Bearer)
            .headers(client::headers());
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
impl UpstreamProvider for FactoryUpstream {
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
