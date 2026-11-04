//! Anthropic Messages upstream on the GLM plan base URL.

use std::path::PathBuf;
use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    UpstreamProvider,
};
use hofund_plugin_sdk::wire::{self, Target};

use crate::session;
use crate::{APP_VERSION, Endpoints, plan_auth_style};

pub(crate) struct ZcodeUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    device_file: PathBuf,
}

impl ZcodeUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        device_file: PathBuf,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            device_file,
        }
    }

    async fn stored(&self) -> Result<session::Stored, PluginError> {
        session::load(
            &self.host,
            &self.endpoints,
            &self.account,
            &self.device_file,
        )
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ZcodeUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        if inbound != ProtocolId::ANTHROPIC_MESSAGES {
            return Err(PluginError::config(
                "Z Code upstream only serves Anthropic Messages on this path",
            ));
        }
        let stored = self.stored().await?;
        let target = Target::new(
            ProtocolId::ANTHROPIC_MESSAGES,
            stored.auth.base.trim_end_matches('/').to_owned(),
        )
        .credential(plan_auth_style())
        .headers(vec![(
            "user-agent".to_owned(),
            format!("ZCode/{APP_VERSION}"),
        )]);
        wire::execute(
            self.host.http(),
            &target,
            Some(&stored.api_key),
            request,
            inbound,
        )
        .await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}
