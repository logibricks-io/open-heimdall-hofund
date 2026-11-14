//! Request bodies for Kiro `generateAssistantResponse`.

use std::collections::{HashMap, HashSet, VecDeque};

use hofund_plugin_api::{
    ContentPart, ImageSource, Message, PluginError, Request, Role, ToolArguments,
};
use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::serde::Serialize;
use hofund_plugin_sdk::{digest, uuid};

use crate::creds::Credential;

const PROCEED: &str = "Please proceed with the task.";
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";
const UNDERSTOOD: &str = "Understood.";
const RESULT_LIMIT: usize = 250_000;
pub(crate) const IMAGE_LIMIT: usize = 5 * 1024 * 1024;

#[derive(Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct Body {
    conversation_state: ConversationState,
    agent_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_model_request_fields: Option<Value>,
}

#[derive(Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct ConversationState {
    chat_trigger_type: &'static str,
    agent_task_type: &'static str,
    conversation_id: String,
    current_message: Entry,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    history: Vec<Entry>,
}

#[allow(non_snake_case)]
#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(untagged)]
enum Entry {
    User {
        userInputMessage: UserInput,
    },
    Assistant {
        assistantResponseMessage: AssistantResponse,
    },
}

impl Entry {
    fn user(&self) -> Option<&UserInput> {
        match self {
            Self::User { userInputMessage } => Some(userInputMessage),
            Self::Assistant { .. } => None,
        }
    }

    fn user_mut(&mut self) -> Option<&mut UserInput> {
        match self {
            Self::User { userInputMessage } => Some(userInputMessage),
            Self::Assistant { .. } => None,
        }
    }

    fn assistant(&self) -> Option<&AssistantResponse> {
        match self {
            Self::Assistant {
                assistantResponseMessage,
            } => Some(assistantResponseMessage),
            Self::User { .. } => None,
        }
    }

