use std::sync::Arc;

use hofund_kit_subscription::TestHost;
use hofund_plugin_api::{
    AccountId, BlockStart, Completion, ContentPart, HostContext, PluginId, PluginKind, RawTable,
    Request, SecretString, StreamEvent, Subscription, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::futures::StreamExt as _;
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::matrix::{Cell, Shape};

pub const ACCOUNT: &str = "a-0000000000000001";

pub fn host(vendor: &'static str, dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static(vendor),
        dir.path(),
    )
}

pub fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

pub async fn seed(host: &TestHost, blob: SecretObject) -> AccountId {
    let id = AccountId::from_static(ACCOUNT);
    seed_as(host, id.clone(), blob).await;
    id
}

pub async fn seed_as(host: &TestHost, id: AccountId, blob: SecretObject) {
    let accounts = host.context().accounts();
    let account = accounts
        .account(id, "matrix@example.test")
        .expect("account");
    accounts
        .upsert(account, blob.to_secret())
        .await
        .expect("seed");
}

pub fn subscription<F: SubscriptionFactory>(
    factory: &F,
    host: &TestHost,
    raw: RawTable,
) -> Arc<dyn Subscription> {
    let config = factory.validate(&raw).expect("config");
    factory.build(&config, host.context()).expect("build")
}

pub fn upstream(
    subscription: &Arc<dyn Subscription>,
    account: &AccountId,
    context: &HostContext,
) -> Arc<dyn UpstreamProvider> {
    subscription.upstream(account, context).expect("upstream")
}

pub async fn execute_async(
    subscription: &Arc<dyn Subscription>,
    account: &AccountId,
    host: &TestHost,
    cell: Cell,
) -> Result<(), String> {
    let upstream = upstream(subscription, account, host.context());
    execute_with_async(&upstream, cell).await
}

pub async fn execute_with_async(
    upstream: &Arc<dyn UpstreamProvider>,
    cell: Cell,
) -> Result<(), String> {
    let request = request(cell);
    let completion = upstream
        .execute(request, cell.shape.protocol())
        .await
        .map_err(|error| format!("execute failed with kind {:?}", error.kind()))?;
    let outcome = collect(completion).await?;
    if cell.tools {
        if outcome.tool {
            Ok(())
        } else {
            Err("completion did not contain a tool call".to_owned())
        }
    } else if outcome.text {
        Ok(())
    } else {
        Err("completion did not contain text".to_owned())
    }
}

fn request(cell: Cell) -> Request {
    let mut value = json!({
        "model": cell.model,
        "stream": cell.stream,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "conformance"}]}]
    });
    if cell.tools {
        value["tools"] = json!([{
            "name": "lookup",
            "description": "look up a fixture value",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
        }]);
    }
    hofund_plugin_sdk::json::from_value(value).expect("request")
}

#[derive(Default)]
struct Outcome {
    text: bool,
    tool: bool,
}

async fn collect(completion: Completion) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    match completion {
        Completion::Unary(response) => {
            for part in &response.message.content {
                match part {
                    ContentPart::Text(text) => outcome.text |= !text.text.is_empty(),
                    ContentPart::ToolCall(_) => outcome.tool = true,
                    _ => {}
                }
            }
        }
        Completion::Stream(mut stream) => {
            while let Some(event) = stream.next().await {
                match event
                    .map_err(|error| format!("stream failed with kind {:?}", error.kind()))?
                {
                    StreamEvent::TextDelta(delta) => outcome.text |= !delta.text.is_empty(),
                    StreamEvent::ContentBlockStart(start)
                        if matches!(start.block, BlockStart::ToolCall { .. }) =>
                    {
                        outcome.tool = true;
                    }
                    _ => {}
                }
            }
        }
        Completion::Raw(mut body) => {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                bytes.extend_from_slice(
                    &chunk.map_err(|error| format!("body failed with kind {:?}", error.kind()))?,
                );
            }
            let text = String::from_utf8_lossy(&bytes);
            outcome.text = text.contains("matrix-ok");
            outcome.tool = text.contains("lookup");
        }
        _ => return Err("unknown completion variant".to_owned()),
    }
    Ok(outcome)
}

pub fn wire_response(shape: Shape, stream: bool, tools: bool) -> Vec<u8> {
    if stream {
        match shape {
            Shape::Chat => chat_stream(tools),
            Shape::Responses => responses_stream(tools),
            Shape::Messages => messages_stream(tools),
            Shape::Generative => generative_stream(tools, false),
        }
    } else {
        json::to_vec(&unary(shape, tools)).expect("response")
    }
}

pub fn unary(shape: Shape, tools: bool) -> Value {
    match shape {
        Shape::Chat => chat_unary(tools),
        Shape::Responses => responses_unary(tools),
        Shape::Messages => messages_unary(tools),
        Shape::Generative => generative_unary(tools),
    }
}

