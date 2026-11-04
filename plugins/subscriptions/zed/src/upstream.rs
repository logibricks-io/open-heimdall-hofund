//! Zed cloud upstream is not wired until the provider envelope is verified offline.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    UpstreamProvider,
};

use crate::Endpoints;

pub(crate) struct ZedUpstream {
    _account: AccountId,
    _host: HostContext,
    _endpoints: Arc<Endpoints>,
}

impl ZedUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            _account: account,
            _host: host,
            _endpoints: endpoints,
        }
    }
}

const UNSUPPORTED: &str =
    "Zed cloud completions need a verified provider envelope; upstream is not enabled yet";

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ZedUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Err(PluginError::unsupported(UNSUPPORTED))
    }

    async fn probe(&self, _query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        Err(PluginError::unsupported(UNSUPPORTED))
    }
}
