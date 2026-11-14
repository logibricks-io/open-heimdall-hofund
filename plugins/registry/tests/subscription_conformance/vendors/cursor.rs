use hofund_kit_subscription::{Http1Response, Http1Server, Http2Response, Http2Server, TestHost};
use hofund_plugin_api::{AccountId, PluginId, PluginKind, RawTable};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_sdk::protobuf::{ProtoBuf, frame};
use hofund_plugin_subscription_cursor::{Endpoints, factory_with_endpoints};

use crate::harness::{execute_async, secret, seed_as};
use crate::matrix::Cell;

fn interaction(field: u32, inner: ProtoBuf) -> Vec<u8> {
    let payload = ProtoBuf::new()
        .message(1, ProtoBuf::new().message(field, inner))
        .finish();
    frame(0, &payload).expect("interaction")
}

fn text_frame() -> Vec<u8> {
    interaction(1, ProtoBuf::new().string(1, "matrix-ok"))
}

fn turn_end_frame() -> Vec<u8> {
    interaction(
        14,
        ProtoBuf::new()
            .varint(1, 1)
            .varint(2, 1)
            .varint(3, 0)
            .varint(4, 0)
            .varint(5, 0),
    )
}

fn exec_frame(id: u32, exec_id: &str, field: u32, body: ProtoBuf) -> Vec<u8> {
    let payload = ProtoBuf::new()
        .message(
            2,
            ProtoBuf::new()
                .varint(1, u64::from(id))
                .string(15, exec_id)
                .bytes(field, &body.finish()),
        )
        .finish();
    frame(0, &payload).expect("exec")
}

fn tool_frames() -> Vec<u8> {
    let list = exec_frame(7, "list-tools", 36, ProtoBuf::new());
    let arguments = ProtoBuf::new()
        .string(1, "tools-lookup")
        .bytes(
            2,
            &ProtoBuf::new()
                .string(1, "query")
                .bytes(2, &ProtoBuf::new().string(3, "fixture").finish())
                .finish(),
        )
        .string(3, "call-1")
        .string(5, "lookup");
    let call = exec_frame(8, "call-tool", 11, arguments);
    let count = interaction(27, ProtoBuf::new().varint(1, 1));
    let checkpoint = {
        let state = ProtoBuf::new().string(4, "call-1");
        let payload = ProtoBuf::new().bytes(3, &state.finish()).finish();
        frame(0, &payload).expect("checkpoint")
    };
    [list, call, count, checkpoint, turn_end_frame()].concat()
}

fn pooled_host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_with_pooled_http_and_tokio_monotonic(
        PluginKind::Subscription,
        PluginId::from_static("cursor"),
        dir.path(),
    )
}

pub async fn execute(cell: Cell) -> Result<(), String> {
    let body = if cell.tools {
        tool_frames()
    } else {
        [text_frame(), turn_end_frame()].concat()
    };
    let h2 = Http2Server::start(vec![Http2Response::new(200).data(body)]).await;
    let h1 = Http1Server::start(vec![Http1Response::new(200).body(
        format!(r#"{{"agentUrlConfig":{{"agentUrl":"{}"}}}}"#, h2.base_url()).into_bytes(),
    )])
    .await;
    let dir = tempfile::tempdir().expect("dir");
    let host = pooled_host(&dir);
    let account = AccountId::from_static("a-0000000000000001");
    seed_as(
        &host,
        account.clone(),
        SecretObject::new()
            .with_secret("access_token", &secret("matrix-access"))
            .with_secret("refresh_token", &secret("matrix-refresh"))
            .with_number("expires_ms", 4_000_000_000_000_u64)
            .with_text("label", "matrix@example.test"),
    )
    .await;
    let endpoints = Endpoints {
        website: "https://cursor.test".to_owned(),
        api: h1.base_url().to_owned(),
        agent: h2.base_url().to_owned(),
        client_version: "cli-matrix".to_owned(),
    };
    let subscription =
        crate::harness::subscription(&factory_with_endpoints(endpoints), &host, RawTable::new());
    let result = execute_async(&subscription, &account, &host, cell).await;
    h1.shutdown().await;
    h2.shutdown().await;
    result
}
