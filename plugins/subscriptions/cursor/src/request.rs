//! Canonical-request mapping for one Cursor Run.

use std::collections::{BTreeMap, HashMap};

use hofund_plugin_api::{
    ContentPart, Extensions, ImagePart, ImageSource, Message, PluginError, Request, Role,
    ToolArguments, ToolChoiceMode, ToolResultPart,
};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};

use crate::proto::{self, AgentRunParams, Pb, ToolDef};
use crate::run::{Limits, RunArgs};

const CALL: &str = "CallDynamicTool";
const NAMESPACE: &str = "tools";
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";

/// A fully owned request opening and every blob its state references.
#[derive(Debug)]
pub(crate) struct MappedRequest {
    pub(crate) opening: Vec<u8>,
    pub(crate) blobs: HashMap<Vec<u8>, Vec<u8>>,
    pub(crate) tools: Vec<ToolDef>,
    pub(crate) conversation_id: String,
    os: String,
    temp_dir: String,
}

impl MappedRequest {
    /// Moves every byte into a stream that can outlive `execute`.
    pub(crate) fn into_args(self, team_id: Option<String>) -> RunArgs<'static> {
        RunArgs {
            opening: self.opening,
            team_id,
            blobs: std::borrow::Cow::Owned(self.blobs),
            os: std::borrow::Cow::Owned(proto::goos_name_for(&self.os).to_owned()),
            temp_dir: std::borrow::Cow::Owned(self.temp_dir),
            tools: self.tools,
            limits: Limits::default(),
        }
    }
}

/// Maps a request, optionally reusing a conversation id and setting Max Mode bits.
pub(crate) fn map_with_mode(
    request: &Request,
    conversation_id: Option<&str>,
    max_mode: bool,
) -> Result<MappedRequest, PluginError> {
    let message_id = new_uuid()?;
    let conversation_id = match conversation_id {
        Some(id) => id.to_owned(),
        None => new_uuid()?,
    };
    let temp_dir = std::env::temp_dir()
        .join(format!("cursor-{message_id}"))
        .to_string_lossy()
        .into_owned();
    let mapped = build(
        request,
        &conversation_id,
        &message_id,
        std::env::consts::OS,
        &temp_dir,
        max_mode,
    )?;
    #[cfg(test)]
    {
        MAPPED_MAX_MODE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(max_mode);
    }
    Ok(mapped)
}

/// Maps a request with a fresh conversation, message, and per-execute temp path.
#[cfg(test)]
pub(crate) fn map(request: &Request) -> Result<MappedRequest, PluginError> {
    map_with_mode(request, None, false)
}

/// Max Mode flags used by each [`map_with_mode`] call, in call order (tests only).
#[cfg(test)]
pub(crate) fn take_mapped_max_mode() -> Vec<bool> {
    std::mem::take(
        &mut *MAPPED_MAX_MODE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[cfg(test)]
static MAPPED_MAX_MODE: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());

fn new_uuid() -> Result<String, PluginError> {
    hofund_plugin_sdk::uuid::v4()
        .map_err(|error| PluginError::io(format!("cannot create a Run id: {error}")))
}

fn build(
    request: &Request,
    conversation_id: &str,
    message_id: &str,
    os: &str,
    temp_dir: &str,
    max_mode: bool,
) -> Result<MappedRequest, PluginError> {
    let tools = tools_of(request);

    let mut blobs = HashMap::new();
    let mut transcript_blob_ids = Vec::new();
    let mut system_text = Vec::new();
    let mut transcript = Vec::new();
    let mut last_user_text = None;
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => system_text.push(text_of(message)),
            Role::User => {
                let text = text_of(message);
                last_user_text = Some(text);
                transcript.push(message);
            }
            Role::Assistant | Role::Tool => transcript.push(message),
        }
    }
    let mut system = system_text.join("\n");
    if !tools.is_empty() {
        system.push_str(&catalog(&tools));
    }
    if !system.is_empty() {
        let blob = system_blob(&system);
        transcript_blob_ids.push(insert_blob(&mut blobs, blob));
    }

    let mut pending = Vec::new();
    let mut results = Vec::new();
    for group in join_split_calls(transcript) {
        match group[0].role {
            Role::Assistant => {
                answer_pending(
                    &mut blobs,
                    &mut transcript_blob_ids,
                    &mut pending,
                    &mut results,
                );
                let blob = assistant_blob(&group, &mut pending);
                transcript_blob_ids.push(insert_blob(&mut blobs, blob));
            }
            Role::Tool => {
                for message in group {
                    collect_results(message, &mut pending, &mut results);
                }
            }
            Role::User => {
                for message in &group {
                    collect_results(message, &mut pending, &mut results);
                }
                answer_pending(
                    &mut blobs,
                    &mut transcript_blob_ids,
                    &mut pending,
                    &mut results,
                );
                let blob = user_blob(group[0]);
                transcript_blob_ids.push(insert_blob(&mut blobs, blob));
            }
            Role::System | Role::Developer => unreachable!("system messages are not in transcript"),
        }
    }
    answer_pending(
        &mut blobs,
        &mut transcript_blob_ids,
        &mut pending,
        &mut results,
    );

    let turn_text = last_user_text
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| ".".to_owned());
    let user_turn = Pb::new()
        .string(1, &turn_text)
        .string(2, message_id)
        .varint(4, 1)
        .finish();
    let user_turn_blob_id = sha256(&user_turn);
    blobs.insert(user_turn_blob_id.to_vec(), user_turn);

    let link = Pb::new()
        .message(
            1,
            Pb::new()
                .bytes(1, &user_turn_blob_id)
                .string(10, message_id),
        )
        .finish();
    transcript_blob_ids.push(insert_blob(&mut blobs, link));

    let opening = proto::client_run(&proto::encode_agent_run_request(&AgentRunParams {
        conversation_id,
        model_id: &request.model,
        transcript_blob_ids: &transcript_blob_ids,
        user_turn_blob_id: &user_turn_blob_id,
        os,
        temp_dir,
        tools: &tools,
        inline_images: has_inline_image(request),
        max_mode,
    }));
    Ok(MappedRequest {
        opening,
        blobs,
        tools,
        conversation_id: conversation_id.to_owned(),
        os: os.to_owned(),
        temp_dir: temp_dir.to_owned(),
    })
}