    fn assistant_mut(&mut self) -> Option<&mut AssistantResponse> {
        match self {
            Self::Assistant {
                assistantResponseMessage,
            } => Some(assistantResponseMessage),
            Self::User { .. } => None,
        }
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct UserInput {
    content: String,
    model_id: String,
    origin: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<Image>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_input_message_context: Option<UserContext>,
}

impl UserInput {
    fn new(model: &str) -> Self {
        Self {
            model_id: model.to_owned(),
            origin: "KIRO_CLI",
            ..Self::default()
        }
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct UserContext {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_results: Vec<ToolResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct ToolResult {
    tool_use_id: String,
    status: &'static str,
    content: Vec<ResultText>,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct ResultText {
    text: String,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    tool_specification: ToolSpecification,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct ToolSpecification {
    name: String,
    description: String,
    input_schema: InputSchema,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct InputSchema {
    json: Value,
}

#[derive(Clone, Default, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct AssistantResponse {
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_uses: Vec<ToolUse>,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
#[serde(rename_all = "camelCase")]
struct ToolUse {
    name: String,
    tool_use_id: String,
    input: Value,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct Image {
    format: String,
    source: ImageBytes,
}

#[derive(Clone, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct ImageBytes {
    bytes: String,
}

#[derive(Clone)]
enum CachedImage {
    Image { format: &'static str, bytes: String },
    Note(&'static str),
}

#[derive(Default)]
pub(crate) struct FittedCache {
    results: HashMap<[u8; 32], CachedImage>,
    newest: VecDeque<[u8; 32]>,
}

impl FittedCache {
    fn fit(&mut self, bytes: &str) -> CachedImage {
        let key = digest::sha256(bytes.as_bytes());
        let result = if let Some(result) = self.results.remove(&key) {
            self.newest.retain(|cached| *cached != key);
            result
        } else {
            match hofund_plugin_sdk::image::fit_base64(bytes, IMAGE_LIMIT) {
                Ok(image) => CachedImage::Image {
                    format: image.format,
                    bytes: image.base64,
                },
                Err(error) => CachedImage::Note(match error {
                    hofund_plugin_sdk::image::FitError::InputTooLarge => {
                        "it is too large to resize"
                    }
                    hofund_plugin_sdk::image::FitError::TooManyPixels => {
                        "it has too many pixels to resize"
                    }
                    hofund_plugin_sdk::image::FitError::Unreadable => {
                        "it couldn't be read as an image"
                    }
                    hofund_plugin_sdk::image::FitError::ResizeFailed => "it couldn't be resized",
                    hofund_plugin_sdk::image::FitError::StillTooLarge => {
                        "it couldn't be made small enough"
                    }
                    hofund_plugin_sdk::image::FitError::Deadline => "resizing it took too long",
                }),
            }
        };
        self.newest.push_back(key);
        self.results.insert(key, result.clone());
        while self.results.len() > 4 {
            if let Some(oldest) = self.newest.pop_front() {
                self.results.remove(&oldest);
            }
        }
        result
    }
}

pub(crate) fn reject_unsupported(_request: &Request) -> Result<(), PluginError> {
    Ok(())
}

pub(crate) fn tool_names(request: &Request) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let mut add = |name: &str| {
        let short = tool_name(name);
        if short != name {
            names.insert(short, name.to_owned());
        }
    };
    for tool in &request.tools {
        add(&tool.name);
    }
    for message in &request.messages {
        for part in &message.content {
            if let ContentPart::ToolCall(call) = part {
                add(&call.name);
            }
        }
    }
    names
}

#[cfg(test)]
pub(crate) fn encode(request: &Request, credential: &Credential) -> Result<Vec<u8>, PluginError> {
    encode_with_cache(request, credential, &mut FittedCache::default())
}

#[cfg(test)]
pub(crate) fn encode_with_cache(
    request: &Request,
    credential: &Credential,
    fitted: &mut FittedCache,
) -> Result<Vec<u8>, PluginError> {
    encode_with(request, credential, None, fitted)
}

pub(crate) fn encode_with(
    request: &Request,
    credential: &Credential,
    native: Option<&crate::models::NativeEffort>,
    fitted: &mut FittedCache,
) -> Result<Vec<u8>, PluginError> {
    let native = native_fields(request, native);
    let profile_arn = crate::creds::effective_profile_arn(credential);
    let mut entries = Vec::new();
    let mut system = String::new();
    for message in context_first(&request.messages) {
        match message.role {
            Role::System | Role::Developer => {
                for part in &message.content {
                    if let ContentPart::Text(text) = part {
                        join_into(&mut system, &text.text);
                    }
                }
            }
            Role::Assistant => encode_assistant(&message, &mut entries),
            Role::User | Role::Tool => encode_user(&message, &request.model, &mut entries)?,
        }
    }

    trim_leading_entries(&mut entries);
    if !matches!(entries.last(), Some(Entry::User { .. })) {
        entries.push(Entry::User {
            userInputMessage: UserInput::new(&request.model),
        });
    }
    pair_tool_results(&mut entries);
    keep_latest_images(&mut entries);

    if native.is_none()
        && let Some(budget) = thinking_budget(request)
    {
        let prefix = format!(
            "<thinking_mode>enabled</thinking_mode><max_thinking_length>{budget}</max_thinking_length>"
        );
        system = if system.is_empty() {
            prefix
        } else {
            format!("{prefix}\n{system}")
        };
    }
    if let Some(format) = structured_output(request) {
        system = join_non_empty(system, format);
    }
    if !system.is_empty()
        && let Some(user) = entries.first_mut().and_then(Entry::user_mut)
    {
        user.content = format!("{system}\n\n{}", user.content);
    }

    add_tools(request, &mut entries);
    fit_images(&mut entries, fitted);
    let current_message = entries.pop().expect("a current user message was added");
    let conversation_id = uuid::v4().map_err(|error| PluginError::io(error.to_string()))?;
    let body = Body {
        conversation_state: ConversationState {
            chat_trigger_type: "MANUAL",
            agent_task_type: "vibe",
            conversation_id,
            current_message,
            history: entries,
        },
        agent_mode: "vibe",
        profile_arn: (!profile_arn.is_empty()).then(|| profile_arn.to_owned()),
        additional_model_request_fields: native,
    };
    json::to_vec(&body).map_err(|_| PluginError::decode("Kiro request body could not be encoded"))
}

fn encode_assistant(message: &Message, entries: &mut Vec<Entry>) {
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text(text) if !text.text.is_empty() => texts.push(text.text.clone()),
            ContentPart::ToolCall(call) => calls.push(ToolUse {
                name: tool_name(&call.name),
                tool_use_id: tool_id(&call.id),
                input: match &call.arguments {
                    ToolArguments::Json {
                        value: value @ Value::Object(_),
                    } => value.clone(),
                    _ => Value::Object(Map::new()),
                },
            }),
            _ => {}
        }
    }
    let content = texts.join("\n\n");
    if content.is_empty() && calls.is_empty() {
        return;
    }
    if let Some(previous) = entries.last_mut().and_then(Entry::assistant_mut) {
        previous.content = join_non_empty(previous.content.clone(), content);
        previous.tool_uses.extend(calls);
    } else {
        entries.push(Entry::Assistant {
            assistantResponseMessage: AssistantResponse {
                content,
                tool_uses: calls,
            },
        });
    }
}

fn encode_user(
    message: &Message,
    model: &str,
    entries: &mut Vec<Entry>,
) -> Result<(), PluginError> {
    let mut user = UserInput::new(model);
    let mut texts = Vec::new();
    let mut results = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text(text) if !text.text.is_empty() => texts.push(text.text.clone()),
            ContentPart::Image(image) => add_image(&mut user.images, image)?,
            ContentPart::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let text = if text.is_empty() {
                    "(no output)".to_owned()
                } else {
                    cap_result(text)
                };
                results.push(ToolResult {
                    tool_use_id: tool_id(&result.id),
                    status: if result.is_error { "error" } else { "success" },
                    content: vec![ResultText { text }],
                });
                for part in &result.content {
                    if let ContentPart::Image(image) = part {
                        add_image(&mut user.images, image)?;
                    }
                }
            }
            _ => {}
        }
    }
    user.content = texts.join("\n\n");
    if !results.is_empty() {
        user.user_input_message_context = Some(UserContext {
            tool_results: results,
            tools: Vec::new(),
        });
    }
    if let Some(previous) = entries.last_mut().and_then(Entry::user_mut) {
        previous.content = join_non_empty(previous.content.clone(), user.content);
        previous.images.extend(user.images);
        if let Some(context) = user.user_input_message_context {
            previous
                .user_input_message_context
                .get_or_insert_with(UserContext::default)
                .tool_results
                .extend(context.tool_results);
        }
    } else {
        entries.push(Entry::User {
            userInputMessage: user,
        });
    }
    Ok(())
}

fn context_first(messages: &[Message]) -> Vec<Message> {
    let mut end = messages.len();
    while end > 0 && matches!(messages[end - 1].role, Role::System | Role::Developer) {
        end -= 1;
    }
    if end == messages.len() {
        return messages.to_vec();
    }
    let mut start = end;
    while start > 0 && messages[start - 1].role == Role::User {
        start -= 1;
    }
    let context = &messages[end..];
    if start == end
        || !context.iter().any(|message| {
            message.content.iter().any(|part| match part {
                ContentPart::Text(text) => !text.text.is_empty(),
                _ => true,
            })
        })
    {
        return messages.to_vec();
    }
    let turn = &messages[start..end];
    if turn.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolResult(_)))
    }) || (start > 0
        && messages[start - 1]
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_))))
    {
        return messages.to_vec();
    }
    let mut out = messages[..start].to_vec();
    out.extend(context.iter().cloned().map(|mut message| {
        message.role = Role::User;
        message
    }));
    out.push(Message {
        role: Role::Assistant,
        content: vec![ContentPart::text(UNDERSTOOD)],
        extensions: hofund_plugin_api::Extensions::new(),
    });
    out.extend(turn.iter().cloned());
    out
}

