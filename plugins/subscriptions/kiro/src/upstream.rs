//! Kiro chat upstream on `generateAssistantResponse`.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, HostContext, Message, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, Response, Role, Stop, StopReason, UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::creds::Stored;
use crate::refresh;
use crate::{Endpoints, kiro_body, kiro_stream};

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
        let region = stored.credential.region.as_str();
        let base = self.endpoints.chat_base(region);
        let url = format!("{base}/generateAssistantResponse");
        let body = kiro_body::encode(&request, &stored.credential)?;
        let mut call = Call::post(url)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("x-amzn-codewhisperer-optout", "true")
            .sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&stored.credential.access)),
            );
        if stored.credential.method == "apikey" {
            call = call.header("tokentype", "API_KEY");
        } else if stored.credential.method == "external-idp" {
            call = call.header("tokentype", "EXTERNAL_IDP");
        }
        if !stored.credential.profile_arn.is_empty() {
            call = call.header("x-amz-codewhisperer-token", &stored.credential.profile_arn);
        }
        let reply = call.bytes(body).fetch(self.host.http()).await?;
        if reply.status() == 401 || reply.status() == 403 {
            return Err(PluginError::auth(
                "Kiro refused the sign-in for this request",
            ));
        }
        if !(200..300).contains(&reply.status()) {
            return Err(PluginError::status_error(
                reply.status(),
                "Kiro did not complete the request",
            ));
        }
        let text =
            decode_json(reply.body()).or_else(|_| kiro_stream::assistant_text(reply.body()))?;
        let response = anthropic_response(&request.model, &text);
        if inbound == ProtocolId::ANTHROPIC_MESSAGES {
            return Ok(Completion::Unary(response));
        }
        Err(PluginError::config(
            "Kiro upstream only serves Anthropic Messages on this path",
        ))
    }
}

fn decode_json(body: &[u8]) -> Result<String, PluginError> {
    let value: Value =
        json::from_slice(body).map_err(|_| PluginError::decode("Kiro's reply is not JSON"))?;
    value
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PluginError::decode("Kiro's JSON reply has no content"))
}

fn anthropic_response(model: &str, text: &str) -> Box<Response> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text(text));
    Box::new(
        Response::new(message)
            .with_id(Some("msg_kiro_probe".to_owned()))
            .with_model(Some(model.to_owned()))
            .with_stop(Some(Stop {
                reason: StopReason::EndTurn,
                sequence: None,
            })),
    )
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
