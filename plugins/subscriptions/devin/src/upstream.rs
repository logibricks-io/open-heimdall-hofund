//! Devin chat upstream on the OpenAI-compatible API.

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::wire::{self, Target};

use crate::creds::Stored;

pub(crate) struct DevinUpstream {
    account: AccountId,
    host: HostContext,
}

impl DevinUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext) -> Self {
        Self { account, host }
    }

    async fn stored(&self) -> Result<Stored, PluginError> {
        Stored::load(&self.host, &self.account).await
    }

    async fn send(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let base = stored.credential.devin_api.trim_end_matches('/');
        let target = Target::new(ProtocolId::OPENAI_CHAT, format!("{base}/v1"))
            .credential(CredentialStyle::Bearer)
            .headers(vec![(
                "authorization".to_owned(),
                format!("Bearer {}", secret_text(&stored.credential.key)),
            )]);
        wire::execute(
            self.host.http(),
            &target,
            Some(&stored.credential.key),
            request,
            inbound,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for DevinUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = self.stored().await?;
        self.send(&stored, request, inbound).await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}
