//! Devin chat upstream.

use std::time::Duration;

use hofund_plugin_api::{
    AccountId, Completion, ContentPart, HostContext, ImagePart, ImageSource, PluginError,
    ProbeQuery, ProbeReport, ProtocolId, Request, Role, Tool, ToolArguments, ToolChoiceMode,
    ToolResultPart, UpstreamProvider,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, uuid};

use crate::catalog::{self, os_name};
use crate::connect::chat::{
    self, ChatRequest, Prompt, PromptCall, PromptImage, PromptRole, PromptThinking, ToolDefinition,
};
use crate::creds::{self, Stored};
use crate::errors;
use crate::models::{effort_of, variant_for};
use crate::stream;

const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const ERROR_BODY_CAP: usize = 1024 * 1024;

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
        let requested_model = request.model.clone();
        let effort = effort_for(&request, inbound);
        let catalog = catalog::families_for(&self.host, &self.account, &stored).await;
        let model_uid = variant_for(&catalog.families, &requested_model, &effort);
        let tools = offered_tools(&request);
        let prompts = prompts_for(&request, &tools, inbound, &model_uid)?;
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
                encoded.images = prompt
                    .images
                    .iter()
                    .map(|image| PromptImage {
                        data: &image.data,
                        media_type: &image.media_type,
                    })
                    .collect();
                encoded.thinking = prompt.thinking.as_ref().map(|thinking| PromptThinking {
                    text: &thinking.text,
                    signature: thinking.signature.as_deref(),
                    signature_type: thinking.signature_type.as_deref(),
                });
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
            inbound == ProtocolId::ANTHROPIC_MESSAGES && is_claude(&model_uid),
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
    images: Vec<OwnedImage>,
    thinking: Option<OwnedThinking>,
}

#[derive(Clone)]
struct OwnedCall {
    id: String,
    name: String,
    arguments: String,
}

struct OwnedImage {
    data: String,
    media_type: String,
}

struct OwnedThinking {
    text: String,
    signature: Option<String>,
    signature_type: Option<String>,
}

struct JoinedMessage<'a> {
    role: Role,
    text: String,
    calls: Vec<OwnedCall>,
    results: Vec<&'a ToolResultPart>,
    content: &'a [ContentPart],
    reasoning: String,
}

struct ResultTrack {
    prompt: usize,
    id: String,
    expected: usize,
    seen: usize,
    without_note: String,
}

