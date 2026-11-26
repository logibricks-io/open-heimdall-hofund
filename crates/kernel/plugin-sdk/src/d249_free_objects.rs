use futures_util::StreamExt;
use hofund_plugin_api::{
    BodyStream, Completion, ContentPart, Extensions, ProtocolId, Request, Tool,
};
use serde_json::{Value, json};

use crate::transport::HttpResponse;
use crate::wire;

const WANT: &str = r#"{"calls":[{"name":"local","arguments":{"project":{"boundary":{"strategy":"rolling"}},"replace":true}}]}"#;

fn request(model: &str) -> Request {
    let mut request = Request::new(model);
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
    let inner = &serde_json::from_str::<Value>(WANT).expect("want")["calls"][0]["arguments"];
    let mut value = serde_json::from_str::<Value>(WANT).expect("want");
    value["calls"][0]["arguments"] = Value::String(inner.to_string());
    value.to_string()
}

fn responses() -> Vec<u8> {
    let said = said();
    let quoted = serde_json::to_string(&said).expect("quoted");
    let item = format!(
        r#"{{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool_call","arguments":{quoted},"status":"completed"}}"#
    );
    format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"tool_call\",\"arguments\":\"\",\"status\":\"in_progress\"}}}}\n\nevent: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":{}}}\n\nevent: response.function_call_arguments.done\ndata: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":0,\"arguments\":{quoted}}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"m\",\"output\":[{item}]}}}}\n\n",
        serde_json::to_string(&said).expect("delta")
    )
    .into_bytes()
}

fn assert_want(arguments: &str) {
    assert_eq!(
        serde_json::from_str::<Value>(arguments).expect("arguments"),
        serde_json::from_str::<Value>(WANT).expect("want")
    );
}

#[tokio::test]
async fn d249_request_aware_decode_restores_relayed_responses_stream() {
    let completion = wire::decode_upstream_response_for_request(
        HttpResponse::new(BodyStream::from_bytes(responses())),
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::OPENAI_RESPONSES,
        true,
        &request("gemini-4-hydrogen"),
    )
    .await
    .expect("completion");
    let Completion::Raw(mut body) = completion.peel() else {
        panic!("raw reply");
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let mut deltas = String::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some(data) = line.strip_prefix(b"data: ") else {
            continue;
        };
        let Ok(event) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        if event["type"].as_str() == Some("response.function_call_arguments.delta") {
            deltas.push_str(event["delta"].as_str().unwrap_or_default());
        }
    }
    assert_want(&deltas);
}

#[tokio::test]
async fn d249_request_aware_decode_restores_translated_stream_and_folded_reply() {
    for client_stream in [true, false] {
        let completion = wire::decode_upstream_response_for_request(
            HttpResponse::new(BodyStream::from_bytes(responses())),
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::OPENAI_CHAT,
            client_stream,
            &request("gemini-4-hydrogen"),
        )
        .await
        .expect("completion");
        let response = match completion.peel() {
            Completion::Unary(response) => *response,
            Completion::Stream(mut events) => {
                let mut all = Vec::new();
                while let Some(event) = events.next().await {
                    all.push(event.expect("event"));
                }
                hofund_ir::assemble(&all)
            }
            _ => panic!("IR reply"),
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
    }
}