fn tools_of(request: &Request) -> Vec<ToolDef> {
    request
        .tools
        .iter()
        .filter(|tool| match &request.tool_choice {
            Some(choice) => match &choice.mode {
                ToolChoiceMode::None => false,
                ToolChoiceMode::Named { name } => &tool.name == name,
                ToolChoiceMode::Auto | ToolChoiceMode::Required => true,
            },
            None => true,
        })
        .map(|tool| ToolDef {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            schema: tool.input_schema.clone(),
        })
        .collect()
}

fn catalog(tools: &[ToolDef]) -> String {
    let mut text = format!(
        "\n\n<dynamic_tool_catalog>\nThe MCP namespace \"{NAMESPACE}\" has the tools below. They are reached only through `{CALL}`, never as tools of their own. Each entry is the exact call to make; its arguments must match the schema given. The schemas are complete, so there is no need to call `GetDynamicTools` first.\n"
    );
    for tool in tools {
        text.push_str(&format!(
            "<call>{CALL}({{\"namespace\":\"{NAMESPACE}\",\"toolName\":\"{}\",\"arguments\":{{...}}}})\n{}\narguments schema: {}\n</call>\n",
            tool.name, tool.description, tool.schema
        ));
    }
    text.push_str("</dynamic_tool_catalog>");
    text
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(part) => Some(part.text.as_str()),
            _ => None,
        })
        .collect()
}

fn system_blob(text: &str) -> Vec<u8> {
    object([
        ("content", Value::String(text.to_owned())),
        ("role", Value::String("system".to_owned())),
    ])
    .to_string()
    .into_bytes()
}

fn join_split_calls(messages: Vec<&Message>) -> Vec<Vec<&Message>> {
    let mut groups: Vec<Vec<&Message>> = Vec::new();
    for message in messages {
        let join = message.role == Role::Assistant
            && groups.last().is_some_and(|previous| {
                previous[0].role == Role::Assistant
                    && previous.iter().any(|message| {
                        message
                            .content
                            .iter()
                            .any(|part| matches!(part, ContentPart::ToolCall(_)))
                    })
            });
        if join {
            groups
                .last_mut()
                .expect("a joined assistant group exists")
                .push(message);
        } else {
            groups.push(vec![message]);
        }
    }
    groups
}