fn chat_unary(tools: bool) -> Value {
    let (message, finish) = if tools {
        (
            json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "lookup", "arguments": "{\"query\":\"fixture\"}"}
            }]}),
            "tool_calls",
        )
    } else {
        (json!({"role": "assistant", "content": "matrix-ok"}), "stop")
    };
    json!({
        "id": "chat-1", "object": "chat.completion", "created": 1, "model": "fixture",
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

fn responses_unary(tools: bool) -> Value {
    let output = if tools {
        json!([{
            "id": "fc-1", "type": "function_call", "status": "completed",
            "call_id": "call-1", "name": "lookup", "arguments": "{\"query\":\"fixture\"}"
        }])
    } else {
        json!([{
            "id": "msg-1", "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": "matrix-ok", "annotations": []}]
        }])
    };
    json!({
        "id": "resp-1", "object": "response", "status": "completed", "model": "fixture",
        "output": output,
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    })
}

fn messages_unary(tools: bool) -> Value {
    let (content, stop) = if tools {
        (
            json!([{
                "type": "tool_use", "id": "call-1", "name": "lookup",
                "input": {"query": "fixture"}
            }]),
            "tool_use",
        )
    } else {
        (json!([{"type": "text", "text": "matrix-ok"}]), "end_turn")
    };
    json!({
        "id": "msg-1", "type": "message", "role": "assistant", "model": "fixture",
        "content": content, "stop_reason": stop,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

fn generative_unary(tools: bool) -> Value {
    let part = if tools {
        json!({"functionCall": {"name": "lookup", "args": {"query": "fixture"}}})
    } else {
        json!({"text": "matrix-ok"})
    };
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [part]}, "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
    })
}

fn sse(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn chat_stream(tools: bool) -> Vec<u8> {
    let (delta, finish) = if tools {
        (
            json!({"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call-1", "type": "function",
                "function": {"name": "lookup", "arguments": "{\"query\":\"fixture\"}"}
            }]}),
            "tool_calls",
        )
    } else {
        (json!({"role": "assistant", "content": "matrix-ok"}), "stop")
    };
    let chunk = json!({
        "id": "chat-1", "object": "chat.completion.chunk", "created": 1, "model": "fixture",
        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
    });
    let done = json!({
        "id": "chat-1", "object": "chat.completion.chunk", "created": 1, "model": "fixture",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
    });
    format!(
        "{}{}{}data: [DONE]\n\n",
        sse(chunk),
        sse(done),
        sse(json!({"usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}))
    )
    .into_bytes()
}

fn responses_stream(tools: bool) -> Vec<u8> {
    let mut out = sse(json!({"type": "response.created", "response": {
        "id": "resp-1", "object": "response", "status": "in_progress", "model": "fixture", "output": []
    }}));
    if tools {
        out.push_str(&sse(json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "fc-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{\"query\":\"fixture\"}"}
        })));
    } else {
        out.push_str(&sse(json!({
            "type": "response.output_text.delta", "item_id": "msg-1",
            "output_index": 0, "content_index": 0, "delta": "matrix-ok"
        })));
    }
    out.push_str(&sse(
        json!({"type": "response.completed", "response": responses_unary(tools)}),
    ));
    out.into_bytes()
}

fn messages_stream(tools: bool) -> Vec<u8> {
    let mut out = sse(json!({"type": "message_start", "message": {
        "id": "msg-1", "type": "message", "role": "assistant", "model": "fixture",
        "content": [], "usage": {"input_tokens": 1, "output_tokens": 0}
    }}));
    if tools {
        out.push_str(&sse(
            json!({"type": "content_block_start", "index": 0, "content_block": {
                "type": "tool_use", "id": "call-1", "name": "lookup", "input": {}
            }}),
        ));
        out.push_str(&sse(
            json!({"type": "content_block_delta", "index": 0, "delta": {
                "type": "input_json_delta", "partial_json": "{\"query\":\"fixture\"}"
            }}),
        ));
    } else {
        out.push_str(&sse(
            json!({"type": "content_block_start", "index": 0, "content_block": {
                "type": "text", "text": ""
            }}),
        ));
        out.push_str(&sse(
            json!({"type": "content_block_delta", "index": 0, "delta": {
                "type": "text_delta", "text": "matrix-ok"
            }}),
        ));
    }
    out.push_str(&sse(json!({"type": "content_block_stop", "index": 0})));
    out.push_str(&sse(json!({"type": "message_delta", "delta": {
        "stop_reason": if tools { "tool_use" } else { "end_turn" }
    }, "usage": {"output_tokens": 1}})));
    out.push_str(&sse(json!({"type": "message_stop"})));
    out.into_bytes()
}

pub fn generative_stream(tools: bool, code_assist: bool) -> Vec<u8> {
    let value = if code_assist {
        json!({"response": generative_unary(tools)})
    } else {
        generative_unary(tools)
    };
    sse(value).into_bytes()
}

pub fn code_assist_response(stream: bool, tools: bool) -> Vec<u8> {
    if stream {
        generative_stream(tools, true)
    } else {
        json::to_vec(&json!({"response": generative_unary(tools), "traceId": "trace-1"}))
            .expect("response")
    }
}
