//! Free-form object restoration on raw protocol replies (D-249).

use hofund_codec::FreeObjectRaw;
use hofund_ir::{Extensions, FreeObjectPaths, Tool};
use serde_json::{Value, json};

const WANT: &str = r#"{"calls":[{"name":"local","arguments":{"project":{"boundary":{"strategy":"rolling"}},"replace":true}}]}"#;

fn paths() -> FreeObjectPaths {
    FreeObjectPaths::from_tools(&[Tool {
        name: "tool_call".to_owned(),
        description: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "calls": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "arguments": {"type": "object"}
                        }
                    }
                }
            }
        }),
        extensions: Extensions::new(),
    }])
    .expect("paths")
}

fn said() -> String {
    let inner = &serde_json::from_str::<Value>(WANT).expect("want")["calls"][0]["arguments"];
    let mut value = serde_json::from_str::<Value>(WANT).expect("want");
    value["calls"][0]["arguments"] = Value::String(inner.to_string());
    value.to_string()
}

fn apply(protocol: &str, streaming: bool, body: &[u8]) -> Vec<u8> {
    let mut filter = FreeObjectRaw::new(protocol, streaming, paths()).expect("filter");
    let mut out = Vec::new();
    for chunk in body.chunks(7) {
        out.extend(filter.push(chunk));
    }
    out.extend(filter.finish());
    out
}

fn events(body: &[u8]) -> Vec<Value> {
    body.split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"data: "))
        .filter(|data| data.starts_with(b"{"))
        .map(|data| serde_json::from_slice(data).expect("event JSON"))
        .collect()
}

fn assert_same_json(got: &str, want: &str) {
    assert_eq!(
        serde_json::from_str::<Value>(got).expect("got JSON"),
        serde_json::from_str::<Value>(want).expect("want JSON")
    );
}

#[test]
fn d249_chat_whole_restores_only_the_declared_tool() {
    let arguments = serde_json::to_string(&said()).expect("quoted arguments");
    let body = format!(
        r#"{{"id":"c","choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{"id":"1","function":{{"name":"tool_call","arguments":{arguments}}}}},{{"id":"2","function":{{"name":"other","arguments":{arguments}}}}}]}}}}]}}"#
    );
    let out = apply("openai-chat", false, body.as_bytes());
    let value: Value = serde_json::from_slice(&out).expect("reply");
    let calls = value["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("calls");
    assert_same_json(
        calls[0]["function"]["arguments"].as_str().expect("args"),
        WANT,
    );
    assert_eq!(
        calls[1]["function"]["arguments"].as_str(),
        Some(said().as_str())
    );
}

#[test]
fn d249_responses_and_messages_whole_restore_tool_inputs() {
    let arguments = serde_json::to_string(&said()).expect("quoted arguments");
    let responses = format!(
        r#"{{"object":"response","output":[{{"type":"function_call","name":"tool_call","arguments":{arguments}}}]}}"#
    );
    let out = apply("openai-responses", false, responses.as_bytes());
    let value: Value = serde_json::from_slice(&out).expect("reply");
    assert_same_json(
        value["output"][0]["arguments"].as_str().expect("args"),
        WANT,
    );

    let messages = format!(
        r#"{{"content":[{{"type":"tool_use","name":"tool_call","input":{said}}}]}}"#,
        said = said()
    );
    let out = apply("anthropic-messages", false, messages.as_bytes());
    let value: Value = serde_json::from_slice(&out).expect("reply");
    assert_eq!(
        value["content"][0]["input"],
        serde_json::from_str::<Value>(WANT).expect("want")
    );
}

#[test]
fn d249_chat_stream_restores_arguments_split_across_frames_and_chunks() {
    let said = said();
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let mut body = String::from(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"tool_call\",\"arguments\":\"\"}}]}}]}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{}}}}}]}}}}]}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n");
    let out = apply("openai-chat", true, body.as_bytes());
    let mut arguments = String::new();
    for event in events(&out) {
        for call in event["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            arguments.push_str(call["function"]["arguments"].as_str().unwrap_or_default());
        }
    }
    assert_same_json(&arguments, WANT);
}

#[test]
fn d249_responses_stream_restores_deltas_done_items_and_completed_output() {
    let said = said();
    let quoted = serde_json::to_string(&said).expect("quoted");
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let item = format!(
        r#"{{"type":"function_call","id":"fc_1","name":"tool_call","arguments":{quoted}}}"#
    );
    let mut body = String::from(
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"tool_call\",\"arguments\":\"\"}}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":{}}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str(&format!(
        "event: response.function_call_arguments.done\ndata: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":0,\"arguments\":{quoted}}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"object\":\"response\",\"output\":[{item}]}}}}\n\n"
    ));
    let out = apply("openai-responses", true, body.as_bytes());
    let mut deltas = String::new();
    let mut complete = Vec::new();
    for event in events(&out) {
        match event["type"].as_str().unwrap_or_default() {
            "response.function_call_arguments.delta" => {
                deltas.push_str(event["delta"].as_str().unwrap_or_default());
            }
            "response.function_call_arguments.done" => {
                complete.push(event["arguments"].as_str().unwrap_or_default().to_owned());
            }
            "response.output_item.done" => {
                complete.push(
                    event["item"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            "response.completed" => {
                complete.push(
                    event["response"]["output"][0]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    assert_same_json(&deltas, WANT);
    assert_eq!(complete.len(), 3);
    for arguments in complete {
        assert_same_json(&arguments, WANT);
    }
}

#[test]
fn d249_messages_stream_restores_partial_json_at_block_stop() {
    let said = said();
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let mut body = String::from(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"tool_call\",\"input\":{}}}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    let out = apply("anthropic-messages", true, body.as_bytes());
    let mut arguments = String::new();
    for event in events(&out) {
        if event["delta"]["type"].as_str() == Some("input_json_delta") {
            arguments.push_str(event["delta"]["partial_json"].as_str().unwrap_or_default());
        }
    }
    assert_same_json(&arguments, WANT);
}

#[test]
fn d249_unchanged_frames_and_unsupported_protocols_pass_through() {
    let body = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\n";
    assert_eq!(apply("anthropic-messages", true, body), body);
    let body = br#"{"content":[{"type":"text","text":"hello"}]}"#;
    assert_eq!(apply("anthropic-messages", false, body), body);
    assert!(FreeObjectRaw::new("other", false, paths()).is_none());
}
