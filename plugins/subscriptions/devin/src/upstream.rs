//! Devin chat upstream.

use std::time::Duration;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, Role, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, uuid};

use crate::catalog::{self, os_name};
use crate::connect::chat::{self, ChatRequest, Prompt, PromptRole};
use crate::creds::Stored;
use crate::errors;
use crate::models::{effort_of, variant_for};
use crate::stream;

const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const ERROR_BODY_CAP: usize = 1024 * 1024;
const IMAGES_MESSAGE: &str = "image inputs are not served on Devin until R-13";
const STRUCTURED_MESSAGE: &str = "structured output is not served on Devin until R-13";
const SIGNED_THINKING_MESSAGE: &str = "signed thinking history is not served on Devin until R-13";

pub(crate) struct DevinUpstream {
    account: AccountId,
    host: HostContext,
}

impl DevinUpstream {
    pub(crate) fn new(account: AccountId, host: HostContext) -> Self {
        Self { account, host }
    }

    async fn execute_inner(
        &self,
        stored: Stored,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        refuse_unserved(&request)?;
        let requested_model = request.model.clone();
        let effort = effort_for(&request, inbound);
        let catalog = catalog::families_for(&self.host, &self.account, &stored).await;
        let model_uid = variant_for(&catalog.families, &requested_model, &effort);
        let prompts = prompts_for(&request)?;
        let borrowed: Vec<_> = prompts
            .iter()
            .map(|prompt| Prompt::text(&prompt.id, prompt.role, &prompt.text))
            .collect();
        let encoded = chat::encode_request(&ChatRequest {
            session_token: secret_text(&stored.credential.key),
            os: os_name(),
            model_uid: &model_uid,
            prompts: &borrowed,
            max_tokens: request.params.max_tokens,
            temperature: request.params.temperature,
            top_p: request.params.top_p,
        })?;
        let mut call = Call::post(format!("{}{CHAT_PATH}", catalog::origin(&stored)))
            .header("Content-Type", "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .sensitive_header(
                "Authorization",
                format!("Basic {}", secret_text(&stored.credential.key)),
            )
            .read_timeout(Duration::from_secs(300))
            .bytes(encoded.body);
        if encoded.gzipped {
            call = call.header("Connect-Content-Encoding", "gzip");
        }
        let response = call.open_raw(self.host.http(), 1).await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_capped(response.into_body(), ERROR_BODY_CAP).await?;
            return Err(errors::failure(status, &body)
                .unwrap_or_else(|| PluginError::status_error(status, "chat request failed")));
        }
        stream::open(
            response.into_body(),
            requested_model,
            inbound,
            request.stream,
        )
        .await
    }
}

async fn read_capped(
    mut body: hofund_plugin_api::BodyStream,
    cap: usize,
) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while out.len() < cap {
        let Some(chunk) = body.next_chunk().await else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            PluginError::status_error(502, format!("the reply broke off: {}", error.message()))
        })?;
        let remaining = cap - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(out)
}

struct OwnedPrompt {
    id: String,
    role: PromptRole,
    text: String,
}

fn prompts_for(request: &Request) -> Result<Vec<OwnedPrompt>, PluginError> {
    let mut instructions = Vec::new();
    let mut turns: Vec<(PromptRole, String)> = Vec::new();
    for message in &request.messages {
        let text = text_of(&message.content);
        match message.role {
            Role::System | Role::Developer => {
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            Role::User => {
                if !text.is_empty() {
                    turns.push((PromptRole::User, text));
                }
            }
            Role::Assistant => {
                if !text.is_empty() {
                    turns.push((PromptRole::Assistant, text));
                }
            }
            Role::Tool => return Err(stream::tools_refusal()),
        }
    }
    if turns
        .last()
        .is_none_or(|(role, _)| *role == PromptRole::Assistant)
    {
        turns.push((PromptRole::User, "Please proceed with the task.".to_owned()));
    }
    if !instructions.is_empty()
        && let Some((_, first)) = turns.iter_mut().find(|(role, _)| *role == PromptRole::User)
    {
        let mut joined = instructions.join("\n\n");
        if !first.is_empty() {
            joined.push_str("\n\n");
            joined.push_str(first);
        }
        *first = joined;
    }
    turns
        .into_iter()
        .map(|(role, text)| {
            Ok(OwnedPrompt {
                id: uuid::v4().map_err(|error| PluginError::io(error.to_string()))?,
                role,
                text,
            })
        })
        .collect()
}

fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn effort_for(request: &Request, inbound: ProtocolId) -> String {
    if inbound != ProtocolId::ANTHROPIC_MESSAGES {
        return request
            .params
            .effort
            .map(|effort| effort_of(effort.openai_label()))
            .unwrap_or_default();
    }
    let thinking = request.extensions.get("thinking");
    let kind = thinking
        .and_then(|value| value.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if kind == "disabled" {
        return String::new();
    }
    if let Some(effort) = request
        .extensions
        .get("output_config")
        .and_then(|value| value.get("effort"))
        .and_then(|value| value.as_str())
    {
        return effort_of(effort);
    }
    if kind != "enabled" && kind != "adaptive" {
        return String::new();
    }
    let budget = thinking
        .and_then(|value| value.get("budget_tokens"))
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    match budget {
        value if value <= 0 => String::new(),
        value if value <= 4096 => "low".to_owned(),
        value if value <= 12000 => "medium".to_owned(),
        value if value <= 24000 => "high".to_owned(),
        _ => "xhigh".to_owned(),
    }
}

fn refuse_unserved(request: &Request) -> Result<(), PluginError> {
    if !request.tools.is_empty() || request.messages.iter().any(tools_in_message) {
        return Err(stream::tools_refusal());
    }
    if request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Image(_) | ContentPart::ToolResult(_)))
    }) {
        return Err(refusal(IMAGES_MESSAGE));
    }
    if request.messages.iter().any(|message| {
        message.content.iter().any(|part| {
            matches!(part, ContentPart::Thinking(thinking) if thinking.signature.is_some() || thinking.redacted || thinking.data.is_some())
        })
    }) {
        return Err(refusal(SIGNED_THINKING_MESSAGE));
    }
    if structured_format(request.extensions.get("response_format"))
        || structured_format(
            request
                .extensions
                .get("text")
                .and_then(|value| value.get("format")),
        )
        || structured_format(
            request
                .extensions
                .get("output_config")
                .and_then(|value| value.get("format")),
        )
    {
        return Err(refusal(STRUCTURED_MESSAGE));
    }
    Ok(())
}

fn tools_in_message(message: &hofund_plugin_api::Message) -> bool {
    message.role == Role::Tool
        || message.content.iter().any(|part| match part {
            ContentPart::ToolCall(_) | ContentPart::ToolResult(_) => true,
            ContentPart::Unknown(part) => part.kind.to_ascii_lowercase().contains("tool"),
            _ => false,
        })
}

fn structured_format(format: Option<&hofund_plugin_sdk::json::Value>) -> bool {
    format
        .and_then(|value| value.get("type"))
        .and_then(|value| value.as_str())
        .is_some_and(|kind| matches!(kind, "json_object" | "json_schema"))
}

fn refusal(message: &'static str) -> PluginError {
    PluginError::status_error(404, message).without_cooldown()
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for DevinUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = Stored::load(&self.host, &self.account).await?;
        self.execute_inner(stored, request, inbound).await
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        Stored::load(&self.host, &self.account).await?;
        let _ = query;
        Ok(ProbeReport::new("Devin credentials are ready"))
    }
}
