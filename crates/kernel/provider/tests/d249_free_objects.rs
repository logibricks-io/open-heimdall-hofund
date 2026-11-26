//! End-to-end free-object behavior through the upstream adapters (D-249).

use futures_util::StreamExt;
use hofund_ir::{ContentPart, Extensions, Message, Request, Response, Role, Tool, assemble};
use hofund_provider::{Anthropic, Completion, Endpoint, OpenAiCompat, OpenAiProtocol, Provider};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WANT: &str = r#"{"calls":[{"name":"local","arguments":{"project":{"boundary":{"strategy":"rolling"}},"replace":true}}]}"#;

fn request(model: &str, stream: bool) -> Request {
    let mut request = Request::new(model);
    request.stream = stream;
    request.params.max_tokens = Some(64);
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("go"));
    request.messages.push(message);
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
                        "properties": {
                            "name": {"type": "string"},
                            "arguments": {"type": "object", "description": "Arguments matching the tool schema."}
                        },
                        "required": ["name", "arguments"]
                    }
                }
            },
            "required": ["calls"]
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

fn expected(model: &str) -> String {
    if model.starts_with("gemini") {
        WANT.to_owned()
    } else {
        said()
    }
}

fn assert_same_json(got: &str, want: &str) {
    assert_eq!(
        serde_json::from_str::<Value>(got).expect("got JSON"),
        serde_json::from_str::<Value>(want).expect("want JSON")
    );
}

async fn raw_bytes(completion: Completion) -> Vec<u8> {
    let Completion::Raw(mut body) = completion.peel() else {
        panic!("raw completion");
    };
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

async fn ir_response(completion: Completion) -> Response {
    match completion.peel() {
        Completion::Unary(response) => *response,
        Completion::Stream(mut events) => {
            let mut all = Vec::new();
            while let Some(event) = events.next().await {
                all.push(event.expect("event"));
            }
            assemble(&all)
        }
        _ => panic!("IR completion"),
    }
}

fn response_arguments(response: &Response) -> String {
    response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call.arguments.to_text()),
            _ => None,
        })
        .expect("tool call")
}

fn sse_events(body: &[u8]) -> Vec<Value> {
    body.split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"data: "))
        .filter(|data| data.starts_with(b"{"))
        .map(|data| serde_json::from_slice(data).expect("event JSON"))
        .collect()
}

fn chat_whole() -> String {
    format!(
        r#"{{"id":"c","object":"chat.completion","model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"tool_call","arguments":{}}}}}]}},"finish_reason":"tool_calls"}}]}}"#,
        serde_json::to_string(&said()).expect("quoted")
    )
}

fn chat_stream() -> String {
    let said = said();
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let mut body = String::from(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"tool_call\",\"arguments\":\"\"}}]}}]}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{}}}}}]}}}}]}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n");
    body
}

fn messages_whole() -> String {
    format!(
        r#"{{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[{{"type":"tool_use","id":"toolu_1","name":"tool_call","input":{}}}],"stop_reason":"tool_use"}}"#,
        said()
    )
}

fn messages_stream() -> String {
    let said = said();
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let mut body = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[]}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"tool_call\",\"input\":{}}}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    body
}

fn responses_stream() -> String {
    let said = said();
    let quoted = serde_json::to_string(&said).expect("quoted");
    let pieces = [&said[..31], &said[31..73], &said[73..]];
    let item = format!(
        r#"{{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool_call","arguments":{quoted},"status":"completed"}}"#
    );
    let mut body = String::from(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"m\",\"output\":[]}}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"tool_call\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
    );
    for piece in pieces {
        body.push_str(&format!(
            "event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":{}}}\n\n",
            serde_json::to_string(piece).expect("piece")
        ));
    }
    body.push_str(&format!(
        "event: response.function_call_arguments.done\ndata: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"output_index\":0,\"arguments\":{quoted}}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"m\",\"output\":[{item}]}}}}\n\n"
    ));
    body
}

