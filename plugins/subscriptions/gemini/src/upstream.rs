//! Code Assist's `v1internal` as a Generative Language upstream, sent as
//! Gemini CLI sends it: the request wrapped with the model, the project and
//! a prompt id, and each reply lifted out of its `response` envelope.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, CredentialStyle, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{TokenCache, new_state};
use hofund_plugin_sdk::wire::{self, Target};

use crate::{Endpoints, project, tokens};

/// Code Assist's method root; calls go to `{code_assist}/v1internal:{method}`.
const ROOT: &str = "v1internal";

pub(crate) struct GeminiUpstream {
    pub(crate) account: AccountId,
    pub(crate) host: HostContext,
    pub(crate) endpoints: Arc<Endpoints>,
    pub(crate) tokens: Arc<TokenCache<SecretString>>,
    /// The project the config names, if any.
    pub(crate) project: Option<String>,
}

impl GeminiUpstream {
    async fn send(
        &self,
        access: &SecretString,
        project: &str,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let model = request.model.clone();
        let prompt = new_state()?;
        let envelope_project = project.to_owned();
        let envelope_model = model.clone();
        let target = Target::new(
            ProtocolId::GENERATIVE_LANGUAGE,
            self.endpoints.code_assist.clone(),
        )
        .credential(CredentialStyle::Bearer)
        .method_root(ROOT)
        .headers(headers(&model))
        .rewrite(Arc::new(move |body| {
            envelope(body, bare(&envelope_model), &envelope_project, &prompt);
            Vec::new()
        }))
        .unwrap(Arc::new(lift));
        wire::execute(self.host.http(), &target, Some(access), request, inbound).await
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for GeminiUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let access =
            tokens::access(&self.host, &self.endpoints, &self.tokens, &self.account).await?;
        let project = project::resolve(
            &self.host,
            &self.endpoints,
            &self.account,
            &access,
            self.project.as_deref(),
        )
        .await?;
        match self.send(&access, &project, request.clone(), inbound).await {
            Err(error) if error.status() == Some(401) => {
                // Google refused a token it issued: drop it, refresh once,
                // and send again.
                self.tokens.invalidate_if(&self.account, |_| true).await;
                let renewed =
                    tokens::access(&self.host, &self.endpoints, &self.tokens, &self.account)
                        .await?;
                self.send(&renewed, &project, request, inbound).await
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

/// The headers Gemini CLI adds: its user agent, naming `model`.
pub(crate) fn headers(model: &str) -> Vec<(String, String)> {
    vec![(
        "user-agent".to_owned(),
        project::user_agent(Some(bare(model))),
    )]
}

/// The model without a leading `models/`, as Code Assist names it.
pub(crate) fn bare(model: &str) -> &str {
    let model = model.trim();
    model.strip_prefix("models/").unwrap_or(model)
}

/// Wraps a Generative Language body as Code Assist takes it:
/// `{model, project, user_prompt_id, request: body}`.
pub(crate) fn envelope(body: &mut Map<String, Value>, model: &str, project: &str, prompt: &str) {
    let mut request = std::mem::take(body);
    // Code Assist reads the model from the envelope only.
    request.remove("model");
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("project".to_owned(), Value::String(project.to_owned()));
    body.insert(
        "user_prompt_id".to_owned(),
        Value::String(prompt.to_owned()),
    );
    body.insert("request".to_owned(), Value::Object(request));
}

/// Lifts a reply out of Code Assist's `{response: ...}` envelope; anything
/// else passes as it is.
pub(crate) fn lift(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.contains_key("response") => {
            map.remove("response").unwrap_or_default()
        }
        other => other,
    }
}