fn assistant_blob(messages: &[&Message], pending: &mut Vec<String>) -> Vec<u8> {
    let mut content = Vec::new();
    for part in messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::Thinking(part) if !part.text.is_empty() => Some(part),
            _ => None,
        })
    {
        // Replay thinking as its own AI SDK reasoning part. Do not copy the
        // bytes into the text part, and do not invent a signature.
        content.push(object([
            ("text", Value::String(part.text.clone())),
            ("type", Value::String("reasoning".to_owned())),
        ]));
    }
    let text = messages
        .iter()
        .map(|message| text_of(message))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !text.is_empty() {
        content.push(object([
            ("text", Value::String(text)),
            ("type", Value::String("text".to_owned())),
        ]));
    }
    for call in messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call),
            _ => None,
        })
    {
        content.push(object([
            (
                "args",
                object([
                    ("arguments", tool_arguments(&call.arguments)),
                    ("namespace", Value::String(NAMESPACE.to_owned())),
                    ("toolName", Value::String(call.name.clone())),
                ]),
            ),
            ("toolCallId", Value::String(call_id(&call.id))),
            ("toolName", Value::String(CALL.to_owned())),
            ("type", Value::String("tool-call".to_owned())),
        ]));
        pending.push(call.id.clone());
    }
    object([
        ("content", Value::Array(content)),
        ("role", Value::String("assistant".to_owned())),
    ])
    .to_string()
    .into_bytes()
}

fn tool_arguments(arguments: &ToolArguments) -> Value {
    let value = match arguments {
        ToolArguments::Json { value } => value.clone(),
        ToolArguments::Raw { text } => {
            hofund_plugin_sdk::json::from_str(text).unwrap_or_else(|_| Value::Object(Map::new()))
        }
    };
    match value {
        Value::Object(_) => value,
        _ => Value::Object(Map::new()),
    }
}

fn user_blob(message: &Message) -> Vec<u8> {
    let content = message
        .content
        .iter()
        .filter_map(user_content_part)
        .collect();
    object([
        ("content", Value::Array(content)),
        ("role", Value::String("user".to_owned())),
    ])
    .to_string()
    .into_bytes()
}

fn user_content_part(part: &ContentPart) -> Option<Value> {
    match part {
        ContentPart::Text(part) => Some(object([
            ("text", Value::String(part.text.clone())),
            ("type", Value::String("text".to_owned())),
        ])),
        ContentPart::Image(image) => Some(image_content_part(image)),
        ContentPart::Unknown(unknown) if is_file_kind(&unknown.kind) => Some(object([
            (
                "text",
                Value::String(format!(
                    "[attachment {}]",
                    file_attachment_name(&unknown.extensions)
                )),
            ),
            ("type", Value::String("text".to_owned())),
        ])),
        _ => None,
    }
}

fn has_inline_image(request: &Request) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|part| match part {
            ContentPart::Image(image) => inline_image_bytes(image).is_some(),
            _ => false,
        })
}

fn image_content_part(image: &ImagePart) -> Value {
    if let Some((mime_type, hex)) = inline_image_bytes(image) {
        return object([
            (
                "image",
                object([
                    ("__type", Value::String("Uint8Array".to_owned())),
                    ("hex", Value::String(hex)),
                ]),
            ),
            ("mimeType", Value::String(mime_type)),
            ("type", Value::String("image".to_owned())),
        ]);
    }
    let url = match &image.source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
    };
    object([
        ("text", Value::String(format!("[attachment image: {url}]"))),
        ("type", Value::String("text".to_owned())),
    ])
}

fn inline_image_bytes(image: &ImagePart) -> Option<(String, String)> {
    let (mime_type, data) = match &image.source {
        ImageSource::Base64 { media_type, data } => (media_type.clone(), data.clone()),
        ImageSource::Url { url } => data_url_parts(url)?,
    };
    let bytes = decode_base64(&data)?;
    Some((mime_type, hex_lower(&bytes)))
}

/// Matches the reference data-URL shape: `data:{type};base64,{payload}`.
fn data_url_parts(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || media_type.contains(',') || media_type.contains(';') {
        return None;
    }
    Some((media_type.to_owned(), data.to_owned()))
}

fn is_file_kind(kind: &str) -> bool {
    matches!(kind, "file" | "input_file" | "document")
}