fn trim_leading_entries(entries: &mut Vec<Entry>) {
    while let Some(first) = entries.first() {
        match first.user() {
            Some(user)
                if user.content.is_empty()
                    && user.images.is_empty()
                    && user
                        .user_input_message_context
                        .as_ref()
                        .is_some_and(|context| !context.tool_results.is_empty()) =>
            {
                entries.remove(0);
            }
            Some(_) => {
                if let Some(user) = entries.first_mut().and_then(Entry::user_mut) {
                    user.user_input_message_context = None;
                }
                break;
            }
            None => {
                entries.remove(0);
            }
        }
    }
}

fn pair_tool_results(entries: &mut [Entry]) {
    for index in 0..entries.len() {
        let calls = if index == 0 {
            Vec::new()
        } else {
            entries[index - 1]
                .assistant()
                .map(|assistant| assistant.tool_uses.clone())
                .unwrap_or_default()
        };
        let Some(user) = entries[index].user_mut() else {
            continue;
        };
        let results = user
            .user_input_message_context
            .take()
            .map(|context| context.tool_results)
            .unwrap_or_default();
        let mut answered = HashSet::new();
        let mut kept = Vec::new();
        for result in results {
            if answered.insert(result.tool_use_id.clone())
                && calls
                    .iter()
                    .any(|call| call.tool_use_id == result.tool_use_id)
            {
                kept.push(result);
            }
        }
        for call in calls {
            if answered.insert(call.tool_use_id.clone()) {
                kept.push(ToolResult {
                    tool_use_id: call.tool_use_id,
                    status: "error",
                    content: vec![ResultText {
                        text: NO_RESULT.to_owned(),
                    }],
                });
            }
        }
        if !kept.is_empty() {
            user.user_input_message_context = Some(UserContext {
                tool_results: kept,
                tools: Vec::new(),
            });
        }
        if user.content.is_empty() && user.user_input_message_context.is_none() {
            user.content = PROCEED.to_owned();
        }
    }
}

