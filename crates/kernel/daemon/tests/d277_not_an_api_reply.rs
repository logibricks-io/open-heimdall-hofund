//! D-277 through the real daemon, registry, SDK transport, and gateway.

mod support;

use hofund_daemon::{Running, start_for_test};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PAGE: &[u8] = b"<!doctype html><title>Relay</title>";

fn write_config(dir: &std::path::Path, base: &str) -> std::path::PathBuf {
    let config = dir.join("hofund.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\n[catalog]\nrefresh = \"off\"\n[[providers]]\nid = \"relay\"\nprotocol = \"openai-chat\"\nbase_url = \"{base}\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"demo\"\npatience = \"off\"\nupstreams = [\"relay\"]\n"
        ),
    )
    .expect("config");
    config
}

async fn ask(running: &Running, dir: &std::path::Path) -> (u16, String) {
    let token = std::fs::read_to_string(dir.join("hofund.token")).expect("token");
    let response = support::client()
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({
            "model": "demo",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .expect("response");
    let status = response.status().as_u16();
    let body = response.text().await.expect("body");
    (status, body)
}

#[tokio::test]
async fn d277_gateway_learns_v1_for_this_process_and_after_restart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html; charset=utf-8"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c",
            "object": "chat.completion",
            "model": "demo",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "from-v1"}, "finish_reason": "stop"}]
        })))
        .expect(3)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let config = write_config(dir.path(), &server.uri());

    let running = start_for_test(&config).await.expect("start");
    for _ in 0..2 {
        let (status, body) = ask(&running, dir.path()).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("from-v1"), "{body}");
    }
    support::shutdown(running).await;

    let running = start_for_test(&config).await.expect("restart");
    let (status, body) = ask(&running, dir.path()).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from-v1"), "{body}");
    support::shutdown(running).await;
    server.verify().await;
}

#[tokio::test]
async fn d277_gateway_turns_invalid_success_bodies_into_502() {
    for (body, content_type, expected) in [
        (PAGE, "text/html", "web page (text/html), not an API reply"),
        (b"".as_slice(), "application/json", "nothing in it"),
        (
            b"this is not JSON".as_slice(),
            "application/json",
            "isn't JSON",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let config = write_config(dir.path(), &format!("{}/v1", server.uri()));
        let running = start_for_test(&config).await.expect("start");
        let (status, reply) = ask(&running, dir.path()).await;
        assert_eq!(status, 502, "{reply}");
        assert!(reply.contains(expected), "{reply}");
        if content_type == "text/html" {
            assert!(reply.contains("Relay"), "{reply}");
        }
        support::shutdown(running).await;
        server.verify().await;
    }
}
