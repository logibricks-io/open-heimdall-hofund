//! Zed cloud as an OpenAI Responses upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::body;
use crate::refresh;

pub(crate) struct ZedUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl ZedUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn send(
        &self,
        token: &str,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let url = format!("{}/completions", self.endpoints.cloud);
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, url)
            .credential(CredentialStyle::Bearer)
            .rewrite(Arc::new(|object| {
                body::transform_map(object);
                Vec::new()
            }));
        let secret = hofund_plugin_api::SecretString::from(token.to_owned());
        wire::execute(self.host.http(), &target, Some(&secret), request, inbound).await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ZedUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let token =
            refresh::model_token(&self.host, Arc::clone(&self.endpoints), &self.account).await?;
        match self.send(&token, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                let renewed =
                    refresh::model_token(&self.host, Arc::clone(&self.endpoints), &self.account)
                        .await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let request = query.minimal_request();
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}
