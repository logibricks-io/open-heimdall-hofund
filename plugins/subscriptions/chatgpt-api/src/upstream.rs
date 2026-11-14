//! OpenAI Responses upstream for ChatGPT API accounts.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::errors;
use crate::shape;
use crate::tokens::{self, Need};

pub(crate) struct ChatgptApiUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl ChatgptApiUpstream {
    pub(crate) const fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn send(
        &self,
        access: &tokens::Access,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, self.endpoints.api.clone())
            .credential(CredentialStyle::Bearer)
            .rewrite(Arc::new(shape::body))
            .classify_status(Arc::new(|status, _headers, body, error| {
                errors::classify(status, body, error)
            }));
        let mut request = request;
        let unary = !request.stream;
        request.stream = true;
        let sent = wire::send(
            self.host.http(),
            &target,
            Some(&access.token),
            request,
            inbound,
        )
        .await?;
        if unary {
            sent.fold().await
        } else {
            sent.complete().await
        }
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ChatgptApiUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let access =
            tokens::access(&self.host, &self.endpoints, &self.account, Need::IfExpiring).await?;
        match self.send(&access, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                let renewed = tokens::access(
                    &self.host,
                    &self.endpoints,
                    &self.account,
                    Need::Replace(access.token),
                )
                .await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}
