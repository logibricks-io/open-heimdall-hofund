//! Devin chat upstream.

use std::time::Duration;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, HostContext, PluginError, ProbeQuery, ProbeReport,
    ProtocolId, Request, Role, Tool, ToolArguments, ToolChoiceMode, ToolResultPart,
    UpstreamProvider,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, uuid};

use crate::catalog::{self, os_name};
use crate::connect::chat::{self, ChatRequest, Prompt, PromptCall, PromptRole, ToolDefinition};
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
        let tools = offered_tools(&request);
        let prompts = prompts_for(&request, &tools)?;
        let borrowed: Vec<_> = prompts
            .iter()
            .map(|prompt| {
                let mut encoded = Prompt::text(&prompt.id, prompt.role, &prompt.text);
                encoded.calls = prompt
                    .calls
                    .iter()
                    .map(|call| PromptCall {
                        id: &call.id,
                        name: &call.name,
                        arguments: &call.arguments,
                    })
                    .collect();
                encoded.answer = prompt.answer.as_deref();
                encoded
            })
            .collect();
        let tool_definitions: Vec<_> = tools
            .iter()
            .map(|tool| ToolDefinition {
                name: &tool.name,
                description: tool.description.as_deref(),
                schema: &tool.input_schema,
            })
            .collect();
        let encoded = chat::encode_request(&ChatRequest {
            session_token: secret_text(&stored.credential.key),
            os: os_name(),
            model_uid: &model_uid,
            prompts: &borrowed,
            tools: &tool_definitions,
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

const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";

struct OwnedPrompt {
    id: String,
    role: PromptRole,
    text: String,
    calls: Vec<OwnedCall>,
    answer: Option<String>,
}

struct OwnedCall {
    id: String,
    name: String,
    arguments: String,
}

struct JoinedMessage<'a> {
    role: Role,
    text: String,
    calls: Vec<OwnedCall>,
    results: Vec<&'a ToolResultPart>,
}

fn prompts_for(request: &Request, tools: &[&Tool]) -> Result<Vec<OwnedPrompt>, PluginError> {
    let mut instructions = Vec::new();
    let mut turns = Vec::new();
    let mut pending = Vec::new();
    for message in join_split_calls(request) {
        match message.role {
            Role::System | Role::Developer => {
                if !message.text.is_empty() {
                    instructions.push(message.text);
                }
            }
            Role::Assistant => {
                if message.text.is_empty() && message.calls.is_empty() {
                    continue;
                }
                answer_pending(&mut turns, &mut pending);
                pending = message.calls.iter().map(|call| call.id.clone()).collect();
                turns.push(OwnedPrompt {
                    id: String::new(),
                    role: PromptRole::Assistant,
                    text: message.text,
                    calls: message.calls,
                    answer: None,
                });
            }
            Role::Tool | Role::User => {
                for result in message.results {
                    let result_id = chat::in_id(&result.id);
                    if let Some(index) = pending.iter().position(|id| id == &result_id) {
                        let text = tool_result_text(result);
                        turns.push(OwnedPrompt {
                            id: String::new(),
                            role: PromptRole::Tool,
                            text,
                            calls: Vec::new(),
                            answer: Some(pending.remove(index)),
                        });
                    }
                }
                if message.role == Role::User && !message.text.is_empty() {
                    answer_pending(&mut turns, &mut pending);
                    turns.push(OwnedPrompt {
                        id: String::new(),
                        role: PromptRole::User,
                        text: message.text,
                        calls: Vec::new(),
                        answer: None,
                    });
                }
            }
        }
    }
    let descriptions = tool_descriptions(tools);
    if !descriptions.is_empty() {
        instructions.push(descriptions);
    }
    answer_pending(&mut turns, &mut pending);
    if turns
        .last()
        .is_none_or(|prompt| prompt.role == PromptRole::Assistant && prompt.calls.is_empty())
    {
        turns.push(OwnedPrompt {
            id: String::new(),
            role: PromptRole::User,
            text: "Please proceed with the task.".to_owned(),
            calls: Vec::new(),
            answer: None,
        });
    }
    if !instructions.is_empty() {
        let first = turns
            .iter_mut()
            .find(|prompt| prompt.role == PromptRole::User);
        let joined = instructions.join("\n\n");
        if let Some(first) = first {
            if !first.text.is_empty() {
                first.text.insert_str(0, "\n\n");
            }
            first.text.insert_str(0, &joined);
        } else {
            turns.insert(
                0,
                OwnedPrompt {
                    id: String::new(),
                    role: PromptRole::User,
                    text: joined,
                    calls: Vec::new(),
                    answer: None,
                },
            );
        }
    }
    turns
        .into_iter()
        .map(|mut prompt| {
            prompt.id = uuid::v4().map_err(|error| PluginError::io(error.to_string()))?;
            Ok(prompt)
        })
        .collect()
}

fn join_split_calls(request: &Request) -> Vec<JoinedMessage<'_>> {
    let mut joined: Vec<JoinedMessage<'_>> = Vec::new();
    for message in &request.messages {
        let calls = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolCall(call) => Some(OwnedCall {
                    id: chat::in_id(&call.id),
                    name: call.name.clone(),
                    arguments: args_of(&call.arguments),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let next = JoinedMessage {
            role: message.role,
            text: text_of(&message.content),
            calls,
            results: message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolResult(result) => Some(result),
                    _ => None,
                })
                .collect(),
        };
        if next.role == Role::Assistant
            && let Some(previous) = joined.last_mut()
            && previous.role == Role::Assistant
            && !previous.calls.is_empty()
        {
            if !next.text.is_empty() {
                if !previous.text.is_empty() {
                    previous.text.push_str("\n\n");
                }
                previous.text.push_str(&next.text);
            }
            previous.calls.extend(next.calls);
            continue;
        }
        joined.push(next);
    }
    joined
}

