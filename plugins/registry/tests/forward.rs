//! Forwarded calls on the built-in OpenAI-style provider: the path under the
//! base URL, the credential and headers, the body's own content type, the
//! answer unchanged, and status errors outside `2xx`.
//!
//! The host HTTP client is process-wide, so every test runs on one shared
//! runtime, as in `sdk_wire_parity`.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{PluginErrorKind, PluginId, RawBody, UpstreamProvider};
use hofund_plugin_registry::{anthropic, openai_compat};
use hofund_provider::{Endpoint, OpenAiProtocol, Wire, credential_for};
use wiremock::matchers::{body_bytes, header, method, path};
use wiremock::{Match, Mock, MockServer, ResponseTemplate};

const KEY: &str = "forward-key";

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
});

fn run(future: impl Future<Output = ()>) {
    RUNTIME.block_on(future);
}

fn endpoint(base: &str, wire: Wire) -> Endpoint {
    let mut endpoint = Endpoint::new("forward-test", base);
    endpoint.api_key = KEY.to_owned();
    endpoint.credential = credential_for("openai", wire, base);
    endpoint.headers = vec![("x-tenant".to_owned(), "blue".to_owned())];
    endpoint
}

fn openai(
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
    let wire = match protocol {
        OpenAiProtocol::Chat => Wire::Chat,
        OpenAiProtocol::Responses => Wire::Responses,
    };
    openai_compat(protocol, endpoint(base, wire), &host).expect("provider")
}

const BODY: &str = r#"{"model":"vendor-embed","input":"hi"}"#;
const ANSWER: &str =
    r#"{"object":"list","data":[{"embedding":[0.25]}],"usage":{"prompt_tokens":2}}"#;

#[test]
fn a_forward_posts_the_body_under_the_base_with_the_credential() {
    run(async {
        for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .and(header("authorization", format!("Bearer {KEY}").as_str()))
                .and(header("x-tenant", "blue"))
                .and(header("content-type", "application/json"))
                .and(body_bytes(BODY.as_bytes()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(ANSWER.as_bytes(), "application/json; charset=utf-8"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let root = tempfile::tempdir().expect("root");
            let provider = openai(protocol, &format!("{}/v1/", server.uri()), root.path());
            let answer = provider
                .forward(
                    "/embeddings",
                    RawBody::new("application/json", BODY.as_bytes().to_vec()),
                )
                .await
                .expect("answer");
            assert_eq!(answer.status(), 200);
            assert_eq!(
                answer.content_type(),
                Some("application/json; charset=utf-8")
            );
            assert_eq!(answer.body().as_ref(), ANSWER.as_bytes());
        }
    });
}

#[test]
fn the_body_keeps_its_own_content_type() {
    run(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .and(header("content-type", "multipart/form-data; boundary=x"))
            .respond_with(ResponseTemplate::new(202).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().expect("root");
        let provider = openai(OpenAiProtocol::Chat, &server.uri(), root.path());
        let answer = provider
            .forward(
                "/rerank",
                RawBody::new("multipart/form-data; boundary=x", b"--x--".to_vec()),
            )
            .await
            .expect("answer");
        assert_eq!(answer.status(), 202);
        assert_eq!(answer.body().as_ref(), b"ok");
    });
}

#[test]
fn an_answer_outside_2xx_is_a_status_error() {
    run(async {
        for status in [404, 400, 429] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_string(r#"{"error":{"message":"refused here"}}"#),
                )
                .mount(&server)
                .await;
            let root = tempfile::tempdir().expect("root");
            let provider = openai(OpenAiProtocol::Chat, &server.uri(), root.path());
            let error = provider
                .forward("/embeddings", RawBody::new("application/json", BODY))
                .await
                .expect_err("status error");
            assert_eq!(error.status(), Some(status), "{status}");
            assert_eq!(error.kind(), PluginErrorKind::Status, "{status}");
            assert!(error.message().contains("refused here"), "{error:?}");
        }
    });
}

#[test]
fn a_messages_provider_does_not_forward_and_sends_nothing() {
    run(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().expect("root");
        let host = runtime_host(
            PluginId::from_static("anthropic"),
            KEY,
            root.path(),
            HttpLimits::default(),
        )
        .expect("host");
        let provider = anthropic(endpoint(&server.uri(), Wire::Messages), &host).expect("provider");
        let error = provider
            .forward("/embeddings", RawBody::new("application/json", BODY))
            .await
            .expect_err("unsupported");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        assert_eq!(error.message(), "this upstream does not serve /embeddings");
    });
}

struct NoJsonAccept;

impl Match for NoJsonAccept {
    fn matches(&self, request: &wiremock::Request) -> bool {
        request
            .headers
            .get("accept")
            .is_none_or(|value| value != "application/json")
    }
}

#[test]
fn p1_4_07_no_json_accept() {
    run(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/speech"))
            .and(NoJsonAccept)
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"audio".as_slice(), "audio/mpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().expect("root");
        let provider = openai(
            OpenAiProtocol::Chat,
            &format!("{}/v1", server.uri()),
            root.path(),
        );
        let answer = provider
            .forward(
                "/audio/speech",
                RawBody::new("application/json", br#"{"model":"tts"}"#.as_slice()),
            )
            .await
            .expect("answer");
        assert!(answer.is_streaming());
        let mut body = answer.into_body_stream();
        let mut received = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            received.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(received, b"audio");
    });
}

#[test]
fn p1_4_05_incremental_streaming() {
    run(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let (release, released) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let header_end = loop {
                let mut buffer = [0_u8; 1024];
                let read = socket.read(&mut buffer).await.expect("read");
                assert_ne!(read, 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(at) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let mut buffer = [0_u8; 1024];
                let read = socket.read(&mut buffer).await.expect("body");
                assert_ne!(read, 0, "request ended before its body");
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nfirst\r\n",
                )
                .await
                .expect("headers and first chunk");
            socket.flush().await.expect("flush");
            released.await.expect("release");
            socket
                .write_all(b"6\r\nsecond\r\n0\r\n\r\n")
                .await
                .expect("second chunk");
        });

        let root = tempfile::tempdir().expect("root");
        let provider = openai(
            OpenAiProtocol::Chat,
            &format!("http://{address}/v1"),
            root.path(),
        );
        let answer = provider
            .forward(
                "/audio/speech",
                RawBody::new("application/json", br#"{"model":"tts"}"#.as_slice()),
            )
            .await
            .expect("answer");
        let mut body = answer.into_body_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.next_chunk())
            .await
            .expect("first chunk before upstream completion")
            .expect("first chunk")
            .expect("first chunk bytes");
        assert_eq!(first.as_ref(), b"first");
        release.send(()).expect("release upstream");
        let mut rest = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            rest.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(rest, b"second");
        upstream.await.expect("upstream");
    });
}
