//! Native COSY SSE upstream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    AccountId, Completion, HostContext, PluginError, ProbeQuery, ProbeReport, ProtocolId, Request,
    UpstreamProvider,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::wire;

use crate::creds::Stored;
use crate::identity::Devices;
use crate::models::{Catalog, ModelInfo};
use crate::{Endpoints, cosy, refresh, shape, stream};

const CHAT_PATH: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
const RELOGIN_GAP: Duration = Duration::from_secs(5 * 60);

pub(crate) struct QoderUpstream {
    account: AccountId,
    host: HostContext,
    endpoints: Arc<Endpoints>,
    devices: Arc<Devices>,
    catalog: Arc<Catalog>,
    renewals: Arc<Mutex<HashMap<String, SystemTime>>>,
    tasks: shape::Tasks,
}

impl QoderUpstream {
    pub(crate) fn new(
        account: AccountId,
        host: HostContext,
        endpoints: Arc<Endpoints>,
        devices: Arc<Devices>,
        catalog: Arc<Catalog>,
        renewals: Arc<Mutex<HashMap<String, SystemTime>>>,
    ) -> Self {
        Self {
            account,
            host,
            endpoints,
            devices,
            catalog,
            renewals,
            tasks: shape::Tasks::default(),
        }
    }

    async fn stored(&self, force: bool) -> Result<Stored, PluginError> {
        if force {
            return refresh::force(&self.host, Arc::clone(&self.endpoints), &self.account).await;
        }
        refresh::ensure_fresh(&self.host, Arc::clone(&self.endpoints), &self.account).await
    }

    async fn send(
        &self,
        stored: &Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let Some(model) = self
            .catalog
            .model(
                &self.host,
                Arc::clone(&self.endpoints),
                stored,
                &request.model,
            )
            .await?
        else {
            return Err(PluginError::status_error(
                400,
                format!("unknown or disabled Qoder model {:?}", request.model),
            ));
        };
        let name = model.name.trim();
        let served_name = if name.is_empty() || name == model.key {
            model.key.clone()
        } else {
            name.to_owned()
        };
        let site = self.endpoints.resolve(&stored.credential)?;
        let url = format!("{}{CHAT_PATH}", site.chat.trim_end_matches('/'));
        let streaming = request.stream;
        let encoded = wire::encode_outbound(ProtocolId::OPENAI_CHAT, inbound, request)?;
        let chat: Value = hofund_plugin_sdk::json::from_slice(&encoded)
            .map_err(|_| PluginError::decode("Qoder request shaping produced invalid JSON"))?;
        let body = self.tasks.body(&chat, &model, self.host.clock().now())?;
        let plaintext = body.value.to_string();
        let wire_body = cosy::encode(&plaintext);
        let signature = cosy::sign(
            &url,
            &stored.credential,
            &wire_body,
            self.host.clock().now(),
        )?;
        let headers = self
            .devices
            .chat_headers(&self.host, &stored.credential)
            .await;
        let mut call = Call::post(url);
        for (name, value) in headers {
            call = call.sensitive_header(name, value);
        }
        call = call
            .sensitive_header("X-Model-Key", &model.key)
            .sensitive_header("X-Model-Source", model_source(&model));
        let response = cosy::signed_call_with_accept(call, signature, "text/event-stream")
            .bytes(&wire_body)
            .open_raw(self.host.http(), 1)
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.body_mut().next_chunk().await {
                bytes.extend_from_slice(&chunk?);
                if bytes.len() > 1 << 20 {
                    break;
                }
            }
            return Err(stream::failure(status, &String::from_utf8_lossy(&bytes)));
        }
        let tools = chat
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let response = stream::transform(response, tools, served_name);
        let completion =
            wire::decode_upstream_response(response, ProtocolId::OPENAI_CHAT, inbound, streaming)
                .await?;
        self.tasks.answered(&body.task_key);
        Ok(completion)
    }

    fn recently_renewed(&self, token: &str) -> bool {
        let now = self.host.clock().now();
        let mut renewals = self
            .renewals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        renewals.retain(|_, at| now.duration_since(*at).unwrap_or_default() < RELOGIN_GAP);
        renewals.contains_key(token)
    }

    fn note_renewal(&self, stored: &Stored) {
        self.renewals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                hofund_plugin_sdk::oauth::secret_text(&stored.credential.token).to_owned(),
                self.host.clock().now(),
            );
    }
}

fn model_source(model: &ModelInfo) -> &str {
    if model.source.is_empty() {
        "system"
    } else {
        &model.source
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for QoderUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = self.stored(false).await?;
        match self.send(&stored, request.clone(), inbound).await {
            Err(error)
                if error.status() == Some(502)
                    && error.message().contains("native tool call has no name") =>
            {
                self.send(&stored, request, inbound).await
            }
            Err(error) if matches!(error.status(), Some(401 | 403)) => {
                let refused = hofund_plugin_sdk::oauth::secret_text(&stored.credential.token);
                if self.recently_renewed(refused) {
                    return Err(error);
                }
                let renewed = self.stored(true).await?;
                self.note_renewal(&renewed);
                self.send(&renewed, request, inbound).await
            }
            other => other,
        }
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let request = query.minimal_request();
        let completion = self.execute(request, query.protocol()).await?;
        ProbeReport::from_completion(completion).await
    }
}