fn keep_latest_images(entries: &mut [Entry]) {
    let latest = entries
        .iter()
        .rposition(|entry| entry.user().is_some_and(|user| !user.images.is_empty()));
    for (index, entry) in entries.iter_mut().enumerate() {
        if let Some(user) = entry.user_mut()
            && (Some(index) != latest || user.images.is_empty())
        {
            user.images.clear();
        }
    }
}

fn fit_images(entries: &mut [Entry], fitted: &mut FittedCache) {
    for user in entries.iter_mut().filter_map(Entry::user_mut) {
        if user.images.is_empty() {
            continue;
        }
        let mut kept = Vec::new();
        let mut notes = Vec::new();
        for image in user.images.drain(..) {
            let bytes = &image.source.bytes;
            if bytes.len() <= IMAGE_LIMIT {
                kept.push(image);
                continue;
            }
            match fitted.fit(bytes) {
                CachedImage::Image { format, bytes } => kept.push(Image {
                    format: format.to_owned(),
                    source: ImageBytes { bytes },
                }),
                CachedImage::Note(note) => notes.push(format!(
                    "[An image ({}, {:.1} MB as Base64) was left out of this message: Kiro takes at most 5.0 MB per image, and {note}.]",
                    image.format,
                    bytes.len() as f64 / (1024.0 * 1024.0),
                )),
            }
        }
        user.images = kept;
        if !notes.is_empty() {
            user.content = join_non_empty(user.content.clone(), notes.join("\n"));
        }
    }
}

fn add_tools(request: &Request, entries: &mut [Entry]) {
    let mut tools = Vec::new();
    let mut offered = HashSet::new();
    for tool in &request.tools {
        let name = tool_name(&tool.name);
        offered.insert(name.clone());
        tools.push(ToolDefinition {
            tool_specification: ToolSpecification {
                name,
                description: tool
                    .description
                    .clone()
                    .unwrap_or_else(|| tool.name.clone()),
                input_schema: InputSchema {
                    json: input_schema(&tool.input_schema),
                },
            },
        });
    }
    for entry in entries.iter() {
        if let Some(assistant) = entry.assistant() {
            for call in &assistant.tool_uses {
                if offered.insert(call.name.clone()) {
                    tools.push(ToolDefinition {
                        tool_specification: ToolSpecification {
                            name: call.name.clone(),
                            description: "Tool".to_owned(),
                            input_schema: InputSchema {
                                json: json::json!({"type": "object", "properties": {}}),
                            },
                        },
                    });
                }
            }
        }
    }
    if tools.is_empty() {
        return;
    }
    if let Some(user) = entries.last_mut().and_then(Entry::user_mut) {
        user.user_input_message_context
            .get_or_insert_with(UserContext::default)
            .tools = tools;
    }
}

fn input_schema(schema: &Value) -> Value {
    let Value::Object(object) = schema else {
        return json::json!({"type": "object", "properties": {}});
    };
    if object.is_empty() {
        return json::json!({"type": "object", "properties": {}});
    }
    let mut object = object.clone();
    match object.get("type") {
        None => {
            object.insert("type".to_owned(), Value::String("object".to_owned()));
        }
        Some(Value::Array(types))
            if types.iter().any(|kind| kind == "object")
                && types.iter().all(|kind| kind == "object" || kind == "null") =>
        {
            object.insert("type".to_owned(), Value::String("object".to_owned()));
        }
        _ => {}
    }
    Value::Object(object)
}

