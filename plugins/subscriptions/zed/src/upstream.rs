//! POST `/completions` and decode the provider event stream.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::transport::HttpResponse;
use hofund_plugin_sdk::wire;

use crate::Endpoints;
use crate::body;
use crate::creds::Stored;
use crate::errors;
use crate::headers;
use crate::models;
use crate::stream;
use crate::thread;
use crate::tokens;

pub(crate) struct ZedUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
}

impl ZedUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext, endpoints: Arc<Endpoints>) -> Self {
        Self {
            account,
            host,
            endpoints,
        }
    }

    async fn execute_inner(
        &self,
        mut stored: Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        body::wire_for(inbound).ok_or_else(|| {
            PluginError::unsupported(format!(
                "protocol `{}` is not served through Zed cloud",
                inbound.as_str()
            ))
        })?;
        let model = request.model.clone();
        let vendor = models::vendor_of(&stored, &model);
        let wire = body::wire_for_vendor(&vendor).ok_or_else(|| {
            PluginError::unsupported(format!(
                "provider `{vendor}` is not served through Zed cloud"
            ))
        })?;
        let caps = models::capabilities(&stored, &model);
        let outbound = wire.outbound();
        let encoded = wire::encode_outbound(outbound, inbound, request.clone())?;
        let mut root: Map<String, Value> = hofund_plugin_sdk::json::from_slice(&encoded)
            .map_err(|_| PluginError::decode("the request could not be encoded for Zed"))?;
        body::prepare_provider(wire, &mut root, &model, caps);
        let (thread_id, prompt_id) = thread::thread_fields(&request, &stored.credential.user_id);
        let mut envelope = Map::new();
        envelope.insert("provider".to_owned(), Value::String(vendor));
        envelope.insert("model".to_owned(), Value::String(model));
        envelope.insert("provider_request".to_owned(), Value::Object(root));
        if let Some(thread_id) = thread_id {
            envelope.insert("thread_id".to_owned(), Value::String(thread_id));
        }
        if let Some(prompt_id) = prompt_id {
            envelope.insert("prompt_id".to_owned(), Value::String(prompt_id));
        }
        let body = Value::Object(envelope).to_string();
        let mut renewed = false;
        let mut moved = false;
        loop {
            let token =
                tokens::model_token(&self.host, Arc::clone(&self.endpoints), &stored, renewed)
                    .await?;
            let url = format!("{}/completions", self.endpoints.cloud);
            let mut call = Call::post(url)
                .header("Content-Type", "application/json")
                .sensitive_header("authorization", format!("Bearer {token}"));
            for (name, value) in headers::model_token_headers() {
                call = call.header(name, value);
            }
            for (name, value) in headers::completion_headers() {
                call = call.header(name, value);
            }
            let response = call
                .bytes(body.clone().into_bytes())
                .open_raw(self.host.http(), 2)
                .await?;
            let status = response.status().as_u16();
            let header_rows: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or("").to_owned(),
                    )
                })
                .collect();
            if !renewed && errors::token_stale(status, &header_rows) {
                renewed = true;
                continue;
            }
            if status == 402 && !moved {
                moved = true;
                if models::repick_and_apply(
                    &self.host,
                    Arc::clone(&self.endpoints),
                    &self.account,
                    &mut stored,
                )
                .await?
                {
                    continue;
                }
            }
            if !(200..300).contains(&status) {
                let text = String::from_utf8_lossy(&read_body(response).await?).into_owned();
                let (mapped, message) = errors::refusal(status, &text);
                return Err(errors::map_http(mapped, message));
            }
            let wrapped = response
                .headers()
                .get("x-zed-server-supports-status-messages")
                .is_some();
            let map_detail = |detail: String| -> PluginError {
                if let Some((status, message)) = detail.split_once(':')
                    && let Ok(code) = status.parse::<u16>()
                {
                    return errors::map_http(code, message);
                }
                PluginError::status_error(502, detail)
            };
            let sse_body = if request.stream {
                stream::ndjson_to_sse_stream(response.into_body(), wire, wrapped)
                    .await
                    .map_err(map_detail)?
            } else {
                let raw = read_body(response).await?;
                stream::ndjson_to_sse_body(&raw, wire, wrapped, false).map_err(map_detail)?
            };
            let http = HttpResponse::new(sse_body);
            return wire::decode_upstream_response(http, outbound, inbound, request.stream).await;
        }
    }
}

async fn read_body(response: HttpResponse) -> Result<Vec<u8>, PluginError> {
    let mut body = response.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for ZedUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = Stored::load(&self.host, &self.account).await?;
        self.execute_inner(stored, request, inbound).await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let mut request = query.minimal_request();
        request.stream = true;
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}
