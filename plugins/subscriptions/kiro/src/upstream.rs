//! Kiro chat upstream on the Anthropic Messages API.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::wire::{self, Target};

use crate::creds::Stored;
use crate::{Endpoints, refresh};

pub(crate) struct KiroUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl KiroUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn stored(&self) -> Result<Stored, PluginError> {
        refresh::ensure_fresh(&self.host, &self.account, Arc::clone(&self.endpoints)).await
    }

    async fn send(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let base = self.endpoints.chat_base(&stored.credential.region);
        let mut headers = vec![(
            "authorization".to_owned(),
            format!("Bearer {}", secret_text(&stored.credential.access)),
        )];
        if !stored.credential.profile_arn.is_empty() {
            headers.push((
                "x-amzn-codewhisperer-token".to_owned(),
                stored.credential.profile_arn.clone(),
            ));
        }
        let target = Target::new(ProtocolId::ANTHROPIC_MESSAGES, base)
            .credential(CredentialStyle::Bearer)
            .headers(headers);
        wire::execute(
            self.host.http(),
            &target,
            Some(&stored.credential.access),
            request,
            inbound,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for KiroUpstream {
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