fn add_image(
    images: &mut Vec<Image>,
    image: &hofund_plugin_api::ImagePart,
) -> Result<(), PluginError> {
    let ImageSource::Base64 { media_type, data } = &image.source else {
        return Ok(());
    };
    if data.is_empty() {
        return Ok(());
    }
    images.push(Image {
        format: image_format(media_type),
        source: ImageBytes {
            bytes: data.clone(),
        },
    });
    Ok(())
}

fn image_format(media_type: &str) -> String {
    let media_type = media_type.to_ascii_lowercase();
    let Some(format) = media_type.strip_prefix("image/") else {
        return "png".to_owned();
    };
    match format {
        "" => "png".to_owned(),
        "jpg" => "jpeg".to_owned(),
        other => other.to_owned(),
    }
}

fn tool_id(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return id.to_owned();
    }
    format!(
        "t_{}",
        base64url(&digest::sha256(id.as_bytes()))[..32].to_owned()
    )
}

fn tool_name(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return name.to_owned();
    }
    let hash = digest::sha256(name.as_bytes());
    let hash = hash[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut head = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    head.truncate(64 - 1 - hash.len());
    format!("{head}_{hash}")
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(char::from(ALPHABET[((value >> 18) & 63) as usize]));
        out.push(char::from(ALPHABET[((value >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((value >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(value & 63) as usize]));
        }
    }
    out
}

fn native_fields(request: &Request, native: Option<&crate::models::NativeEffort>) -> Option<Value> {
    let native = native?;
    let effort = request
        .extensions
        .get("output_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("effort"))
        .and_then(Value::as_str)?;
    if !native.levels.iter().any(|level| level == effort) || thinking_budget(request).is_none() {
        return None;
    }
    let mut fields = Map::new();
    fields.insert("thinking".to_owned(), json::json!({"type": "adaptive"}));
    fields.insert(native.path.to_owned(), json::json!({"effort": effort}));
    Some(Value::Object(fields))
}

fn thinking_budget(request: &Request) -> Option<u64> {
    let thinking = request.extensions.get("thinking")?.as_object()?;
    if !matches!(
        thinking.get("type").and_then(Value::as_str),
        Some("enabled" | "adaptive")
    ) || !(request.model.eq_ignore_ascii_case("auto")
        || request.model.to_ascii_lowercase().contains("claude"))
    {
        return None;
    }
    let effort = request
        .extensions
        .get("output_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("effort"))
        .and_then(Value::as_str);
    if let Some(effort) = effort {
        return Some(match effort {
            "low" => 10_000,
            "high" => 30_000,
            "xhigh" | "max" => 50_000,
            "medium" => 20_000,
            _ => 20_000,
        });
    }
    if let Some(effort) = request.params.effort {
        return match effort.openai_label() {
            "none" => None,
            "minimal" | "low" => Some(10_000),
            "medium" => Some(20_000),
            "high" => Some(30_000),
            "xhigh" | "max" => Some(50_000),
            _ => Some(20_000),
        };
    }
    thinking
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .filter(|budget| *budget > 0)
        .or(Some(20_000))
}

fn structured_output(request: &Request) -> Option<String> {
    let format = request
        .extensions
        .get("output_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("format"))
        .or_else(|| request.extensions.get("output_format"))?;
    let schema = format
        .as_object()
        .filter(|format| format.get("type").and_then(Value::as_str) == Some("json_schema"))?
        .get("schema")?
        .as_object()?;
    Some(format!(
        "Respond with a single JSON value and nothing else, matching this JSON schema:\n{}.",
        Value::Object(schema.clone())
    ))
}

fn cap_result(mut text: String) -> String {
    if text.len() <= RESULT_LIMIT {
        return text;
    }
    let mut boundary = RESULT_LIMIT;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str("\n… (cut)");
    text
}

fn join_non_empty(left: String, right: String) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => String::new(),
        (false, true) => left,
        (true, false) => right,
        (false, false) => format!("{left}\n\n{right}"),
    }
}

fn join_into(target: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    *target = join_non_empty(target.clone(), text.to_owned());
}