fn prompts_for(
    request: &Request,
    tools: &[&Tool],
    inbound: ProtocolId,
    model_uid: &str,
) -> Result<Vec<OwnedPrompt>, PluginError> {
    let mut instructions = Vec::new();
    let mut turns = Vec::new();
    let mut pending = Vec::new();
    let mut results = Vec::new();
    let claude = is_claude(model_uid);
    for message in join_split_calls(request, inbound != ProtocolId::ANTHROPIC_MESSAGES) {
        match message.role {
            Role::System | Role::Developer => {
                settle_results(&mut turns, &mut results);
                if !message.text.is_empty() {
                    instructions.push(message.text);
                }
            }
            Role::Assistant => {
                settle_results(&mut turns, &mut results);
                let next_pending = message.calls.iter().map(|call| call.id.clone()).collect();
                let own = if inbound == ProtocolId::ANTHROPIC_MESSAGES {
                    messages_prompts(&message, claude)
                } else if message.text.is_empty() && message.calls.is_empty() {
                    Vec::new()
                } else {
                    let thinking = if !claude && !message.reasoning.is_empty() {
                        Some(OwnedThinking {
                            text: message.reasoning.clone(),
                            signature: None,
                            signature_type: None,
                        })
                    } else {
                        None
                    };
                    vec![assistant_prompt(
                        message.text.clone(),
                        message.calls,
                        thinking,
                    )]
                };
                if own.is_empty() {
                    continue;
                }
                answer_pending(&mut turns, &mut pending);
                pending = next_pending;
                turns.extend(own);
            }
            Role::Tool | Role::User => {
                for result in message.results {
                    let result_id = chat::in_id(&result.id);
                    if let Some(index) = pending.iter().position(|id| id == &result_id) {
                        let text = tool_result_text(result);
                        let (expected, without_note) = noted(&text);
                        let prompt = turns.len();
                        let images = result
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                ContentPart::Image(image) => inline_image(image),
                                _ => None,
                            })
                            .collect();
                        turns.push(OwnedPrompt {
                            id: String::new(),
                            role: PromptRole::Tool,
                            text,
                            calls: Vec::new(),
                            answer: Some(pending.remove(index)),
                            images,
                            thinking: None,
                        });
                        results.push(ResultTrack {
                            prompt,
                            id: result_id,
                            expected,
                            seen: 0,
                            without_note,
                        });
                    }
                }
                if message.role == Role::User {
                    let mut texts = Vec::new();
                    let mut images = Vec::new();
                    let mut attach = None;
                    for part in message.content {
                        match part {
                            ContentPart::Text(text) => {
                                attach = result_for(&text.text, &results);
                                if attach.is_none() && !text.text.is_empty() {
                                    texts.push(text.text.as_str());
                                }
                            }
                            ContentPart::Image(part) => {
                                let image = inline_image(part);
                                if let Some(index) = attach {
                                    if let Some(image) = image {
                                        turns[results[index].prompt].images.push(image);
                                    }
                                    results[index].seen += 1;
                                    if results[index].seen == results[index].expected {
                                        attach = None;
                                    }
                                } else if let Some(image) = image {
                                    images.push(image);
                                }
                            }
                            _ => {}
                        }
                    }
                    if !texts.is_empty() || !images.is_empty() {
                        answer_pending(&mut turns, &mut pending);
                        turns.push(OwnedPrompt {
                            id: String::new(),
                            role: PromptRole::User,
                            text: texts.join("\n\n"),
                            calls: Vec::new(),
                            answer: None,
                            images,
                            thinking: None,
                        });
                    }
                    settle_results(&mut turns, &mut results);
                }
            }
        }
    }
    settle_results(&mut turns, &mut results);
    let format = structured_text(request);
    if !format.is_empty() {
        instructions.push(format);
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
            images: Vec::new(),
            thinking: None,
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
                    images: Vec::new(),
                    thinking: None,
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

fn join_split_calls(request: &Request, join: bool) -> Vec<JoinedMessage<'_>> {
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
            content: &message.content,
            reasoning: reasoning_of(&message.content),
        };
        if join
            && next.role == Role::Assistant
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
            if !next.reasoning.is_empty() {
                if !previous.reasoning.is_empty() {
                    previous.reasoning.push_str("\n\n");
                }
                previous.reasoning.push_str(&next.reasoning);
            }
            previous.calls.extend(next.calls);
            continue;
        }
        joined.push(next);
    }
    joined
}

