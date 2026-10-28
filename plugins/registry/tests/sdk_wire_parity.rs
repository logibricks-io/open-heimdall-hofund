//! Differential tests: the SDK Chat/Responses transport against the adapter.
//!
//! Each case runs the same request through `openai_compat` (SDK `wire` on the
//! host client) and `openai_compat_adapter` (the adapter's own transport), each
//! against its own mock with the same reply. The received requests and the
//! outputs must be equal, except the kind of a reset after the request was
//! written (see the last test).
//!
//! The host HTTP client is process-wide, as in the daemon, so every test runs
//! on one shared runtime: a pooled connection opened on a runtime that a
//! `#[tokio::test]` already dropped would fail the next send.

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures_util::StreamExt;
use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{Completion, PluginError, PluginId, ProtocolId, Request, UpstreamProvider};
use hofund_plugin_registry::{openai_compat, openai_compat_adapter};
use hofund_provider::{Endpoint, OpenAiProtocol, Wire, credential_for};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "parity-key";
const BILLING: &str = "x-anthropic-billing-header: cc_version=2.1.197; cc_entrypoint=cli; cch=abc;";

const CHAT_UNARY: &str = r#"{"id":"c1","object":"chat.completion","model":"demo","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
const CHAT_STREAM: &str = concat!(
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);
const RESPONSES_UNARY: &str = r#"{"id":"r1","object":"response","model":"demo","status":"completed","output":[{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi","annotations":[]}]}],"usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}"#;
const RESPONSES_STREAM: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"object\":\"response\",\"model\":\"demo\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"object\":\"response\",\"model\":\"demo\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n",
);

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
});

fn run(future: impl Future<Output = ()>) {
    RUNTIME.block_on(future);
}

#[derive(Clone, Copy, Debug)]
enum Side {
    Sdk,
    Adapter,
}

/// What a request looked like on the wire.
#[derive(Debug, PartialEq)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// What `execute` produced.
#[derive(Debug, PartialEq)]
enum Outcome {
    Raw(Vec<u8>),
    Events(Vec<String>),
    Unary(String),
    Failed(Failure),
}

#[derive(Debug, PartialEq)]
struct Failure {
    kind: String,
    status: Option<u16>,
    retry_after: Option<Duration>,
    message: String,
}

impl From<PluginError> for Failure {
    fn from(error: PluginError) -> Self {
        Self {
            kind: format!("{:?}", error.kind()),
            status: error.status(),
            retry_after: error.retry_after(),
            message: error.message().to_owned(),
        }
    }
}

fn protocol_id(protocol: OpenAiProtocol) -> ProtocolId {
    match protocol {
        OpenAiProtocol::Chat => ProtocolId::OPENAI_CHAT,
        OpenAiProtocol::Responses => ProtocolId::OPENAI_RESPONSES,
    }
}

fn wire_of(protocol: OpenAiProtocol) -> Wire {
    match protocol {
        OpenAiProtocol::Chat => Wire::Chat,
        OpenAiProtocol::Responses => Wire::Responses,
    }
}

fn request(stream: bool) -> Request {
    serde_json::from_value(serde_json::json!({
        "model": "demo",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": format!("{BILLING}\nBe brief.")}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ],
        "params": {"max_tokens": 16},
        "stream": stream,
        "extensions": {"metadata": {"trace": "t1"}},
    }))
    .expect("request")
}

fn provider(
    side: Side,
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
    let mut endpoint = Endpoint::new("azure-parity", base);
    endpoint.preset = Some("azure".to_owned());
    endpoint.api_key = KEY.to_owned();
    endpoint.credential = credential_for("azure", wire_of(protocol), base);
    endpoint.headers = vec![("x-tenant".to_owned(), "blue".to_owned())];
    let built = match side {
        Side::Sdk => openai_compat(protocol, endpoint, &host),
        Side::Adapter => openai_compat_adapter(protocol, endpoint, &host),
    };
    match built {
        Ok(provider) => provider,
        Err(error) => panic!("build {side:?}: {error}"),
    }
}