fn answer_pending(turns: &mut Vec<OwnedPrompt>, pending: &mut Vec<String>) {
    for id in pending.drain(..) {
        turns.push(OwnedPrompt {
            id: String::new(),
            role: PromptRole::Tool,
            text: NO_RESULT.to_owned(),
            calls: Vec::new(),
            answer: Some(id),
        });
    }
}

fn args_of(arguments: &ToolArguments) -> String {
    let value = match arguments {
        ToolArguments::Json { value } => value.clone(),
        ToolArguments::Raw { text } => {
            hofund_plugin_sdk::json::from_str(text).unwrap_or(Value::Null)
        }
    };
    if value.is_object() || value.is_array() {
        hofund_plugin_sdk::json::to_string(&value)
            .expect("a JSON tool argument value always serializes")
    } else {
        "{}".to_owned()
    }
}

fn tool_result_text(result: &ToolResultPart) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        "(no output)".to_owned()
    } else {
        text
    }
}

fn offered_tools(request: &Request) -> Vec<&Tool> {
    if request
        .tool_choice
        .as_ref()
        .is_some_and(|choice| choice.mode == ToolChoiceMode::None)
    {
        Vec::new()
    } else {
        request.tools.iter().collect()
    }
}

fn tool_descriptions(tools: &[&Tool]) -> String {
    let described: Vec<_> = tools
        .iter()
        .filter_map(|tool| {
            tool.description
                .as_deref()
                .filter(|description| !description.is_empty())
                .map(|description| (tool.name.as_str(), description))
        })
        .collect();
    if described.is_empty() {
        return String::new();
    }
    let body = described
        .into_iter()
        .map(|(name, description)| {
            format!("<tool name=\"{name}\">\n{}\n</tool>\n", description.trim())
        })
        .collect::<String>();
    format!("<tool_descriptions>\n{body}</tool_descriptions>")
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
    if request.messages.iter().any(|message| {
        message.content.iter().any(|part| match part {
            ContentPart::Image(_) => true,
            ContentPart::ToolResult(result) => result.content.iter().any(|part| match part {
                ContentPart::Image(_) => true,
                ContentPart::Unknown(part) => part.kind.to_ascii_lowercase().contains("image"),
                _ => false,
            }),
            ContentPart::Unknown(part) => part.kind.to_ascii_lowercase().contains("image"),
            _ => false,
        })
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
