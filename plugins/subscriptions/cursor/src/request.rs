//! Text-only canonical-request mapping for one Cursor Run.

use std::collections::{BTreeMap, HashMap};

use hofund_plugin_api::{ContentPart, Message, PluginError, Request, Role};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};

use crate::proto::{self, AgentRunParams, Pb};
use crate::run::{Limits, RunArgs};

/// A fully owned request opening and every blob its state references.
#[derive(Debug)]
pub(crate) struct MappedRequest {
    pub(crate) opening: Vec<u8>,
    pub(crate) blobs: HashMap<Vec<u8>, Vec<u8>>,
    os: String,
    temp_dir: String,
}

impl MappedRequest {
    /// Borrows the owned mapping in the shape the Run transport takes.
    pub(crate) fn args(&self) -> RunArgs<'_> {
        RunArgs {
            opening: self.opening.clone(),
            team_id: None,
            blobs: &self.blobs,
            os: proto::goos_name_for(&self.os),
            temp_dir: &self.temp_dir,
            limits: Limits::default(),
        }
    }
}

/// Deterministic mapping inputs used by unit tests.
#[cfg(test)]
struct MapOptions<'a> {
    os: &'a str,
    temp_dir: &'a str,
    conversation_id: &'a str,
    message_id: &'a str,
}

/// Maps a request with a fresh conversation, message, and per-execute temp path.
pub(crate) fn map(request: &Request) -> Result<MappedRequest, PluginError> {
    let message_id = new_uuid()?;
    let conversation_id = new_uuid()?;
    let temp_dir = std::env::temp_dir()
        .join(format!("cursor-{message_id}"))
        .to_string_lossy()
        .into_owned();
    build(
        request,
        &conversation_id,
        &message_id,
        std::env::consts::OS,
        &temp_dir,
    )
}

#[cfg(test)]
fn map_with(request: &Request, options: MapOptions<'_>) -> Result<MappedRequest, PluginError> {
    build(
        request,
        options.conversation_id,
        options.message_id,
        options.os,
        options.temp_dir,
    )
}

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
) -> Result<MappedRequest, PluginError> {
    reject_unsupported(request)?;

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
            Role::Assistant => transcript.push(message),
            Role::Tool => {}
        }
    }
    if !system_text.is_empty() {
        let blob = system_blob(&system_text.join("\n"));
        transcript_blob_ids.push(insert_blob(&mut blobs, blob));
    }
    for message in transcript {
        let blob = transcript_blob(message);
        transcript_blob_ids.push(insert_blob(&mut blobs, blob));
    }

    let user_turn = Pb::new()
        .string(1, last_user_text.as_deref().unwrap_or("."))
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
        tools: &[],
        inline_images: false,
    }));
    Ok(MappedRequest {
        opening,
        blobs,
        os: os.to_owned(),
        temp_dir: temp_dir.to_owned(),
    })
}

fn reject_unsupported(request: &Request) -> Result<(), PluginError> {
    let has_tool_part = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_) | ContentPart::ToolResult(_)))
    });
    if !request.tools.is_empty() || has_tool_part {
        return Err(PluginError::status_error(404, "tools are not served").without_cooldown());
    }
    let has_image = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Image(_)))
    });
    if has_image {
        return Err(PluginError::status_error(404, "images are not served").without_cooldown());
    }
    Ok(())
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

fn transcript_blob(message: &Message) -> Vec<u8> {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        _ => unreachable!("only user and assistant messages reach transcript blobs"),
    };
    let content = message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(part) => Some(object([
                ("text", Value::String(part.text.clone())),
                ("type", Value::String("text".to_owned())),
            ])),
            _ => None,
        })
        .collect();
    object([
        ("content", Value::Array(content)),
        ("role", Value::String(role.to_owned())),
    ])
    .to_string()
    .into_bytes()
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
        }
    }

    fn message(role: Role, parts: &[&str]) -> Message {
        let mut message = Message::new(role);
        message
            .content
            .extend(parts.iter().map(|text| ContentPart::text(*text)));
        message
    }

    fn field<'a>(fields: &'a [Field], number: u32) -> &'a Field {
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
            json!([{"text": "answer", "type": "text"}])
        );
        assert!(!assistant.to_string().contains("hidden"));

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
        assert_eq!(mapped.args().os, "darwin");
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

    #[test]
    fn tools_are_rejected_before_images() {
        let mut request: Request = from_value(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
        }))
        .expect("request");
        request.messages[0]
            .content
            .push(ContentPart::Image(ImagePart {
                source: ImageSource::Url {
                    url: "https://image.test/a.png".to_owned(),
                },
                detail: None,
                extensions: Default::default(),
            }));
        let error = map_with(&request, options()).expect_err("tools are not served");
        assert_eq!(error.status(), Some(404));
        assert!(error.message().contains("tools are not served"));
        assert!(!error.cools());
    }

    #[test]
    fn tool_parts_are_rejected_and_images_have_their_own_404() {
        for content in [
            json!({"type": "tool_call", "id": "call-1", "name": "lookup", "arguments": {"form": "raw", "text": "{}"}}),
            json!({"type": "tool_result", "id": "call-1", "content": [{"type": "text", "text": "done"}]}),
        ] {
            let request: Request = from_value(json!({
                "model": "auto",
                "messages": [{"role": "user", "content": [content]}]
            }))
            .expect("request");
            let error = map_with(&request, options()).expect_err("tool part");
            assert_eq!(error.status(), Some(404));
            assert!(error.message().contains("tools are not served"));
            assert!(!error.cools());
        }

        let request: Request = from_value(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": [{
                "type": "image",
                "source": {"type": "url", "url": "https://image.test/a.png"}
            }]}]
        }))
        .expect("request");
        let error = map_with(&request, options()).expect_err("image part");
        assert_eq!(error.status(), Some(404));
        assert!(error.message().contains("images are not served"));
        assert!(!error.cools());
    }

    #[test]
    fn outbound_mapping_contains_no_product_or_gateway_branding() {
        let mapped = map_with(
            &Request::new("auto").with_messages(vec![message(Role::User, &["hello"])]),
            options(),
        )
        .expect("mapping");
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
