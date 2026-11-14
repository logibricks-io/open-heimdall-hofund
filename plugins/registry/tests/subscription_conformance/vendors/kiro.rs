use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_kiro::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription};
use crate::matrix::Cell;

fn frame(event_type: &str, payload: Value) -> Vec<u8> {
    let payload = hofund_plugin_sdk::json::to_vec(&payload).expect("payload");
    let headers = [
        b"\x0b:event-type\x07".as_slice(),
        &(event_type.len() as u16).to_be_bytes(),
        event_type.as_bytes(),
    ]
    .concat();
    let total = 12 + headers.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame
}

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("kiro", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_secret("access_token", &secret("matrix-access"))
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_text("region", "us-east-1")
            .with_text("profile_arn", "arn:aws:codewhisperer:us-east-1:1:profile/P")
            .with_text("label", "matrix@example.test")
            .with_text("method", "social")
            .with_text("provider", "")
            .with_text("client_id", "")
            .with_secret("client_secret", &secret(""))
            .with_text("client_id_hash", "")
            .with_text("token_url", "")
            .with_number("expires_at", 4_000_000_000_000_u64),
    )
    .await;
    let body = if cell.tools {
        frame(
            "toolUseEvent",
            json!({"toolUseId": "call-1", "name": "lookup", "input": {"query": "fixture"}}),
        )
    } else {
        frame("assistantResponseEvent", json!({"content": "matrix-ok"}))
    };
    host.http().enqueue_response(
        200,
        &[("content-type", "application/vnd.amazon.eventstream")],
        body,
    );
    let endpoints = Endpoints {
        runtime: "https://kiro-runtime.test".to_owned(),
        ..Endpoints::default()
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
