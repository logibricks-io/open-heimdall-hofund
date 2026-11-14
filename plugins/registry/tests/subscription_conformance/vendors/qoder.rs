use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_qoder::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription};
use crate::matrix::Cell;

fn envelope(chunk: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"statusCodeValue": 200, "body": chunk.to_string()})
    )
}

fn response(tools: bool) -> Vec<u8> {
    let delta = if tools {
        json!({"role": "assistant", "tool_calls": [{
            "index": 0, "id": "call-1", "type": "function",
            "function": {"name": "lookup", "arguments": "{\"query\":\"fixture\"}"}
        }]})
    } else {
        json!({"role": "assistant", "content": "matrix-ok"})
    };
    let chunk = json!({"choices": [{"index": 0, "delta": delta, "finish_reason": if tools { "tool_calls" } else { "stop" }}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
    format!(
        "{}data: {{\"statusCodeValue\":200,\"body\":\"[DONE]\"}}\n\n",
        envelope(chunk)
    )
    .into_bytes()
}

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("qoder", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("uid", "matrix-user")
            .with_text("email", "matrix@example.test")
            .with_secret("access_token", &secret("matrix-access"))
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_number("expires_at", 4_000_000_000_000_u64)
            .with_text("machine_id", "matrix-machine")
            .with_text("site", "")
            .with_text("vpc", "")
            .with_secret("device_token", &secret(""))
            .with_secret("device_refresh_token", &secret("")),
    )
    .await;
    host.http().enqueue(
        include_bytes!("../../../../subscriptions/qoder/src/fixtures/models.json").as_slice(),
    );
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        response(cell.tools),
    );
    let endpoints = Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: "https://qoder.test".to_owned(),
        openapi_cn: "https://qoder-cn.test".to_owned(),
        chat_global: "https://qoder-chat.test".to_owned(),
        chat_cn: "https://qoder-cn-chat.test".to_owned(),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
