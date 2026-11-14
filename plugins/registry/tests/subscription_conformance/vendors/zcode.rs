use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_zcode::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::{Cell, Shape};

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("zcode", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("site", "zai")
            .with_text("base", "https://api.z.ai/api/anthropic")
            .with_text("device", "00000000-0000-4000-8000-000000000001")
            .with_text("label", "matrix@example.test")
            .with_text("uid", "matrix-user")
            .with_secret("api_key", &secret("matrix-plan-key")),
    )
    .await;
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
        wire_response(Shape::Messages, cell.stream, cell.tools),
    );
    let endpoints = Endpoints {
        portal: "https://zcode.test".to_owned(),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
