use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_copilot::CopilotFactory;

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::Cell;

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("copilot", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_secret("oauth_token", &secret("matrix-oauth"))
            .with_text("host", "github.com")
            .with_text("login", "matrix-user"),
    )
    .await;
    host.http().enqueue(
        json!({"token": "matrix-session", "expires_at": 3600, "endpoints": {"api": "https://api.individual.githubcopilot.com"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"data": [
            {"id": "claude-sonnet", "name": "Claude Sonnet", "model_picker_enabled": true,
             "capabilities": {"type": "chat"},
             "supported_endpoints": ["/v1/messages", "/chat/completions"]},
            {"id": "gpt-5", "name": "GPT-5", "model_picker_enabled": true,
             "capabilities": {"type": "chat"},
             "supported_endpoints": ["/responses", "/chat/completions"]},
            {"id": "gpt-4.1", "model_picker_enabled": true, "capabilities": {"type": "chat"}}
        ]})
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue_response(
        200,
        &[(
            "content-type",
            if cell.stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )],
        wire_response(cell.shape, cell.stream, cell.tools),
    );
    let subscription = subscription(&CopilotFactory, &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
