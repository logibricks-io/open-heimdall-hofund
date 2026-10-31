//! An Azure chat stream through the real registry and the SDK transport.
//!
//! The upstream is a local socket that holds the rest of the body until the
//! client has seen the first chunk, so a buffered body would time out here.

mod support;

use std::time::Duration;

use hofund_daemon::start_for_test;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const FIRST: &str = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"early\"},\"finish_reason\":null}]}\n\n";
const REST: &str = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn chunk(data: &str) -> Vec<u8> {
    let mut framed = format!("{:x}\r\n", data.len()).into_bytes();
    framed.extend_from_slice(data.as_bytes());
    framed.extend_from_slice(b"\r\n");
    framed
}

/// Serves one streamed reply and returns the request head it read.
async fn serve_once(listener: TcpListener, release: oneshot::Receiver<()>) -> String {
    let (mut socket, _) = listener.accept().await.expect("accept");
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head = loop {
        let read = socket.read(&mut buffer).await.expect("read");
        assert!(read > 0, "the request ended early");
        data.extend_from_slice(&buffer[..read]);
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
        }
    };
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        )
        .await
        .expect("head");
    socket.write_all(&chunk(FIRST)).await.expect("first");
    socket.flush().await.expect("flush");
    let _ = release.await;
    socket.write_all(&chunk(REST)).await.expect("rest");
    socket.write_all(b"0\r\n\r\n").await.expect("end");
    socket.flush().await.expect("flush");
    head
}

#[tokio::test]
async fn an_azure_chat_stream_reaches_the_client_before_the_upstream_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (release, released) = oneshot::channel();
    let upstream = tokio::spawn(serve_once(listener, released));

    let dir = tempfile::tempdir().expect("directory");
    let config = dir.path().join("hofund.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"azure\"\npreset = \"azure\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:{port}/openai/v1\"\napi_key = \"azure-test-key\"\n[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"azure\"]\n"
        ),
    )
    .expect("config");
    let running = start_for_test(&config).await.expect("start");
    let token = std::fs::read_to_string(dir.path().join("hofund.token")).expect("token");

    let mut response = support::client()
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({
            "model": "demo",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("the first chunk was held back")
        .expect("chunk")
        .expect("a chunk");
    assert!(String::from_utf8_lossy(&first).contains("early"));
    release.send(()).expect("release");
    let mut rest = Vec::new();
    while let Some(more) = response.chunk().await.expect("chunk") {
        rest.extend_from_slice(&more);
    }
    assert!(String::from_utf8_lossy(&rest).contains("[DONE]"));

    let head = upstream.await.expect("upstream");
    assert!(
        head.starts_with("post /openai/v1/chat/completions "),
        "{head}"
    );
    assert!(
        head.contains("\r\napi-key: azure-test-key"),
        "missing api-key"
    );
    assert!(
        !head.contains("\r\nauthorization:"),
        "a bearer header leaked"
    );
    support::shutdown(running).await;
}
