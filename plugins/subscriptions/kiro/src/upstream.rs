//! Kiro chat upstream on `generateAssistantResponse`.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, BodyStream, Completion, ContentPart, HostContext, PluginError, ProbeQuery,
    ProbeReport, ProtocolId, Request, Response, UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::creds::{self, Stored};
use crate::refresh;
use crate::{Endpoints, kiro_body, kiro_stream};

const ERROR_BODY_CAP: usize = 1024 * 1024;

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

    async fn send(&self, stored: &Stored, request: Request) -> Result<Completion, PluginError> {
        let profile = creds::effective_profile_arn(&stored.credential);
        let base = self.endpoints.chat_base(profile, &stored.credential.region);
        let url = format!("{base}/generateAssistantResponse");
        let names = kiro_body::tool_names(&request);
        let body = kiro_body::encode(&request, &stored.credential)?;
        let mut call = Call::post(url)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .sensitive_header(
                "authorization",
                format!("Bearer {}", secret_text(&stored.credential.access)),
            );
        if stored.credential.method == "apikey" {
            call = call.header("tokentype", "API_KEY");
        } else if stored.credential.method == "external-idp" {
            call = call.header("tokentype", "EXTERNAL_IDP");
        }
        let reply = call.bytes(body).open_raw(self.host.http(), 1).await?;
        let status = reply.status().as_u16();
        if status == 401 || status == 403 {
            return Err(PluginError::auth(
                "Kiro refused the sign-in for this request",
            ));
        }
        if !(200..300).contains(&status) {
            let body = read_capped(reply.into_body(), ERROR_BODY_CAP).await?;
            let detail = String::from_utf8_lossy(&body);
            return Err(PluginError::status_error(
                status,
                if detail.trim().is_empty() {
                    "Kiro did not complete the request".to_owned()
                } else {
                    format!("Kiro did not complete the request: {}", detail.trim())
                },
            ));
        }
        if request.stream {
            return Ok(kiro_stream::stream(reply.into_body(), request.model, names));
        }
        let body = read_all(reply.into_body()).await?;
        let response = kiro_stream::unary(&body, &request.model, &names)
            .or_else(|_| decode_json(&body, &request.model))?;
        Ok(Completion::Unary(response))
    }
}

async fn read_all(mut body: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        out.extend_from_slice(&chunk.map_err(|error| {
            PluginError::decode(format!("Kiro's reply broke off: {}", error.message()))
        })?);
    }
    Ok(out)
}

async fn read_capped(mut body: BodyStream, cap: usize) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while out.len() < cap {
        let Some(chunk) = body.next_chunk().await else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            PluginError::decode(format!("Kiro's reply broke off: {}", error.message()))
        })?;
        let remaining = cap - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(out)
}

fn decode_json(body: &[u8], model: &str) -> Result<Box<Response>, PluginError> {
    let value: Value =
        json::from_slice(body).map_err(|_| PluginError::decode("Kiro's reply is not JSON"))?;
    let text = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::decode("Kiro's JSON reply has no content"))?;
    let mut message = hofund_plugin_api::Message::new(hofund_plugin_api::Role::Assistant);
    message.content.push(ContentPart::text(text));
    Ok(Box::new(
        Response::new(message)
            .with_id(Some("msg_kiro_probe".to_owned()))
            .with_model(Some(model.to_owned()))
            .with_stop(Some(hofund_plugin_api::Stop {
                reason: hofund_plugin_api::StopReason::EndTurn,
                sequence: None,
            })),
    ))
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for KiroUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        if inbound != ProtocolId::ANTHROPIC_MESSAGES {
            return Err(PluginError::config(
                "Kiro upstream only serves Anthropic Messages on this path",
            ));
        }
        kiro_body::reject_unsupported(&request)?;
        let stored = self.stored().await?;
        self.send(&stored, request).await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }
}
