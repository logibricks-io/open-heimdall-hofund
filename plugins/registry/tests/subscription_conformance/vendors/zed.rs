use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_zed::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::{Cell, Shape};

fn provider(shape: Shape) -> &'static str {
    match shape {
        Shape::Chat => "x_ai",
        Shape::Responses => "open_ai",
        Shape::Messages => "anthropic",
        Shape::Generative => "google",
    }
}

fn ndjson(shape: Shape, _stream: bool, tools: bool) -> Vec<u8> {
    let sse = String::from_utf8(wire_response(shape, true, tools)).expect("fixture text");
    let mut out = Vec::new();
    for data in sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
    {
        let event: Value = hofund_plugin_sdk::json::from_str(data).expect("event");
        out.extend_from_slice(json!({"event": event}).to_string().as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(br#"{"status":"stream_ended"}"#);
    out.push(b'\n');
    out
}

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("zed", &dir);
    let models = json!({"models": [{"id": cell.model, "provider": provider(cell.shape)}]});
    let access = format!("matrix-access-{}", cell.label());
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("user_id", "matrix-user")
            .with_secret("access_token", &secret(&access))
            .with_text("system_id", "system")
            .with_text("org", "org")
            .with_text("login", "matrix-user")
            .with_text("models_json", &models.to_string()),
    )
    .await;
    host.http()
        .enqueue(br#"{"token":"matrix-model-token"}"#.to_vec());
    host.http().enqueue_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-zed-server-supports-status-messages", "true"),
        ],
        ndjson(cell.shape, cell.stream, cell.tools),
    );
    let endpoints = Endpoints {
        cloud: "https://zed.test".to_owned(),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
