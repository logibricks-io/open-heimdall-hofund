//! OpenAI Chat upstream on the Qoder API base.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::creds::Stored;
use crate::refresh;

pub(crate) struct QoderUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl QoderUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn stored(&self, force: bool) -> Result<Stored, PluginError> {
        if force {
            refresh::run(&self.host, Arc::clone(&self.endpoints), &self.account).await?;
        }
        refresh::ensure_fresh(&self.host, Arc::clone(&self.endpoints), &self.account).await
    }

    async fn send(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let base = format!(
            "{}/chat/completions",
            self.endpoints.chat_for(stored.credential.site)
        );
        let target = Target::new(ProtocolId::OPENAI_CHAT, base).credential(CredentialStyle::Bearer);
        wire::execute(
            self.host.http(),
            &target,
            Some(&stored.credential.token),
            request,
            inbound,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for QoderUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = self.stored(false).await?;
        match self.send(&stored, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                let renewed = self.stored(true).await?;
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
