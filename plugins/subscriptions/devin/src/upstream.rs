//! Devin chat upstream.

use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    UpstreamProvider,
};

use crate::creds::Stored;

pub(crate) struct DevinUpstream {
    account: AccountId,
    host: HostContext,
}

impl DevinUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext) -> Self {
        Self { account, host }
    }

    fn unsupported() -> PluginError {
        PluginError::config(
            "Devin subscription chat is not implemented; the reference has no verified Devin chat API path",
        )
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for DevinUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let _stored = Stored::load(&self.host, &self.account).await?;
        Err(Self::unsupported())
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let _stored = Stored::load(&self.host, &self.account).await?;
        let _ = query;
        Err(Self::unsupported())
    }
}