async fn outcome(result: Result<Completion, PluginError>) -> Outcome {
    match result {
        Err(error) => Outcome::Failed(error.into()),
        Ok(Completion::Raw(mut body)) => {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                match chunk {
                    Ok(chunk) => bytes.extend_from_slice(&chunk),
                    Err(error) => return Outcome::Failed(error.into()),
                }
            }
            Outcome::Raw(bytes)
        }
        Ok(Completion::Stream(mut events)) => {
            let mut shown = Vec::new();
            while let Some(event) = events.next().await {
                match event {
                    Ok(event) => shown.push(format!("{event:?}")),
                    Err(error) => shown.push(format!("{:?}", Failure::from(error))),
                }
            }
            Outcome::Events(shown)
        }
        Ok(Completion::Unary(response)) => Outcome::Unary(format!("{response:?}")),
        Ok(_) => panic!("unexpected completion shape"),
    }
}

fn seen(request: &wiremock::Request) -> Seen {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            (name.as_str().to_owned(), value)
        })
        .filter(|(name, _)| name != "host")
        .collect();
    headers.sort();
    Seen {
        method: request.method.to_string(),
        path: request.url.path().to_owned(),
        query: request.url.query().map(str::to_owned),
        headers,
        body: request.body.clone(),
    }
}

async fn run_side(
    side: Side,
    protocol: OpenAiProtocol,
    inbound: ProtocolId,
    request: Request,
    reply: ResponseTemplate,
) -> (Vec<Seen>, Outcome) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().expect("root");
    let base = format!("{}/openai/v1", server.uri());
    let provider = provider(side, protocol, &base, root.path());
    let result = outcome(provider.execute(request, inbound).await).await;
    let received = server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .map(seen)
        .collect();
    (received, result)
}

async fn compare(
    protocol: OpenAiProtocol,
    inbound: ProtocolId,
    request: impl Fn() -> Request,
    reply: impl Fn() -> ResponseTemplate,
) -> (usize, Outcome) {
    let (sdk_seen, sdk) = run_side(Side::Sdk, protocol, inbound, request(), reply()).await;
    let (adapter_seen, adapter) =
        run_side(Side::Adapter, protocol, inbound, request(), reply()).await;
    assert_eq!(
        sdk_seen, adapter_seen,
        "requests differ for {protocol:?} from {inbound:?}"
    );
    assert_eq!(
        sdk, adapter,
        "outcomes differ for {protocol:?} from {inbound:?}"
    );
    (sdk_seen.len(), sdk)
}

fn success(stream: bool, protocol: OpenAiProtocol) -> ResponseTemplate {
    let (body, kind) = match (protocol, stream) {
        (OpenAiProtocol::Chat, false) => (CHAT_UNARY, "application/json"),
        (OpenAiProtocol::Chat, true) => (CHAT_STREAM, "text/event-stream"),
        (OpenAiProtocol::Responses, false) => (RESPONSES_UNARY, "application/json"),
        (OpenAiProtocol::Responses, true) => (RESPONSES_STREAM, "text/event-stream"),
    };
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), kind)
}

#[test]
fn successful_calls_match_for_every_protocol_inbound_and_shape() {
    run(async {
        for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
            for inbound in [protocol_id(protocol), ProtocolId::ANTHROPIC_MESSAGES] {
                for stream in [false, true] {
                    let (sent, result) = compare(
                        protocol,
                        inbound,
                        || request(stream),
                        || success(stream, protocol),
                    )
                    .await;
                    assert_eq!(sent, 1);
                    let same = inbound == protocol_id(protocol);
                    match (&result, same, stream) {
                        (Outcome::Raw(bytes), true, _) => assert!(!bytes.is_empty()),
                        (Outcome::Events(events), false, true) => {
                            assert!(!events.is_empty());
                            assert!(
                                events.iter().all(|event| !event.starts_with("Failure")),
                                "{events:?}"
                            );
                        }
                        (Outcome::Unary(_), false, false) => {}
                        other => panic!("{protocol:?} from {inbound:?} stream={stream}: {other:?}"),
                    }
                }
            }
        }
    });
}

#[test]
fn an_encode_error_matches_and_sends_nothing() {
    run(async {
        for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
            let (sent, result) = compare(
                protocol,
                ProtocolId::ANTHROPIC_MESSAGES,
                || {
                    let mut request = request(false);
                    request.params.temperature = Some(f64::NAN);
                    request
                },
                || success(false, protocol),
            )
            .await;
            let Outcome::Failed(failure) = result else {
                panic!("expected an encode error: {result:?}");
            };
            assert_eq!(failure.status, Some(400), "{failure:?}");
            assert_eq!(sent, 0, "an encode error must not reach the upstream");
        }
    });
}

