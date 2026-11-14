use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_cline::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::{Cell, Shape};

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("cline", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("auth_type", "oauth")
            .with_secret("access_token", &secret("matrix-access"))
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_number("expires_ms", 4_000_000_000_000_u64)
            .with_text("email", "matrix@example.test"),
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
        wire_response(Shape::Chat, cell.stream, cell.tools),
    );
    let endpoints = Endpoints {
        api: "https://cline.test/api/v1".to_owned(),
        workos_device: "https://workos.test/authorize/device".to_owned(),
        workos_token: "https://workos.test/authenticate".to_owned(),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