async fn mount_reply(protocol: &str, stream: bool) -> MockServer {
    let server = MockServer::start().await;
    let body = match (protocol, stream) {
        ("openai-chat", false) => chat_whole(),
        ("openai-chat", true) => chat_stream(),
        ("anthropic-messages", false) => messages_whole(),
        ("anthropic-messages", true) => messages_stream(),
        ("openai-responses", false) => r#"{"object":"response","output":[]}"#.to_owned(),
        ("openai-responses", true) => responses_stream(),
        _ => panic!("reply shape"),
    };
    let route = match protocol {
        "openai-chat" => "/chat/completions",
        "openai-responses" => "/responses",
        "anthropic-messages" => "/v1/messages",
        _ => panic!("protocol"),
    };
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

fn adapter(protocol: &str, server: &MockServer) -> Box<dyn Provider> {
    let endpoint = Endpoint::new("relay", server.uri());
    match protocol {
        "openai-chat" => Box::new(OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("chat")),
        "openai-responses" => {
            Box::new(OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("responses"))
        }
        "anthropic-messages" => Box::new(Anthropic::new(endpoint).expect("messages")),
        _ => panic!("protocol"),
    }
}

#[tokio::test]
async fn d249_adapters_send_free_objects_as_json_text() {
    for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
        for model in ["gemini-4-hydrogen", "gpt-5.5"] {
            let server = mount_reply(protocol, false).await;
            let provider = adapter(protocol, &server);
            provider
                .execute(request(model, false), protocol)
                .await
                .expect("execute");
            let requests = server.received_requests().await.expect("requests");
            let body: Value = serde_json::from_slice(&requests[0].body).expect("request JSON");
            let schema = match protocol {
                "openai-chat" => &body["tools"][0]["function"]["parameters"],
                "openai-responses" => &body["tools"][0]["parameters"],
                "anthropic-messages" => &body["tools"][0]["input_schema"],
                _ => panic!("protocol"),
            };
            let arguments = &schema["properties"]["calls"]["items"]["properties"]["arguments"];
            if model.starts_with("gemini") {
                assert_eq!(arguments["type"], "string", "{protocol}");
                assert_eq!(
                    arguments["description"],
                    "Arguments matching the tool schema. (A JSON object, written as JSON text.)"
                );
            } else {
                assert_eq!(arguments["type"], "object", "{protocol}");
            }
            assert_eq!(
                schema["properties"]["calls"]["items"]["required"],
                json!(["name", "arguments"])
            );
        }
    }
}

#[tokio::test]
async fn d249_chat_relayed_replies_restore_objects_whole_and_streamed() {
    for stream in [false, true] {
        for model in ["gemini-4-hydrogen", "gpt-5.5"] {
            let server = mount_reply("openai-chat", stream).await;
            let provider = adapter("openai-chat", &server);
            let completion = provider
                .execute(request(model, stream), "openai-chat")
                .await
                .expect("execute");
            let body = raw_bytes(completion).await;
            let arguments = if stream {
                let mut arguments = String::new();
                for event in sse_events(&body) {
                    for call in event["choices"][0]["delta"]["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        arguments
                            .push_str(call["function"]["arguments"].as_str().unwrap_or_default());
                    }
                }
                arguments
            } else {
                let value: Value = serde_json::from_slice(&body).expect("reply");
                value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .expect("arguments")
                    .to_owned()
            };
            assert_same_json(&arguments, &expected(model));
        }
    }
}

#[tokio::test]
async fn d249_messages_relayed_replies_restore_objects_whole_and_streamed() {
    for stream in [false, true] {
        for model in ["gemini-4-hydrogen", "gpt-5.5"] {
            let server = mount_reply("anthropic-messages", stream).await;
            let provider = adapter("anthropic-messages", &server);
            let completion = provider
                .execute(request(model, stream), "anthropic-messages")
                .await
                .expect("execute");
            let body = raw_bytes(completion).await;
            if stream {
                let mut arguments = String::new();
                for event in sse_events(&body) {
                    if event["delta"]["type"].as_str() == Some("input_json_delta") {
                        arguments
                            .push_str(event["delta"]["partial_json"].as_str().unwrap_or_default());
                    }
                }
                assert_same_json(&arguments, &expected(model));
            } else {
                let value: Value = serde_json::from_slice(&body).expect("reply");
                assert_same_json(&value["content"][0]["input"].to_string(), &expected(model));
            }
        }
    }
}

#[tokio::test]
async fn d249_responses_relayed_stream_restores_every_argument_surface() {
    for model in ["gemini-4-hydrogen", "gpt-5.5"] {
        let server = mount_reply("openai-responses", true).await;
        let provider = adapter("openai-responses", &server);
        let completion = provider
            .execute(request(model, true), "openai-responses")
            .await
            .expect("execute");
        let body = raw_bytes(completion).await;
        let mut deltas = String::new();
        let mut complete = Vec::new();
        for event in sse_events(&body) {
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
        assert_same_json(&deltas, &expected(model));
        assert_eq!(complete.len(), 3);
        for arguments in complete {
            assert_same_json(&arguments, &expected(model));
        }
    }
}

#[tokio::test]
async fn d249_chat_translated_from_messages_restores_objects_whole_and_streamed() {
    for stream in [false, true] {
        for model in ["gemini-4-hydrogen", "gpt-5.5"] {
            let server = mount_reply("anthropic-messages", stream).await;
            let provider = adapter("anthropic-messages", &server);
            let completion = provider
                .execute(request(model, stream), "openai-chat")
                .await
                .expect("execute");
            let response = ir_response(completion).await;
            assert_same_json(&response_arguments(&response), &expected(model));
        }
    }
}
