//! The shared test deadlines themselves: a stall fails with a named step,
//! and the test client never reuses an idle connection.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn within_returns_the_output_of_a_future_that_finishes() {
    let value = support::within("quick step", Duration::from_secs(5), async { 7 }).await;
    assert_eq!(value, 7);
}

#[tokio::test]
async fn within_names_the_step_that_stalled_once_the_limit_passes() {
    let stalled = tokio::spawn(support::within(
        "stalled step",
        Duration::from_millis(20),
        std::future::pending::<()>(),
    ));
    let joined = tokio::time::timeout(Duration::from_secs(5), stalled)
        .await
        .expect("within gave up long after its limit");
    let panic = joined.expect_err("a stalled step fails").into_panic();
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .unwrap_or_default();
    assert_eq!(message, "stalled step did not finish within 20ms");
}

#[tokio::test]
async fn the_client_opens_a_new_connection_for_every_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = [0_u8; 1024];
                // Answer every request on this connection and keep it open.
                while let Ok(read) = socket.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    let reply =
                        "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: keep-alive\r\n\r\nok";
                    if socket.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let client = support::client();
    for _ in 0..2 {
        let response = support::within(
            "local request",
            Duration::from_secs(10),
            client.get(format!("http://{address}/")).send(),
        )
        .await
        .expect("request");
        assert_eq!(response.text().await.expect("body"), "ok");
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}
