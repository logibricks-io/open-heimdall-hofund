//! Mint short-lived model tokens from Zed cloud.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::Endpoints;
use crate::tokens;

pub(crate) async fn model_token(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<String, PluginError> {
    let stored = crate::creds::Stored::load(host, account).await?;
    tokens::model_token(host, endpoints, &stored, false).await
}

pub(crate) async fn run(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let _ = model_token(host, endpoints, account).await?;
    Ok(())
}
