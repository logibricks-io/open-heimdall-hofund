//! OpenAI Chat upstream with WorkBuddy client headers.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::Endpoints;
use crate::creds;
use crate::refresh;

pub(crate) struct WorkBuddyUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl WorkBuddyUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn stored(&self, force: bool) -> Result<creds::Stored, PluginError> {
        if force && !self.stored_is_desktop().await? {
            refresh::force(&self.host, Arc::clone(&self.endpoints), &self.account).await?;
        }
        refresh::ensure_fresh(&self.host, Arc::clone(&self.endpoints), &self.account).await
    }

    async fn stored_is_desktop(&self) -> Result<bool, PluginError> {
        Ok(creds::Stored::load(&self.host, &self.account)
            .await?
            .credential
            .desktop)
    }

    async fn send(
        &self,
        stored: &creds::Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let base = self.endpoints.base_for(stored.credential.site);
        let url = format!("{base}/v1/chat/completions");
        let header_rows: Vec<(String, String)> = creds::bearer_headers(&stored.credential)
            .into_iter()
            .filter(|(name, _)| name != "Authorization")
            .collect();
        let target = Target::new(ProtocolId::OPENAI_CHAT, url)
            .credential(CredentialStyle::Bearer)
            .headers(header_rows);
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
impl UpstreamProvider for WorkBuddyUpstream {
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
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}
