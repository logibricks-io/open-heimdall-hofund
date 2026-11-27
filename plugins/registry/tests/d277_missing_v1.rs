//! D-277 reply validation and missing `/v1` learning on the real SDK path.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{
    BodyStream, Completion, PluginError, PluginId, ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_registry::openai_compat;
use hofund_provider::{Endpoint, OpenAiProtocol};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "d277-test-key";
const PAGE: &[u8] = b"<!doctype html><title>Relay</title>";

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
});

fn run(future: impl Future<Output = ()>) {
    RUNTIME.block_on(future);
}

fn protocol_id(protocol: OpenAiProtocol) -> ProtocolId {
    match protocol {
        OpenAiProtocol::Chat => ProtocolId::OPENAI_CHAT,
        OpenAiProtocol::Responses => ProtocolId::OPENAI_RESPONSES,
    }
}

fn endpoint_path(protocol: OpenAiProtocol) -> &'static str {
    match protocol {
        OpenAiProtocol::Chat => "chat/completions",
        OpenAiProtocol::Responses => "responses",
    }
}

fn provider(
    protocol: OpenAiProtocol,
    base: &str,
    root: &std::path::Path,
) -> Arc<dyn UpstreamProvider> {
    let host = runtime_host(
        PluginId::from_static("openai-compat"),
        KEY,
        root,
        HttpLimits::default(),
    )
    .expect("host");
    let mut endpoint = Endpoint::new("d277", base);
    endpoint.api_key = KEY.to_owned();
    openai_compat(protocol, endpoint, &host).expect("provider")
}

fn request(model: &str, stream: bool) -> Request {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "stream": stream,
    }))
    .expect("request")
}

async fn body(completion: Completion) -> Vec<u8> {
    let Completion::Raw(mut body) = completion.peel() else {
        panic!("raw completion");
    };
    read_body(&mut body).await
}

async fn read_body(body: &mut BodyStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    bytes
}

async fn execute(
    provider: &Arc<dyn UpstreamProvider>,
    protocol: OpenAiProtocol,
    stream: bool,
) -> Result<Vec<u8>, PluginError> {
    let completion = provider
        .execute(request("demo", stream), protocol_id(protocol))
        .await?;
    Ok(body(completion).await)
}

fn answer(stream: bool) -> ResponseTemplate {
    if stream {
        ResponseTemplate::new(200).set_body_raw(
            b"data: {\"ok\":true}\n\ndata: [DONE]\n\n".as_slice(),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))
    }
}

#[test]
fn d277_calls_retry_a_page_under_v1_and_reuse_it() {
    run(async {
        for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
            for stream in [false, true] {
                let server = MockServer::start().await;
                let endpoint = endpoint_path(protocol);
                Mock::given(method("POST"))
                    .and(path(format!("/{endpoint}")))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_raw(PAGE, "text/html; charset=utf-8"),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(format!("/v1/{endpoint}")))
                    .respond_with(answer(stream))
                    .expect(2)
                    .mount(&server)
                    .await;
                let root = tempfile::tempdir().expect("root");
                let provider = provider(protocol, &server.uri(), root.path());
                let first = execute(&provider, protocol, stream).await.expect("first");
                assert!(!first.is_empty());
                assert_eq!(provider.attempt_base(), format!("{}/v1", server.uri()));
                let second = execute(&provider, protocol, stream).await.expect("second");
                assert!(!second.is_empty());
                let paths = server
                    .received_requests()
                    .await
                    .expect("requests")
                    .into_iter()
                    .map(|request| request.url.path().to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(
                    paths,
                    [
                        format!("/{endpoint}"),
                        format!("/v1/{endpoint}"),
                        format!("/v1/{endpoint}"),
                    ],
                    "{protocol:?}, stream={stream}"
                );
                server.verify().await;
            }
        }
    });
}

#[test]
fn d277_refusal_under_v1_is_returned_with_a_base_hint() {
    run(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "bad key"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().expect("root");
        let provider = provider(OpenAiProtocol::Chat, &server.uri(), root.path());
        let error = execute(&provider, OpenAiProtocol::Chat, false)
            .await
            .expect_err("refused");
        assert_eq!(error.status(), Some(401));
        assert!(error.message().contains("bad key"), "{error}");
        assert!(
            error
                .message()
                .contains(&format!("set the Base URL to {}/v1", server.uri())),
            "{error}"
        );
        assert_eq!(provider.attempt_base(), server.uri());
        server.verify().await;
    });
}

#[test]
fn d277_versioned_calls_reject_invalid_success_bodies() {
    run(async {
        for (body, content_type, expected) in [
            (
                PAGE,
                "text/html",
                "web page (text/html), not an API reply: \"Relay\"",
            ),
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
            let root = tempfile::tempdir().expect("root");
            let provider = provider(
                OpenAiProtocol::Chat,
                &format!("{}/v1", server.uri()),
                root.path(),
            );
            let error = execute(&provider, OpenAiProtocol::Chat, false)
                .await
                .expect_err("invalid reply");
            assert_eq!(error.status(), Some(502), "{error}");
            assert!(error.message().contains(expected), "{error}");
            server.verify().await;
        }
    });
}

#[test]
fn d277_page_or_404_under_v1_keeps_the_original_error() {
    run(async {
        for (name, status, body, content_type) in [
            ("page", 200, PAGE, "text/html"),
            ("missing", 404, b"missing".as_slice(), "text/plain"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html"))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(status).set_body_raw(body, content_type))
                .expect(1)
                .mount(&server)
                .await;
            let root = tempfile::tempdir().expect("root");
            let provider = provider(OpenAiProtocol::Chat, &server.uri(), root.path());
            let error = execute(&provider, OpenAiProtocol::Chat, false)
                .await
                .expect_err(name);
            assert_eq!(error.status(), Some(502), "{name}: {error}");
            assert!(
                error.message().contains("not an API reply: \"Relay\""),
                "{name}: {error}"
            );
            assert!(!error.message().contains("set the Base URL"), "{error}");
            assert_eq!(provider.attempt_base(), server.uri());
            server.verify().await;
        }
    });
}

#[test]
fn d277_json_labelled_event_stream_is_an_answer() {
    run(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"data: {\"ok\":true}\n\n".as_slice(), "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().expect("root");
        let provider = provider(
            OpenAiProtocol::Chat,
            &format!("{}/v1", server.uri()),
            root.path(),
        );
        let body = execute(&provider, OpenAiProtocol::Chat, true)
            .await
            .expect("stream");
        assert!(body.starts_with(b"data:"), "{body:?}");
        server.verify().await;
    });
}
