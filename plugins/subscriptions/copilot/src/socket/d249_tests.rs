use hofund_kit_subscription::block_on;
use hofund_plugin_api::{Completion, ContentPart, Extensions, ProtocolId, Request, Tool};
use hofund_plugin_sdk::json::{self, Value, json};

use super::decode_sse;

const WANT: &str = r#"{"calls":[{"arguments":{"nested":{"ok":true}}}]}"#;

fn request() -> Request {
    let mut request = Request::new("gemini-4-hydrogen");
    request.tools.push(Tool {
        name: "tool_call".to_owned(),
        description: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "calls": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"arguments": {"type": "object"}}
                    }
                }
            }
        }),
        extensions: Extensions::new(),
    });
    request
}

fn said() -> String {
    let inner = &json::from_str::<Value>(WANT).expect("want")["calls"][0]["arguments"];
    let mut value = json::from_str::<Value>(WANT).expect("want");
    value["calls"][0]["arguments"] = Value::String(inner.to_string());
    value.to_string()
}

fn responses() -> Vec<u8> {
    let said = said();
    let quoted = json::to_string(&said).expect("quoted");
    format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"tool_call\",\"arguments\":\"\"}}}}\n\nevent: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":{}}}\n\nevent: response.function_call_arguments.done\ndata: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":0,\"arguments\":{quoted}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}}}\n\n",
        json::to_string(&said).expect("delta")
    )
    .into_bytes()
}

fn assert_want(arguments: &str) {
    assert_eq!(
        json::from_str::<Value>(arguments).expect("arguments"),
        json::from_str::<Value>(WANT).expect("want")
    );
}

#[test]
fn d249_copilot_websocket_relay_restores_json_text_objects() {
    block_on(async {
        let completion = decode_sse(responses(), ProtocolId::OPENAI_RESPONSES, true, &request())
            .await
            .expect("completion");
        let Completion::Raw(mut body) = completion.peel() else {
            panic!("raw reply");
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        let mut arguments = String::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            let Some(data) = line.strip_prefix(b"data: ") else {
                continue;
            };
            let Ok(event) = json::from_slice::<Value>(data) else {
                continue;
            };
            if event["type"].as_str() == Some("response.function_call_arguments.delta") {
                arguments.push_str(event["delta"].as_str().unwrap_or_default());
            }
        }
        assert_want(&arguments);
    });
}

#[test]
fn d249_copilot_websocket_fold_restores_json_text_objects() {
    block_on(async {
        let completion = decode_sse(responses(), ProtocolId::OPENAI_CHAT, false, &request())
            .await
            .expect("completion");
        let Completion::Unary(response) = completion.peel() else {
            panic!("unary reply");
        };
        let arguments = response
            .message
            .content
            .iter()
            .find_map(|part| match part {
                ContentPart::ToolCall(call) => Some(call.arguments.to_text()),
                _ => None,
            })
            .expect("tool call");
        assert_want(&arguments);
    });
}
