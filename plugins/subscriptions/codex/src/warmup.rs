//! One tiny Codex backend request to start an allowance window.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, HostContext, PluginError, ProbeQuery, ProtocolId, UpstreamProvider,
};

use crate::Endpoints;
use crate::models::Prompts;
use crate::upstream::CodexUpstream;

/// Sends one streamed "hi" through the Codex backend.
pub(crate) async fn run(
    endpoints: &Arc<Endpoints>,
    prompts: &Arc<Prompts>,
    account: &AccountId,
    host: &HostContext,
) -> Result<(), PluginError> {
    let upstream = CodexUpstream::new(
        account.clone(),
        host.clone(),
        Arc::clone(endpoints),
        Arc::clone(prompts),
    );
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "gpt-5-mini")?;
    let request = query.minimal_request();
    let _completion = upstream
        .execute(request, ProtocolId::OPENAI_RESPONSES)
        .await?;
    Ok(())
}