fn reasoning_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Thinking(thinking) if !thinking.redacted && !thinking.text.is_empty() => {
                Some(thinking.text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn assistant_prompt(
    text: String,
    calls: Vec<OwnedCall>,
    thinking: Option<OwnedThinking>,
) -> OwnedPrompt {
    OwnedPrompt {
        id: String::new(),
        role: PromptRole::Assistant,
        text,
        calls,
        answer: None,
        images: Vec::new(),
        thinking,
    }
}

fn messages_prompts(message: &JoinedMessage<'_>, claude: bool) -> Vec<OwnedPrompt> {
    let mut own = Vec::new();
    let mut thought = None;
    for part in message.content {
        match part {
            ContentPart::Text(text) if !text.text.is_empty() => {
                own.push(assistant_prompt(
                    text.text.clone(),
                    Vec::new(),
                    thought.take(),
                ));
            }
            ContentPart::Thinking(thinking) if !thinking.redacted => {
                let next = if claude {
                    thinking
                        .signature
                        .as_ref()
                        .filter(|signature| !signature.is_empty())
                        .map(|signature| OwnedThinking {
                            text: thinking.text.clone(),
                            signature: Some(signature.clone()),
                            signature_type: Some(stream::sig_type_of(signature)),
                        })
                } else if !thinking.text.is_empty() {
                    Some(OwnedThinking {
                        text: thinking.text.clone(),
                        signature: None,
                        signature_type: None,
                    })
                } else {
                    None
                };
                if let Some(next) = next {
                    if thought.is_some() {
                        own.push(assistant_prompt(String::new(), Vec::new(), thought.take()));
                    }
                    thought = Some(next);
                }
            }
            _ => {}
        }
    }
    if !message.calls.is_empty() {
        if own.last().is_none_or(|prompt| prompt.thinking.is_some()) {
            own.push(assistant_prompt(String::new(), Vec::new(), None));
        }
        let last = own.last_mut().expect("a calls prompt exists");
        if last.thinking.is_none() {
            last.thinking = thought.take();
        }
        last.calls = message.calls.clone();
    } else if let Some(last) = own.last_mut()
        && last.thinking.is_none()
    {
        last.thinking = thought;
    }
    own
}

fn is_claude(uid: &str) -> bool {
    uid.to_ascii_lowercase().contains("claude")
}

fn answer_pending(turns: &mut Vec<OwnedPrompt>, pending: &mut Vec<String>) {
    for id in pending.drain(..) {
        turns.push(OwnedPrompt {
            id: String::new(),
            role: PromptRole::Tool,
            text: NO_RESULT.to_owned(),
            calls: Vec::new(),
            answer: Some(id),
            images: Vec::new(),
            thinking: None,
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
    result
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn noted(text: &str) -> (usize, String) {
    const ONE: &str = "[The tool returned an image; it follows in the next message.]";
    const MANY_END: &str = " images; they follow in the next message.]";
    if let Some(before) = text.strip_suffix(ONE) {
        return (1, without_note_separator(before).to_owned());
    }
    if let Some(end) = text.strip_suffix(MANY_END)
        && let Some(start) = end.rfind("[The tool returned ")
        && let Ok(count) = end[start + "[The tool returned ".len()..].parse::<usize>()
    {
        return (count, without_note_separator(&text[..start]).to_owned());
    }
    (0, text.to_owned())
}

fn without_note_separator(text: &str) -> &str {
    text.strip_suffix("\n\n").unwrap_or(text)
}

fn result_for(text: &str, results: &[ResultTrack]) -> Option<usize> {
    const FROM: &str = "[From the result of ";
    if !text.starts_with(FROM) {
        return None;
    }
    results.iter().position(|result| {
        result.expected > result.seen
            && (text == format!("{FROM}tool call {}:]", result.id)
                || text.ends_with(&format!(" (tool call {}):]", result.id)))
    })
}

fn inline_image(part: &ImagePart) -> Option<OwnedImage> {
    let ImageSource::Base64 { media_type, data } = &part.source else {
        return None;
    };
    if data.is_empty() {
        return None;
    }
    Some(OwnedImage {
        data: data.clone(),
        media_type: if media_type.is_empty() {
            "image/png".to_owned()
        } else {
            media_type.clone()
        },
    })
}

fn settle_results(turns: &mut [OwnedPrompt], results: &mut Vec<ResultTrack>) {
    for result in results.drain(..) {
        let prompt = &mut turns[result.prompt];
        if result.expected != 0 && result.seen == result.expected {
            prompt.text = result.without_note;
        }
        if prompt.text.is_empty() {
            prompt.text = "(no output)".to_owned();
        }
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

fn structured_text(request: &Request) -> String {
    request
        .extensions
        .get("response_format")
        .or_else(|| {
            request
                .extensions
                .get("text")
                .and_then(|value| value.get("format"))
        })
        .or_else(|| {
            request
                .extensions
                .get("output_config")
                .and_then(|value| value.get("format"))
        })
        .map(format_text)
        .unwrap_or_default()
}

fn format_text(format: &Value) -> String {
    const OBJECT: &str = "Respond with a single JSON object and nothing else.";
    match format.get("type").and_then(Value::as_str) {
        Some("json_object") => OBJECT.to_owned(),
        Some("json_schema") => {
            let schema = format
                .get("json_schema")
                .and_then(|value| value.get("schema"))
                .or_else(|| format.get("schema"));
            if schema.is_none_or(Value::is_null) {
                OBJECT.to_owned()
            } else {
                let schema = hofund_plugin_sdk::json::to_string(schema.expect("schema is present"))
                    .expect("a JSON response schema always serializes");
                format!(
                    "Respond with a single JSON value and nothing else, matching this JSON schema:\n{schema}."
                )
            }
        }
        _ => String::new(),
    }
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for DevinUpstream {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let stored = Stored::live(&self.host, &self.account).await?;
        let result = self.execute_inner(stored, request, inbound).await;
        if result
            .as_ref()
            .is_err_and(|error| error.status() == Some(401))
        {
            creds::mark_needs_sign_in(&self.host, &self.account).await?;
        }
        result
    }

    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        Stored::load(&self.host, &self.account).await?;
        let _ = query;
        Ok(ProbeReport::new("Devin credentials are ready"))
    }
}
