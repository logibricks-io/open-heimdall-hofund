//! Text-only GetChatMessage protobuf and Connect response decoding.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::inflate::compress;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::protobuf::{FLAG_GZIP, ProtoBuf, fields, frame, frames};

use super::metadata;

pub(crate) const GZIP_AT: usize = 8192;
const DEFAULT_MAX_TOKENS: u64 = 128_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptContent<'a> {
    Text(&'a str),
    Tools,
    Images,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Prompt<'a> {
    pub(crate) id: &'a str,
    pub(crate) role: PromptRole,
    pub(crate) content: PromptContent<'a>,
}

impl<'a> Prompt<'a> {
    pub(crate) fn text(id: &'a str, role: PromptRole, text: &'a str) -> Self {
        Self {
            id,
            role,
            content: PromptContent::Text(text),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChatRequest<'a> {
    pub(crate) session_token: &'a str,
    pub(crate) os: &'a str,
    pub(crate) model_uid: &'a str,
    pub(crate) prompts: &'a [Prompt<'a>],
    pub(crate) max_tokens: Option<u64>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EncodedChatRequest {
    pub(crate) body: Vec<u8>,
    pub(crate) gzipped: bool,
}

pub(crate) fn encode_request(request: &ChatRequest<'_>) -> Result<EncodedChatRequest, PluginError> {
    let mut message = ProtoBuf::new().bytes(1, &metadata(request.session_token, request.os));
    for prompt in request.prompts {
        message = message.bytes(3, &encode_prompt(prompt)?);
    }
    let sampling = sampling(request);
    let message = message
        .varint(7, 5)
        .bytes(8, &sampling)
        .string(21, request.model_uid)
        .finish();
    frame_request(&message)
}

fn encode_prompt(prompt: &Prompt<'_>) -> Result<Vec<u8>, PluginError> {
    if prompt.role == PromptRole::Tool || prompt.content == PromptContent::Tools {
        return Err(PluginError::config(
            "Devin tool history is not supported by the text-only chat codec",
        ));
    }
    if prompt.content == PromptContent::Images {
        return Err(PluginError::config(
            "Devin image prompts are not supported by the text-only chat codec",
        ));
    }
    let PromptContent::Text(text) = prompt.content else {
        unreachable!("tool and image content returned above");
    };
    let role = match prompt.role {
        PromptRole::User => 1,
        PromptRole::Assistant => 2,
        PromptRole::Tool => unreachable!("tool role returned above"),
    };
    let mut encoded = ProtoBuf::new().string(1, prompt.id).varint(2, role);
    if !text.is_empty() {
        encoded = encoded.string(3, text);
    }
    Ok(encoded.finish())
}

fn sampling(request: &ChatRequest<'_>) -> Vec<u8> {
    ProtoBuf::new()
        .varint(1, 1)
        .varint(2, request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS))
        .varint(3, 400)
        .double(5, above_zero(request.temperature.unwrap_or(1.0)))
        .varint(7, 40)
        .double(8, above_zero(request.top_p.unwrap_or(0.95)))
        .finish()
}

fn above_zero(value: f64) -> f64 {
    if value == 0.0 { 1e-6 } else { value }
}

fn frame_request(message: &[u8]) -> Result<EncodedChatRequest, PluginError> {
    if message.len() > GZIP_AT {
        return Ok(EncodedChatRequest {
            body: frame(FLAG_GZIP, &compress(message)?)?,
            gzipped: true,
        });
    }
    Ok(EncodedChatRequest {
        body: frame(0, message)?,
        gzipped: false,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatResponse {
    pub(crate) text: Vec<String>,
    pub(crate) error: Option<ConnectError>,
    pub(crate) saw_tool_call: bool,
    pub(crate) saw_usage: bool,
}

impl ChatResponse {
    pub(crate) fn has_text_stream(&self) -> bool {
        !self.text.is_empty()
    }

    pub(crate) fn is_empty_reply(&self) -> bool {
        self.error.is_none() && self.text.is_empty() && !self.saw_tool_call
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError {
    pub(crate) code: String,
    pub(crate) message: String,
}

pub(crate) fn decode_response(body: &[u8]) -> Result<ChatResponse, PluginError> {
    let mut response = ChatResponse::default();
    for next in frames(body) {
        let frame = next?;
        if frame.is_end() {
            response.error = end_error(&frame.payload)?;
            break;
        }
        for field in fields(&frame.payload) {
            match field.number {
                3 => {
                    if let Some(text) = field.as_string()
                        && !text.is_empty()
                    {
                        response.text.push(text);
                    }
                }
                6 if field.as_bytes().is_some() => response.saw_tool_call = true,
                7 if field.as_bytes().is_some() => response.saw_usage = true,
                _ => {}
            }
        }
    }
    Ok(response)
}

fn end_error(payload: &[u8]) -> Result<Option<ConnectError>, PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_slice(payload)
        .map_err(|error| PluginError::decode(format!("invalid Connect end JSON: {error}")))?;
    let object = if value.get("error").is_some_and(Value::is_object) {
        value.get("error").unwrap_or(&Value::Null)
    } else {
        &value
    };
    let Some(code) = object.get("code").and_then(Value::as_str) else {
        return Ok(None);
    };
    if code.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConnectError {
        code: code.to_owned(),
        message: object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginError;
    use hofund_plugin_sdk::inflate::compress;
    use hofund_plugin_sdk::protobuf::{FLAG_END, FLAG_GZIP, WireValue, deframe, fields, frame};

    use super::{
        ChatRequest, ConnectError, GZIP_AT, Prompt, PromptContent, PromptRole, decode_response,
        encode_request, frame_request,
    };

    const EMPTY_REPLY: &[u8] = crate::connect::testdata::EMPTY_REPLY;

    fn request<'a>(prompts: &'a [Prompt<'a>]) -> ChatRequest<'a> {
        ChatRequest {
            session_token: "session-token",
            os: "linux",
            model_uid: "swe-2-high",
            prompts,
            max_tokens: None,
            temperature: None,
            top_p: None,
        }
    }

    #[test]
    fn text_only_request_uses_the_chat_field_table() {
        let prompts = [
            Prompt::text("user-id", PromptRole::User, "hello"),
            Prompt::text("assistant-id", PromptRole::Assistant, "answer"),
        ];
        let encoded = encode_request(&request(&prompts)).expect("encode");
        assert!(!encoded.gzipped);
        let frame = deframe(&encoded.body).expect("request frame");
        assert_eq!(frame.flags, 0);
        let outer = fields(&frame.payload);
        assert_eq!(outer[0].number, 1);
        let prompts: Vec<_> = outer.iter().filter(|field| field.number == 3).collect();
        assert_eq!(prompts.len(), 2);
        let first = fields(prompts[0].as_bytes().expect("prompt"));
        assert_eq!(first[0].as_string().as_deref(), Some("user-id"));
        assert_eq!(first[1].as_varint(), Some(1));
        assert_eq!(first[2].as_string().as_deref(), Some("hello"));
        assert_eq!(first.len(), 3);
        let second = fields(prompts[1].as_bytes().expect("prompt"));
        assert_eq!(second[1].as_varint(), Some(2));
        assert_eq!(
            outer
                .iter()
                .find(|field| field.number == 7)
                .and_then(|field| field.as_varint()),
            Some(5)
        );
        assert_eq!(
            outer
                .iter()
                .find(|field| field.number == 21)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some("swe-2-high")
        );
        let sampling = fields(
            outer
                .iter()
                .find(|field| field.number == 8)
                .and_then(|field| field.as_bytes())
                .expect("sampling"),
        );
        assert_eq!(sampling[1].as_varint(), Some(128_000));
        assert_eq!(sampling[3].value, WireValue::Fixed64(1.0_f64.to_bits()));
        assert_eq!(sampling[5].value, WireValue::Fixed64(0.95_f64.to_bits()));
        assert!(!outer.iter().any(|field| field.number == 10));
    }

    #[test]
    fn zero_sampling_values_become_one_millionth() {
        let prompts = [Prompt::text("id", PromptRole::User, "hello")];
        let mut request = request(&prompts);
        request.temperature = Some(0.0);
        request.top_p = Some(0.0);
        request.max_tokens = Some(42);
        let encoded = encode_request(&request).expect("encode");
        let frame = deframe(&encoded.body).expect("frame");
        let outer = fields(&frame.payload);
        let sampling = fields(
            outer
                .iter()
                .find(|field| field.number == 8)
                .and_then(|field| field.as_bytes())
                .expect("sampling"),
        );
        assert_eq!(sampling[1].as_varint(), Some(42));
        assert_eq!(sampling[3].value, WireValue::Fixed64(1e-6_f64.to_bits()));
        assert_eq!(sampling[5].value, WireValue::Fixed64(1e-6_f64.to_bits()));
    }

    #[test]
    fn tool_and_image_prompts_are_refused_not_dropped() {
        for content in [PromptContent::Tools, PromptContent::Images] {
            let prompts = [Prompt {
                id: "id",
                role: PromptRole::User,
                content,
            }];
            let error = encode_request(&request(&prompts)).expect_err("unsupported content");
            assert!(error.message().contains("not supported"));
        }
        let prompts = [Prompt::text("id", PromptRole::Tool, "tool result")];
        let error = encode_request(&request(&prompts)).expect_err("tool role");
        assert!(error.message().contains("tool history"));
    }

    #[test]
    fn gzip_threshold_matches_the_captured_cli_sizes() {
        let plain = frame_request(&vec![0; 8083]).expect("plain");
        assert!(!plain.gzipped);
        assert_eq!(deframe(&plain.body).expect("plain frame").flags, 0);
        assert_eq!(GZIP_AT, 8192);

        let gzipped = frame_request(&vec![0; 8333]).expect("gzip");
        assert!(gzipped.gzipped);
        let decoded = deframe(&gzipped.body).expect("gzip frame");
        assert_eq!(decoded.flags, FLAG_GZIP);
        assert_eq!(decoded.payload, vec![0; 8333]);
    }

    #[test]
    fn outbound_chat_bytes_do_not_contain_product_names() {
        let prompts = [Prompt::text("id", PromptRole::User, "hello")];
        let encoded = encode_request(&request(&prompts)).expect("encode");
        let lower = String::from_utf8_lossy(&encoded.body).to_ascii_lowercase();
        assert!(!lower.contains("hofund"));
        let reference = String::from("mag") + "pie";
        assert!(!lower.contains(&reference));
    }

    #[test]
    fn response_surfaces_nonempty_text_deltas_in_order() {
        let first = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, "hello")
            .string(3, "")
            .string(9, "not text in this slice")
            .finish();
        let second = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, " world")
            .finish();
        let mut body = frame(0, &first).expect("first");
        body.extend(frame(0, &second).expect("second"));
        body.extend(frame(FLAG_END, b"{}").expect("end"));
        let decoded = decode_response(&body).expect("decode");
        assert_eq!(decoded.text, ["hello", " world"]);
        assert!(decoded.has_text_stream());
        assert!(!decoded.is_empty_reply());
        assert_eq!(decoded.error, None);
    }

    #[test]
    fn empty_reply_golden_is_usage_only_not_a_text_stream() {
        let decoded = decode_response(EMPTY_REPLY).expect("golden");
        assert!(decoded.text.is_empty());
        assert!(!decoded.has_text_stream());
        assert!(decoded.saw_usage);
        assert!(!decoded.saw_tool_call);
        assert!(decoded.is_empty_reply());
        assert_eq!(decoded.error, None);
    }

    #[test]
    fn direct_and_nested_end_json_errors_decode() {
        for (json, expected) in [
            (
                br#"{"code":"unauthenticated","message":"bad key"}"#.as_slice(),
                ConnectError {
                    code: "unauthenticated".into(),
                    message: "bad key".into(),
                },
            ),
            (
                br#"{"error":{"code":"resource_exhausted","message":"daily quota used up"}}"#,
                ConnectError {
                    code: "resource_exhausted".into(),
                    message: "daily quota used up".into(),
                },
            ),
        ] {
            let decoded = decode_response(&frame(FLAG_END, json).expect("end")).expect("decode");
            assert_eq!(decoded.error, Some(expected));
            assert!(!decoded.is_empty_reply());
        }
    }

    #[test]
    fn gzipped_end_json_error_decodes_after_inflation() {
        let json = br#"{"error":{"code":"unavailable","message":"try later"}}"#;
        let body = frame(FLAG_GZIP | FLAG_END, &compress(json).expect("gzip")).expect("frame");
        let decoded = decode_response(&body).expect("decode");
        assert_eq!(
            decoded.error,
            Some(ConnectError {
                code: "unavailable".into(),
                message: "try later".into(),
            })
        );
    }

    #[test]
    fn malformed_protobuf_tail_keeps_an_already_decoded_text_delta() {
        let mut message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, "prefix")
            .finish();
        message.extend_from_slice(&[(4 << 3) | 2, 20, 0]);
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("partial");
        assert_eq!(decoded.text, ["prefix"]);
    }

    #[test]
    fn truncated_connect_frame_is_an_error() {
        let body = frame(0, b"payload").expect("frame");
        let error = decode_response(&body[..body.len() - 1]).expect_err("truncated");
        assert_eq!(error.message(), "unexpected EOF");
    }

    #[test]
    fn invalid_end_json_is_a_decode_error() {
        let error = decode_response(&frame(FLAG_END, b"not-json").expect("frame"))
            .expect_err("invalid JSON");
        assert!(error.message().contains("invalid Connect end JSON"));
    }

    #[test]
    fn tool_only_response_is_not_misclassified_as_empty() {
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .bytes(6, b"later slice")
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        assert!(decoded.saw_tool_call);
        assert!(!decoded.has_text_stream());
        assert!(!decoded.is_empty_reply());
    }

    #[test]
    fn tool_and_usage_markers_require_length_delimited_fields() {
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .varint(6, 1)
            .varint(7, 1)
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        assert!(!decoded.saw_tool_call);
        assert!(!decoded.saw_usage);
        assert!(decoded.is_empty_reply());
    }

    #[test]
    fn empty_end_json_is_clean_and_still_empty() {
        let decoded = decode_response(&frame(FLAG_END, b"{}").expect("frame")).expect("decode");
        assert_eq!(decoded.error, None);
        assert!(decoded.is_empty_reply());
    }

    #[test]
    fn request_rejection_uses_plugin_errors() {
        let prompts = [Prompt {
            id: "id",
            role: PromptRole::User,
            content: PromptContent::Tools,
        }];
        let error: PluginError = encode_request(&request(&prompts)).expect_err("tools");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Config);
    }
}
