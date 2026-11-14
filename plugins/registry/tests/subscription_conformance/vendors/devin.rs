use hofund_plugin_api::RawTable;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_sdk::protobuf::{FLAG_END, ProtoBuf, frame};
use hofund_plugin_subscription_devin::{
    DevinFactory, clear_catalog_cache_for_tests, model_configs_for_tests,
};

use crate::harness::{execute_async, host, secret, seed, subscription};
use crate::matrix::Cell;

fn text_response() -> Vec<u8> {
    let message = ProtoBuf::new().string(3, "matrix-ok").varint(5, 3).finish();
    let mut body = frame(0, &message).expect("text frame");
    body.extend(frame(FLAG_END, b"{}").expect("end frame"));
    body
}

fn tool_response() -> Vec<u8> {
    let start = ProtoBuf::new()
        .string(1, "call:1")
        .string(2, "lookup")
        .finish();
    let arguments = ProtoBuf::new()
        .string(3, "{\"query\":\"fixture\"}")
        .finish();
    let usage = ProtoBuf::new().varint(2, 1).varint(3, 1).finish();
    [
        frame(0, &ProtoBuf::new().bytes(6, &start).finish()).expect("tool start"),
        frame(
            0,
            &ProtoBuf::new()
                .bytes(6, &arguments)
                .bytes(7, &usage)
                .varint(5, 10)
                .finish(),
        )
        .expect("tool arguments"),
        frame(FLAG_END, b"{}").expect("end"),
    ]
    .concat()
}

pub async fn run(cell: Cell) -> Result<(), String> {
    clear_catalog_cache_for_tests();
    let dir = tempfile::tempdir().expect("dir");
    let host = host("devin", &dir);
    let account = seed(
        &host,
        SecretObject::new()
            .with_number("v", 1)
            .with_secret("api_key", &secret("matrix-api-key"))
            .with_text("api_server", "https://server.test")
            .with_text("devin_api", "https://api.devin.test")
            .with_text("label", "matrix@example.test"),
    )
    .await;
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        model_configs_for_tests(),
    );
    host.http().enqueue(if cell.tools {
        tool_response()
    } else {
        text_response()
    });
    let subscription = subscription(&DevinFactory, &host, RawTable::new());
    execute_async(&subscription, &account, &host, cell).await
}
