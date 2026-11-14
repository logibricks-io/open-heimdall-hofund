use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_subscription_workbuddy::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, host, secret, seed, subscription, wire_response};
use crate::matrix::{Cell, Shape};

pub async fn run(cell: Cell) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("dir");
    let host = host("workbuddy", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_text("uid", "matrix-user")
            .with_secret("access_token", &secret("matrix-access"))
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_number("expires_at", 4_000_000_000_000_u64)
            .with_number("refresh_expires_at", 4_000_000_000_000_u64)
            .with_text("domain", "www.codebuddy.cn")
            .with_text("token_type", "Bearer")
            .with_text("label", "matrix-user")
            .with_text("site", "workbuddy"),
    )
    .await;
    host.http()
        .enqueue(br#"{"code":0,"data":{"models":[]}}"#.to_vec());
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
        cn: "https://workbuddy-cn.test".to_owned(),
        ai: "https://workbuddy-ai.test".to_owned(),
    };
    let subscription = subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