#[test]
fn status_errors_match_with_retry_after() {
    run(async {
        for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
            let (_, limited) = compare(
                protocol,
                protocol_id(protocol),
                || request(true),
                || {
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "7")
                        .set_body_string(r#"{"error":{"message":"slow down"}}"#)
                },
            )
            .await;
            let Outcome::Failed(failure) = limited else {
                panic!("expected 429: {limited:?}");
            };
            assert_eq!(failure.status, Some(429));
            assert_eq!(failure.retry_after, Some(Duration::from_secs(7)));

            let (_, broken) = compare(
                protocol,
                ProtocolId::ANTHROPIC_MESSAGES,
                || request(false),
                || ResponseTemplate::new(500).set_body_string("  upstream exploded \n"),
            )
            .await;
            let Outcome::Failed(failure) = broken else {
                panic!("expected 500: {broken:?}");
            };
            assert_eq!(failure.status, Some(500));

            let (_, quota) = compare(
                protocol,
                protocol_id(protocol),
                || request(false),
                || {
                    ResponseTemplate::new(429)
                        .set_body_string(r#"{"error":{"message":"monthly quota exhausted"}}"#)
                },
            )
            .await;
            let Outcome::Failed(failure) = quota else {
                panic!("expected quota: {quota:?}");
            };
            assert_eq!(failure.kind, "Quota");

            let (_, empty) = compare(
                protocol,
                protocol_id(protocol),
                || request(false),
                || ResponseTemplate::new(503),
            )
            .await;
            assert!(matches!(empty, Outcome::Failed(_)), "{empty:?}");

            let long = "x".repeat(700);
            let (_, cut) = compare(
                protocol,
                ProtocolId::ANTHROPIC_MESSAGES,
                || request(false),
                move || ResponseTemplate::new(502).set_body_string(long.clone()),
            )
            .await;
            assert!(matches!(cut, Outcome::Failed(_)), "{cut:?}");
        }
    });
}

#[test]
fn an_in_band_error_as_the_first_event_matches() {
    run(async {
        let body = "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n";
        for inbound in [ProtocolId::OPENAI_CHAT, ProtocolId::ANTHROPIC_MESSAGES] {
            let (sent, result) = compare(
                OpenAiProtocol::Chat,
                inbound,
                || request(true),
                || {
                    ResponseTemplate::new(200)
                        .set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
                },
            )
            .await;
            assert_eq!(sent, 1);
            assert!(
                format!("{result:?}").contains("overloaded"),
                "{inbound:?}: {result:?}"
            );
        }
    });
}

/// Accepts connections, reads one full request on each, and closes it
/// without answering. Returns the port and the connection count.
fn resetting_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut data = Vec::new();
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if request_complete(&data) {
                    break;
                }
            }
            let _ = stream.flush();
            drop(stream);
        }
    });
    (port, accepted)
}

fn request_complete(data: &[u8]) -> bool {
    let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    data.len() >= end + 4 + length
}

#[test]
fn a_post_reset_after_its_body_was_written_is_not_replayed() {
    run(async {
        let mut failures = Vec::new();
        for side in [Side::Sdk, Side::Adapter] {
            let (port, accepted) = resetting_listener();
            let root = tempfile::tempdir().expect("root");
            let base = format!("http://127.0.0.1:{port}/openai/v1");
            let provider = provider(side, OpenAiProtocol::Chat, &base, root.path());
            let result = outcome(
                provider
                    .execute(request(false), ProtocolId::OPENAI_CHAT)
                    .await,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                accepted.load(Ordering::SeqCst),
                1,
                "{side:?} replayed the POST"
            );
            let Outcome::Failed(failure) = result else {
                panic!("{side:?}: {result:?}");
            };
            failures.push(failure);
        }
        // The one intended difference. The adapter's `map_transport` labels any
        // error without a status `Connect`, so the router would resend the POST
        // to the next member. The SDK keeps the host's `Transport`, which the
        // router does not fail over (follow-up filed for the adapter label).
        assert_eq!(failures[0].kind, "Transport", "{failures:?}");
        assert_eq!(failures[1].kind, "Connect", "{failures:?}");
        assert_eq!(failures[0].status, None);
        assert_eq!(failures[1].status, None);
    });
}