fn file_attachment_name(extensions: &Extensions) -> String {
    if let Some(name) = extensions
        .get("file")
        .and_then(|file| file.get("filename").or_else(|| file.get("name")))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        return name.to_owned();
    }
    for key in ["filename", "name", "title", "file_name"] {
        if let Some(name) = extensions
            .get(key)
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            return name.to_owned();
        }
    }
    String::from("file")
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("write to a string");
    }
    out
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let pad = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if pad > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - pad];
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    for chunk in body.chunks(4) {
        let mut acc = 0_u32;
        for byte in chunk {
            acc = (acc << 6) | u32::from(base64_value(*byte)?);
        }
        match chunk.len() {
            2 => out.push((acc >> 4) as u8),
            3 => {
                out.push((acc >> 10) as u8);
                out.push((acc >> 2) as u8);
            }
            4 => {
                out.push((acc >> 16) as u8);
                out.push((acc >> 8) as u8);
                out.push(acc as u8);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn collect_results(message: &Message, pending: &mut Vec<String>, results: &mut Vec<Value>) {
    for result in message.content.iter().filter_map(|part| match part {
        ContentPart::ToolResult(result) => Some(result),
        _ => None,
    }) {
        let Some(index) = pending.iter().position(|id| id == &result.id) else {
            continue;
        };
        pending.remove(index);
        results.push(tool_result(result));
    }
}

fn tool_result(result: &ToolResultPart) -> Value {
    let text = result
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(part) => Some(part.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let parsed =
        hofund_plugin_sdk::json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
    let mut value = object([
        (
            "experimental_content",
            Value::Array(vec![object([
                ("text", Value::String(text)),
                ("type", Value::String("text".to_owned())),
            ])]),
        ),
        ("result", parsed),
        ("toolCallId", Value::String(call_id(&result.id))),
        ("toolName", Value::String(CALL.to_owned())),
        ("type", Value::String("tool-result".to_owned())),
    ]);
    if result.is_error {
        let Value::Object(fields) = &mut value else {
            unreachable!("tool result is an object")
        };
        fields.insert("isError".to_owned(), Value::Bool(true));
    }
    value
}

fn no_result(id: &str) -> Value {
    let result = ToolResultPart {
        id: id.to_owned(),
        is_error: true,
        content: vec![ContentPart::text(NO_RESULT)],
        extensions: Default::default(),
    };
    tool_result(&result)
}

fn answer_pending(
    blobs: &mut HashMap<Vec<u8>, Vec<u8>>,
    transcript_blob_ids: &mut Vec<[u8; 32]>,
    pending: &mut Vec<String>,
    results: &mut Vec<Value>,
) {
    results.extend(pending.drain(..).map(|id| no_result(&id)));
    if results.is_empty() {
        return;
    }
    let blob = object([
        ("content", Value::Array(std::mem::take(results))),
        ("role", Value::String("tool".to_owned())),
    ])
    .to_string()
    .into_bytes();
    transcript_blob_ids.push(insert_blob(blobs, blob));
}

fn call_id(id: &str) -> String {
    if id.starts_with("call_") {
        id.replacen("__fc_", "\nfc_", 1)
    } else {
        id.to_owned()
    }
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let sorted = entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect::<BTreeMap<_, _>>();
    Value::Object(sorted.into_iter().collect::<Map<_, _>>())
}

fn insert_blob(blobs: &mut HashMap<Vec<u8>, Vec<u8>>, bytes: Vec<u8>) -> [u8; 32] {
    let id = sha256(&bytes);
    blobs.insert(id.to_vec(), bytes);
    id
}

/// Deterministic mapping inputs used by unit tests.
#[cfg(test)]
struct MapOptions<'a> {
    os: &'a str,
    temp_dir: &'a str,
    conversation_id: &'a str,
    message_id: &'a str,
    max_mode: bool,
}

#[cfg(test)]
fn map_with(request: &Request, options: MapOptions<'_>) -> Result<MappedRequest, PluginError> {
    build(
        request,
        options.conversation_id,
        options.message_id,
        options.os,
        options.temp_dir,
        options.max_mode,
    )
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{ContentPart, ImagePart, ImageSource, Message, Request, Role};
    use hofund_plugin_sdk::digest::sha256;
    use hofund_plugin_sdk::json::{Value, from_value, json};

    use super::{MapOptions, map, map_with};
    use crate::proto::{self, Field, Pb};

    const CONVERSATION_ID: &str = "00000000-0000-4000-8000-000000000001";
    const MESSAGE_ID: &str = "00000000-0000-4000-8000-000000000002";

    fn options() -> MapOptions<'static> {
        MapOptions {
            os: "macos",
            temp_dir: "/tmp/cursor-request-test",
            conversation_id: CONVERSATION_ID,
            message_id: MESSAGE_ID,
            max_mode: false,
        }
    }

    fn message(role: Role, parts: &[&str]) -> Message {
        let mut message = Message::new(role);
        message
            .content
            .extend(parts.iter().map(|text| ContentPart::text(*text)));
        message
    }

    fn field(fields: &[Field], number: u32) -> &Field {
        fields
            .iter()
            .find(|field| field.number == number)
            .unwrap_or_else(|| panic!("missing protobuf field {number}"))
    }

    fn agent_request(opening: &[u8]) -> Vec<Field> {
        let client = proto::fields(opening);
        proto::fields(field(&client, 1).as_bytes().expect("AgentRunRequest"))
    }

    fn state(mapped: &super::MappedRequest) -> Vec<Field> {
        let request = agent_request(&mapped.opening);
        field(&request, 1).as_message().expect("conversation state")
    }

    fn user_turn_id(mapped: &super::MappedRequest) -> Vec<u8> {
        field(&state(mapped), 8)
            .as_bytes()
            .expect("user turn id")
            .to_vec()
    }

    fn json_blobs(mapped: &super::MappedRequest) -> Vec<Value> {
        mapped
            .blobs
            .values()
            .filter_map(|bytes| hofund_plugin_sdk::json::from_slice::<Value>(bytes).ok())
            .collect()
    }

    #[test]
    fn empty_conversation_uses_a_dot_and_links_the_user_message_blob() {
        let mapped = map_with(&Request::new("auto"), options()).expect("mapping");
        let state = state(&mapped);
        let user_turn_id = user_turn_id(&mapped);
        assert_eq!(user_turn_id.len(), 32);

        let user_turn = proto::fields(
            mapped
                .blobs
                .get(&user_turn_id)
                .expect("user turn blob is fetchable"),
        );
        assert_eq!(field(&user_turn, 1).as_string().as_deref(), Some("."));
        assert_eq!(
            field(&user_turn, 2).as_string().as_deref(),
            Some(MESSAGE_ID)
        );
        assert_eq!(field(&user_turn, 4).as_varint(), Some(1));
        assert_eq!(field(&state, 10).as_varint(), Some(1));
        assert_eq!(field(&state, 22).as_string().as_deref(), Some("cli"));

        let transcript_ids = state
            .iter()
            .filter(|field| field.number == 1)
            .map(|field| field.as_bytes().expect("transcript id").to_vec())
            .collect::<Vec<_>>();
        assert_eq!(transcript_ids.len(), 1, "only the user-message link");
        let link_id = &transcript_ids[0];
        assert_eq!(
            link_id.as_slice(),
            sha256(&mapped.blobs[link_id]).as_slice()
        );
        let link = proto::fields(&mapped.blobs[link_id]);
        let inner = field(&link, 1).as_message().expect("user-message link");
        assert_eq!(field(&inner, 1).as_bytes(), Some(user_turn_id.as_slice()));
        assert_eq!(field(&inner, 10).as_string().as_deref(), Some(MESSAGE_ID));
    }

    #[test]
    fn system_developer_and_text_history_become_stable_json_blobs() {
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(ContentPart::thinking("hidden"));
        assistant.content.push(ContentPart::text("answer"));
        let request = Request::new("auto").with_messages(vec![
            message(Role::System, &["system-a", "system-b"]),
            message(Role::Developer, &["developer"]),
            message(Role::User, &["first"]),
            assistant,
            message(Role::User, &["last", " user"]),
        ]);
        let mapped = map_with(&request, options()).expect("mapping");
        let blobs = json_blobs(&mapped);
        let system = blobs
            .iter()
            .find(|blob| blob["role"] == "system")
            .expect("one system blob");
        assert_eq!(system["content"], "system-asystem-b\ndeveloper");

        let assistant = blobs
            .iter()
            .find(|blob| blob["role"] == "assistant")
            .expect("assistant blob");
        assert_eq!(
            assistant["content"],
            json!([
                {"text": "hidden", "type": "reasoning"},
                {"text": "answer", "type": "text"}
            ])
        );
        let text_part = &assistant["content"][1];
        assert_eq!(text_part["type"], "text");
        assert!(
            !text_part.to_string().contains("hidden"),
            "thinking bytes must be absent from the text block: {text_part}"
        );

        let user_turn = proto::fields(&mapped.blobs[&user_turn_id(&mapped)]);
        assert_eq!(
            field(&user_turn, 1).as_string().as_deref(),
            Some("last user")
        );
        for transcript in state(&mapped).iter().filter(|field| field.number == 1) {
            let id = transcript.as_bytes().expect("digest");
            let bytes = mapped.blobs.get(id).expect("every id is fetchable");
            assert_eq!(id, sha256(bytes).as_slice());
        }
    }

    #[test]
    fn model_auto_conversation_and_environment_are_encoded_verbatim() {
        let mapped = map_with(&Request::new("auto"), options()).expect("mapping");
        let request = agent_request(&mapped.opening);
        assert_eq!(
            field(&request, 5).as_string().as_deref(),
            Some(CONVERSATION_ID)
        );
        for number in [3, 9] {
            let model = field(&request, number).as_message().expect("model");
            assert_eq!(field(&model, 1).as_string().as_deref(), Some("auto"));
        }
        let action = field(&request, 2).as_message().expect("action");
        let action = field(&action, 2).as_message().expect("action context");
        let context = field(&action, 2).as_message().expect("request context");
        let environment = field(&context, 4).as_message().expect("environment");
        assert_eq!(
            field(&environment, 1).as_string().as_deref(),
            Some("darwin")
        );
        assert_eq!(
            field(&environment, 2).as_string().as_deref(),
            Some("/tmp/cursor-request-test")
        );
        assert_eq!(field(&environment, 10).as_string().as_deref(), Some("UTC"));
        assert!(
            field(&request, 4)
                .as_message()
                .expect("tool wrapper")
                .is_empty()
        );
        assert!(request.iter().all(|field| field.number != 19));
        assert_eq!(mapped.into_args(None).os, "darwin");
        let details = field(&request, 3).as_message().expect("model details");
        let requested = field(&request, 9).as_message().expect("requested model");
        assert!(details.iter().all(|field| field.number != 7));
        assert!(requested.iter().all(|field| field.number != 2));
    }

    #[test]
    fn mapping_sets_max_mode_bits_on_model_details_and_requested_model() {
        let mut options = options();
        options.max_mode = true;
        let mapped = map_with(&Request::new("auto"), options).expect("mapping");
        let request = agent_request(&mapped.opening);
        let details = field(&request, 3).as_message().expect("model details");
        let requested = field(&request, 9).as_message().expect("requested model");
        assert_eq!(field(&details, 7).as_varint(), Some(1));
        assert_eq!(field(&requested, 2).as_varint(), Some(1));
        assert_eq!(field(&requested, 1).as_string().as_deref(), Some("auto"));
    }

    #[test]
    fn production_mapping_generates_a_fresh_conversation_id() {
        let request = Request::new("auto");
        let first = map(&request).expect("first mapping");
        let second = map(&request).expect("second mapping");
        let first = field(&agent_request(&first.opening), 5)
            .as_string()
            .expect("first conversation id");
        let second = field(&agent_request(&second.opening), 5)
            .as_string()
            .expect("second conversation id");
        assert_ne!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(second.len(), 36);
    }

    fn request_with_tools() -> Request {
        from_value(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [
                {"name": "lookup", "description": "find a value", "input_schema": {"type": "object", "properties": {"key": {"type": "string"}}}},
                {"name": "shell", "input_schema": {"type": "object"}}
            ]
        }))
        .expect("request")
    }

    fn selected(request: Request) -> super::MappedRequest {
        map_with(&request, options()).expect("tool mapping")
    }

    fn tool_names(fields: &[Field], number: u32) -> Vec<String> {
        fields
            .iter()
            .filter(|field| field.number == number)
            .map(|tool| {
                tool.as_message()
                    .and_then(|tool| field(&tool, 1).as_string())
                    .expect("tool name")
            })
            .collect()
    }

    #[test]
    fn tool_choice_none_sends_no_defs_or_catalog() {
        let mapped = selected(
            request_with_tools().with_tool_choice(Some(hofund_plugin_api::ToolChoice::none())),
        );
        assert!(mapped.tools.is_empty());
        let request = agent_request(&mapped.opening);
        assert!(field(&request, 4).as_message().expect("wrapper").is_empty());
        let action = field(&request, 2).as_message().expect("action");
        let action = field(&action, 2).as_message().expect("action context");
        let context = field(&action, 2).as_message().expect("request context");
        assert!(context.iter().all(|field| field.number != 7));
        assert!(
            json_blobs(&mapped)
                .iter()
                .all(|blob| blob["role"] != "system")
        );
    }

    #[test]
    fn named_tool_choice_sends_only_that_function_and_its_catalog() {
        let mapped = selected(
            request_with_tools()
                .with_tool_choice(Some(hofund_plugin_api::ToolChoice::named("shell"))),
        );
        assert_eq!(mapped.tools.len(), 1);
        assert_eq!(mapped.tools[0].name, "shell");
        assert_eq!(mapped.tools[0].description, "");
        let request = agent_request(&mapped.opening);
        assert_eq!(
            tool_names(&field(&request, 4).as_message().expect("wrapper"), 1),
            ["shell"]
        );
        let system = json_blobs(&mapped)
            .into_iter()
            .find(|blob| blob["role"] == "system")
            .expect("catalog system blob");
        let text = system["content"].as_str().expect("catalog text");
        assert!(text.contains("The MCP namespace \"tools\""));
        assert!(text.contains(
            "CallDynamicTool({\"namespace\":\"tools\",\"toolName\":\"shell\",\"arguments\":{...}})"
        ));
        assert!(!text.contains("toolName\":\"lookup"));
    }

    #[test]
    fn default_tool_choice_sends_all_defs_and_appends_the_catalog_to_system_text() {
        let mut request = request_with_tools();
        request
            .messages
            .insert(0, message(Role::System, &["keep me"]));
        let mapped = selected(request);
        assert_eq!(mapped.tools.len(), 2);
        let request = agent_request(&mapped.opening);
        assert_eq!(
            tool_names(&field(&request, 4).as_message().expect("wrapper"), 1),
            ["lookup", "shell"]
        );
        let action = field(&request, 2).as_message().expect("action");
        let action = field(&action, 2).as_message().expect("action context");
        let context = field(&action, 2).as_message().expect("request context");
        assert_eq!(tool_names(&context, 7), ["lookup", "shell"]);
        let system = json_blobs(&mapped)
            .into_iter()
            .find(|blob| blob["role"] == "system")
            .expect("system catalog");
        let text = system["content"].as_str().expect("system text");
        assert!(text.starts_with("keep me\n\n<dynamic_tool_catalog>"));
        assert!(text.contains("find a value\narguments schema: {\"type\":\"object\",\"properties\":{\"key\":{\"type\":\"string\"}}}"));
    }

    #[test]
    fn history_encodes_joined_dynamic_calls_results_no_result_and_replay_ids() {
        let request: Request = from_value(json!({
            "model": "auto",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "read both"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "text": "hidden reasoning"},
                    {"type": "text", "text": "reading"},
                    {"type": "tool_call", "id": "call_alpha__fc_1", "name": "lookup", "arguments": {"form": "json", "value": {"key": "a"}}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_call", "id": "toolu_b__fc_2", "name": "lookup", "arguments": {"form": "raw", "text": "not json"}}
                ]},
                {"role": "tool", "content": [
                    {"type": "tool_result", "id": "call_alpha__fc_1", "content": [{"type": "text", "text": "{\"ok\":true}"}]}
                ]}
            ]
        }))
        .expect("request");
        let mapped = map_with(&request, options()).expect("history mapping");
        let blobs = json_blobs(&mapped);
        let assistants = blobs
            .iter()
            .filter(|blob| blob["role"] == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1, "split calls join into one assistant");
        let content = assistants[0]["content"].as_array().expect("parts");
        assert_eq!(content.len(), 4, "reasoning, text and two calls");
        assert_eq!(
            content[0],
            json!({"text": "hidden reasoning", "type": "reasoning"})
        );
        assert_eq!(content[1], json!({"text": "reading", "type": "text"}));
        assert!(
            !content[1].to_string().contains("hidden reasoning"),
            "thinking bytes must be absent from the text block: {}",
            content[1]
        );
        assert_eq!(content[2]["type"], "tool-call");
        assert_eq!(content[2]["toolCallId"], "call_alpha\nfc_1");
        assert_eq!(content[2]["toolName"], "CallDynamicTool");
        assert_eq!(
            content[2]["args"],
            json!({"namespace": "tools", "toolName": "lookup", "arguments": {"key": "a"}})
        );
        assert_eq!(content[3]["toolCallId"], "toolu_b__fc_2");
        assert_eq!(content[3]["args"]["arguments"], json!({}));

        let tool = blobs
            .iter()
            .find(|blob| blob["role"] == "tool")
            .expect("results");
        let results = tool["content"].as_array().expect("result parts");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolCallId"], "call_alpha\nfc_1");
        assert_eq!(results[0]["result"], json!({"ok": true}));
        assert_eq!(
            results[0]["experimental_content"][0]["text"],
            "{\"ok\":true}"
        );
        assert_eq!(results[1]["toolCallId"], "toolu_b__fc_2");
        assert_eq!(results[1]["isError"], true);
        assert_eq!(
            results[1]["experimental_content"][0]["text"],
            "Tool use was interrupted and did not produce a result."
        );
    }

    #[test]
    fn image_urls_become_text_placeholders_and_do_not_set_field_19() {
        let mut request = request_with_tools();
        request.messages[0]
            .content
            .push(ContentPart::Image(ImagePart {
                source: ImageSource::Url {
                    url: "https://image.test/a.png".to_owned(),
                },
                detail: None,
                extensions: Default::default(),
            }));
        let mapped = map_with(&request, options()).expect("url images are mapped");
        let user = json_blobs(&mapped)
            .into_iter()
            .find(|blob| blob["role"] == "user")
            .expect("user blob");
        assert_eq!(
            user["content"],
            json!([
                {"text": "hi", "type": "text"},
                {"text": "[attachment image: https://image.test/a.png]", "type": "text"}
            ])
        );
        let request = agent_request(&mapped.opening);
        assert!(request.iter().all(|field| field.number != 19));
    }

    #[test]
    fn data_url_and_base64_images_become_hex_parts_and_set_field_19() {
        let png = "aGVsbG8="; // "hello"
        let mut request = Request::new("auto").with_messages(vec![message(Role::User, &["see"])]);
        request.messages[0]
            .content
            .push(ContentPart::Image(ImagePart {
                source: ImageSource::Url {
                    url: format!("data:image/png;base64,{png}"),
                },
                detail: None,
                extensions: Default::default(),
            }));
        request.messages[0]
            .content
            .push(ContentPart::Image(ImagePart {
                source: ImageSource::Base64 {
                    media_type: "image/jpeg".to_owned(),
                    data: png.to_owned(),
                },
                detail: None,
                extensions: Default::default(),
            }));
        let mapped = map_with(&request, options()).expect("inline images");
        let user = json_blobs(&mapped)
            .into_iter()
            .find(|blob| blob["role"] == "user")
            .expect("user blob");
        assert_eq!(
            user["content"],
            json!([
                {"text": "see", "type": "text"},
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "image": {"__type": "Uint8Array", "hex": "68656c6c6f"}
                },
                {
                    "type": "image",
                    "mimeType": "image/jpeg",
                    "image": {"__type": "Uint8Array", "hex": "68656c6c6f"}
                }
            ])
        );
        let request = agent_request(&mapped.opening);
        assert_eq!(field(&request, 19).as_varint(), Some(1));
        let user_turn = proto::fields(&mapped.blobs[&user_turn_id(&mapped)]);
        assert_eq!(field(&user_turn, 1).as_string().as_deref(), Some("see"));
    }

    #[test]
    fn an_image_only_user_turn_uses_a_dot() {
        let mut request = Request::new("auto");
        let mut user = Message::new(Role::User);
        user.content.push(ContentPart::Image(ImagePart {
            source: ImageSource::Url {
                url: "https://image.test/only.png".to_owned(),
            },
            detail: None,
            extensions: Default::default(),
        }));
        request.messages.push(user);
        let mapped = map_with(&request, options()).expect("image-only user");
        let user_turn = proto::fields(&mapped.blobs[&user_turn_id(&mapped)]);
        assert_eq!(field(&user_turn, 1).as_string().as_deref(), Some("."));
        assert!(
            agent_request(&mapped.opening)
                .iter()
                .all(|field| field.number != 19)
        );
    }

    #[test]
    fn file_parts_become_named_attachment_placeholders() {
        let mut request = Request::new("auto").with_messages(vec![message(Role::User, &["docs"])]);
        request.messages[0].content.push(
            from_value(json!({
                "type": "unknown",
                "kind": "file",
                "extensions": {"file": {"filename": "notes.pdf"}}
            }))
            .expect("file part"),
        );
        request.messages[0].content.push(
            from_value(json!({
                "type": "unknown",
                "kind": "document",
                "extensions": {}
            }))
            .expect("nameless file"),
        );
        let mapped = map_with(&request, options()).expect("file parts");
        let user = json_blobs(&mapped)
            .into_iter()
            .find(|blob| blob["role"] == "user")
            .expect("user blob");
        assert_eq!(
            user["content"],
            json!([
                {"text": "docs", "type": "text"},
                {"text": "[attachment notes.pdf]", "type": "text"},
                {"text": "[attachment file]", "type": "text"}
            ])
        );
    }

    #[test]
    fn decode_base64_accepts_standard_and_unpadded_payloads() {
        assert_eq!(
            super::decode_base64("aGVsbG8=").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            super::decode_base64("aGVsbG8").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            super::decode_base64("YQ==").as_deref(),
            Some(b"a".as_slice())
        );
        assert_eq!(
            super::decode_base64("YWI=").as_deref(),
            Some(b"ab".as_slice())
        );
        assert_eq!(super::decode_base64("").as_deref(), Some(b"".as_slice()));
        assert!(super::decode_base64("!!!!").is_none());
    }

    #[test]
    fn outbound_mapping_contains_no_product_or_gateway_branding() {
        let mapped = map_with(&request_with_tools(), options()).expect("mapping");
        let mut bytes = mapped.opening.clone();
        for blob in mapped.blobs.values() {
            bytes.extend_from_slice(blob);
        }
        let lower = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        let mut reference_name = String::from("mag");
        reference_name.push_str("pie");
        assert!(!lower.contains("hofund"));
        assert!(!lower.contains(&reference_name));
    }

    #[test]
    fn user_turn_protobuf_has_the_expected_field_numbers() {
        let expected = Pb::new()
            .string(1, "hello")
            .string(2, MESSAGE_ID)
            .varint(4, 1)
            .finish();
        let mapped = map_with(
            &Request::new("auto").with_messages(vec![message(Role::User, &["hello"])]),
            options(),
        )
        .expect("mapping");
        assert_eq!(mapped.blobs[&user_turn_id(&mapped)], expected);
    }
}
