//! The CLI backend as an OpenAI Responses upstream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::wire::{self, Target};

use crate::creds::Stored;
use crate::headers;
use crate::shape;
use crate::{Endpoints, refresh};

pub(crate) struct GrokUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    x_search: bool,
    cache: crate::creds::Cache,
    program: Arc<dyn Fn() -> Result<std::path::PathBuf, PluginError> + Send + Sync>,
}

impl GrokUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        x_search: bool,
        cache: crate::creds::Cache,
        program: Arc<dyn Fn() -> Result<std::path::PathBuf, PluginError> + Send + Sync>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            x_search,
            cache,
            program,
        }
    }

    async fn stored(&self, rejected: Option<&SecretString>) -> Result<Stored, PluginError> {
        let program = Arc::clone(&self.program);
        refresh::ensure_fresh(
            &self.host,
            &self.account,
            &self.cache,
            move || program(),
            rejected,
        )
        .await
    }

    async fn send(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let x_search = self.x_search;
        let (api_model, window) = shape::model_of(&request.model);
        let mut header_rows = headers::client_only();
        header_rows.push(("x-grok-model-override".to_owned(), api_model.clone()));
        if window > 0 {
            header_rows.push(("x-grok-context-window".to_owned(), window.to_string()));
        }
        if let Some(key) = request
            .params
            .extensions
            .get("prompt_cache_key")
            .and_then(|value| value.as_str())
            .filter(|key| !key.is_empty())
        {
            header_rows.push(("x-grok-conv-id".to_owned(), key.to_owned()));
        }
        for (name, value) in headers::LOOP_CHECKS {
            header_rows.push((name.to_owned(), value.to_owned()));
        }
        let model_override = api_model.clone();
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, self.endpoints.base.clone())
            .credential(CredentialStyle::Bearer)
            .headers(header_rows)
            .rewrite(Arc::new(move |body| {
                if window > 0 {
                    body.insert("model".to_owned(), Value::String(model_override.clone()));
                }
                shape::body(body, x_search);
                Vec::new()
            }));
        let mut request = request;
        if window > 0 {
            request.model = api_model;
        }
        request.stream = true;
        wire::send(
            self.host.http(),
            &target,
            Some(&stored.credential.key),
            request,
            inbound,
        )
        .await?
        .complete()
        .await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for GrokUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = self.stored(None).await?;
        match self.send(&stored, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                let renewed = self.stored(Some(&stored.credential.key)).await?;
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}
