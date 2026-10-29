//! Differential tests: the SDK Generative Language transport against the
//! adapter.
//!
//! Each case runs the same call through `generative` (SDK `wire` on the host
//! client) and `generative_adapter` (the adapter's own transport), each against
//! its own mock with the same reply. The received requests and the outputs
//! must be equal. `count_input` is compared the same way. The last test shows
//! that both calls take the SDK path: only the kind of a reset differs.
//!
//! The host HTTP client is process-wide, as in the daemon, so every test runs
//! on one shared runtime (see `sdk_wire_parity.rs`).

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures_util::StreamExt;
use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{
    Completion, CountOutcome, PluginError, PluginId, ProtocolId, Request, UpstreamProvider,
};
use hofund_plugin_registry::{generative, generative_adapter};
use hofund_provider::{Credential, Endpoint};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builds one canned upstream reply.
type Reply = Box<dyn Fn() -> ResponseTemplate>;
type Build = fn() -> Request;

const KEY: &str = "parity-key";
const MODEL: &str = "models/demo-1.5:tag/x";
const MODEL_PATH: &str = "/gateway/models/demo-1.5:tag%2Fx";

const GENERATIVE_UNARY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4},"modelVersion":"demo"}"#;
const GENERATIVE_STREAM: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"h\"}]},\"index\":0}],\"modelVersion\":\"demo\"}\r\n\r\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1,\"totalTokenCount\":4},\"modelVersion\":\"demo\"}\r\n\r\n",
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

/// How the endpoint names its credential.
#[derive(Clone, Copy, Debug)]
enum Auth {
    /// `Credential::Default`, the adapter's `x-goog-api-key`.
    Default,
    /// An explicit bearer token instead.
    Bearer,
    /// No key at all.
    Keyless,
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

#[derive(Debug, PartialEq)]
enum Outcome {
    Raw(Vec<u8>),
    Events(Vec<String>),
    Unary(String),
    Counted(u64),
    Unsupported,
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

fn request(stream: bool) -> Request {
    request_for(MODEL, stream)
}

fn request_for(model: &str, stream: bool) -> Request {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Be brief."}]},
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
    auth: Auth,
    headers: &[(&str, &str)],
    base: &str,
    root: &std::path::Path,
) -> Arc<dyn UpstreamProvider> {
    let host = runtime_host(
        PluginId::from_static("generative"),
        KEY,
        root,
        HttpLimits::default(),
    )
    .expect("host");
    let mut endpoint = Endpoint::new("generative-parity", base);
    match auth {
        Auth::Default => {
            endpoint.api_key = KEY.to_owned();
            endpoint.credential = Credential::Default;
        }
        Auth::Bearer => {
            endpoint.api_key = KEY.to_owned();
            endpoint.credential = Credential::Bearer;
        }
        Auth::Keyless => endpoint.credential = Credential::Default,
    }
    endpoint.headers = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let built = match side {
        Side::Sdk => generative(endpoint, &host),
        Side::Adapter => generative_adapter(endpoint, &host),
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

fn counted(result: Result<CountOutcome, PluginError>) -> Outcome {
    match result {
        Ok(CountOutcome::Counted(tokens)) => Outcome::Counted(tokens),
        Ok(CountOutcome::Unsupported) => Outcome::Unsupported,
        Ok(other) => panic!("unexpected count outcome: {other:?}"),
        Err(error) => Outcome::Failed(error.into()),
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

/// One call to compare: execute, or count.
#[derive(Clone, Copy, Debug)]
enum Call {
    Execute,
    Count,
}

#[derive(Clone, Copy, Debug)]
struct Case<'a> {
    call: Call,
    auth: Auth,
    headers: &'a [(&'a str, &'a str)],
    inbound: ProtocolId,
}

impl Case<'_> {
    fn execute(inbound: ProtocolId) -> Self {
        Case {
            call: Call::Execute,
            auth: Auth::Default,
            headers: &[],
            inbound,
        }
    }

    fn count(inbound: ProtocolId) -> Self {
        Case {
            call: Call::Count,
            ..Case::execute(inbound)
        }
    }
}

async fn run_side(
    side: Side,
    case: Case<'_>,
    request: Request,
    reply: ResponseTemplate,
) -> (Vec<Seen>, Outcome) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().expect("root");
    let base = format!("{}/gateway", server.uri());
    let provider = provider(side, case.auth, case.headers, &base, root.path());
    let result = match case.call {
        Call::Execute => outcome(provider.execute(request, case.inbound).await).await,
        Call::Count => counted(provider.count_input(request, case.inbound).await),
    };
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
    case: Case<'_>,
    request: impl Fn() -> Request,
    reply: impl Fn() -> ResponseTemplate,
) -> (Vec<Seen>, Outcome) {
    let (sdk_seen, sdk) = run_side(Side::Sdk, case, request(), reply()).await;
    let (adapter_seen, adapter) = run_side(Side::Adapter, case, request(), reply()).await;
    assert_eq!(sdk_seen, adapter_seen, "requests differ for {case:?}");
    assert_eq!(sdk, adapter, "outcomes differ for {case:?}");
    (sdk_seen, sdk)
}

fn success(stream: bool) -> ResponseTemplate {
    if stream {
        ResponseTemplate::new(200)
            .set_body_raw(GENERATIVE_STREAM.as_bytes().to_vec(), "text/event-stream")
    } else {
        ResponseTemplate::new(200)
            .set_body_raw(GENERATIVE_UNARY.as_bytes().to_vec(), "application/json")
    }
}

fn header<'a>(seen: &'a Seen, name: &str) -> Vec<&'a str> {
    seen.headers
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

fn body_of(seen: &Seen) -> serde_json::Value {
    serde_json::from_slice(&seen.body).expect("json body")
}

#[test]
fn successful_calls_match_for_every_inbound_and_shape() {
    run(async {
        for inbound in [ProtocolId::GENERATIVE_LANGUAGE, ProtocolId::OPENAI_CHAT] {
            for stream in [false, true] {
                let (sent, result) = compare(
                    Case::execute(inbound),
                    || request(stream),
                    || success(stream),
                )
                .await;
                assert_eq!(sent.len(), 1);
                if stream {
                    assert_eq!(sent[0].path, format!("{MODEL_PATH}:streamGenerateContent"));
                    assert_eq!(sent[0].query.as_deref(), Some("alt=sse"));
                } else {
                    assert_eq!(sent[0].path, format!("{MODEL_PATH}:generateContent"));
                    assert_eq!(sent[0].query, None);
                }
                assert_eq!(header(&sent[0], "x-goog-api-key"), [KEY]);
                assert!(header(&sent[0], "authorization").is_empty());
                let body = body_of(&sent[0]);
                assert!(body.get("model").is_none(), "{body}");
                assert!(body.get("stream").is_none(), "{body}");
                let same = inbound == ProtocolId::GENERATIVE_LANGUAGE;
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
                    other => panic!("from {inbound:?} stream={stream}: {other:?}"),
                }
            }
        }
    });
}

#[test]
fn credentials_and_configured_headers_match() {
    run(async {
        let plain: &[(&str, &str)] = &[("x-goog-user-project", "p1")];
        let replaced: &[(&str, &str)] = &[("x-goog-api-key", "configured")];
        for headers in [plain, replaced] {
            for auth in [Auth::Default, Auth::Bearer, Auth::Keyless] {
                for call in [Call::Execute, Call::Count] {
                    let case = Case {
                        call,
                        auth,
                        headers,
                        inbound: ProtocolId::GENERATIVE_LANGUAGE,
                    };
                    let reply = || match call {
                        Call::Execute => success(false),
                        Call::Count => {
                            ResponseTemplate::new(200).set_body_string(r#"{"totalTokens":9}"#)
                        }
                    };
                    let (sent, _) = compare(case, || request(false), reply).await;
                    assert_eq!(sent.len(), 1);
                    if headers == plain {
                        assert_eq!(header(&sent[0], "x-goog-user-project"), ["p1"]);
                        let bearer = format!("Bearer {KEY}");
                        match auth {
                            Auth::Default => {
                                assert_eq!(header(&sent[0], "x-goog-api-key"), [KEY]);
                                assert!(header(&sent[0], "authorization").is_empty());
                            }
                            Auth::Bearer => {
                                assert!(header(&sent[0], "x-goog-api-key").is_empty());
                                assert_eq!(header(&sent[0], "authorization"), [bearer.as_str()]);
                            }
                            Auth::Keyless => {
                                assert!(header(&sent[0], "x-goog-api-key").is_empty());
                                assert!(header(&sent[0], "authorization").is_empty());
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn an_encode_error_or_empty_model_matches_and_sends_nothing() {
    run(async {
        for call in [Call::Execute, Call::Count] {
            let case = Case {
                call,
                ..Case::execute(ProtocolId::OPENAI_CHAT)
            };
            let bad: [(&str, Build); 3] = [
                ("nan", || {
                    let mut request = request(false);
                    request.params.temperature = Some(f64::NAN);
                    request
                }),
                ("empty", || request_for("", false)),
                ("prefix only", || {
                    // The model check runs before encoding on both sides.
                    let mut request = request_for(" models/ ", true);
                    request.params.temperature = Some(f64::NAN);
                    request
                }),
            ];
            for (name, build) in bad {
                let (sent, result) = compare(case, build, || success(false)).await;
                let Outcome::Failed(failure) = result else {
                    panic!("{name}: expected an error: {result:?}");
                };
                assert_eq!(failure.status, Some(400), "{name}: {failure:?}");
                if name != "nan" {
                    assert_eq!(failure.message, "model is empty", "{name}");
                }
                assert!(sent.is_empty(), "{name}: must not reach the upstream");
            }
        }
    });
}

#[test]
fn status_errors_match_with_retry_after() {
    run(async {
        let (_, limited) = compare(
            Case::execute(ProtocolId::GENERATIVE_LANGUAGE),
            || request(true),
            || {
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_string(r#"{"error":{"code":429,"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#)
            },
        )
        .await;
        let Outcome::Failed(failure) = limited else {
            panic!("expected 429: {limited:?}");
        };
        assert_eq!(failure.status, Some(429));
        assert_eq!(failure.retry_after, Some(Duration::from_secs(7)));

        let (_, quota) = compare(
            Case::execute(ProtocolId::OPENAI_CHAT),
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

        let (_, broken) = compare(
            Case::execute(ProtocolId::OPENAI_CHAT),
            || request(false),
            || ResponseTemplate::new(503).set_body_string("  unavailable \n"),
        )
        .await;
        let Outcome::Failed(failure) = broken else {
            panic!("expected 503: {broken:?}");
        };
        assert_eq!(failure.status, Some(503));
    });
}

#[test]
fn an_in_band_error_as_the_first_event_matches() {
    run(async {
        let body = "data: {\"error\":{\"code\":503,\"message\":\"overloaded\",\"status\":\"UNAVAILABLE\"}}\r\n\r\n";
        for inbound in [ProtocolId::GENERATIVE_LANGUAGE, ProtocolId::OPENAI_CHAT] {
            let (sent, result) = compare(
                Case::execute(inbound),
                || request(true),
                || {
                    ResponseTemplate::new(200)
                        .set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
                },
            )
            .await;
            assert_eq!(sent.len(), 1);
            assert!(
                format!("{result:?}").contains("overloaded"),
                "{inbound:?}: {result:?}"
            );
        }
    });
}

#[test]
fn counts_match_for_every_reply() {
    run(async {
        let replies: Vec<(&str, Reply)> = vec![
            (
                "counted",
                Box::new(|| ResponseTemplate::new(200).set_body_string(r#"{"totalTokens":42}"#)),
            ),
            (
                "messages field",
                Box::new(|| ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":42}"#)),
            ),
            (
                "404",
                Box::new(|| ResponseTemplate::new(404).set_body_string("missing")),
            ),
            ("405", Box::new(|| ResponseTemplate::new(405))),
            ("501", Box::new(|| ResponseTemplate::new(501))),
            (
                "400 endpoint",
                Box::new(|| {
                    ResponseTemplate::new(400)
                        .set_body_string(r#"{"error":{"message":"countTokens is not supported"}}"#)
                }),
            ),
            (
                "400 request",
                Box::new(|| {
                    ResponseTemplate::new(400)
                        .set_body_string(r#"{"error":{"message":"contents: field required"}}"#)
                }),
            ),
            (
                "429",
                Box::new(|| {
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "3")
                        .set_body_string(r#"{"error":{"message":"slow down"}}"#)
                }),
            ),
            (
                "quota",
                Box::new(|| {
                    ResponseTemplate::new(429)
                        .set_body_string(r#"{"error":{"message":"monthly quota exhausted"}}"#)
                }),
            ),
            (
                "not json",
                Box::new(|| ResponseTemplate::new(200).set_body_string("ok")),
            ),
        ];
        for (name, reply) in &replies {
            for inbound in [ProtocolId::GENERATIVE_LANGUAGE, ProtocolId::OPENAI_CHAT] {
                let (sent, result) = compare(Case::count(inbound), || request(true), reply).await;
                assert_eq!(sent.len(), 1, "{name}");
                assert_eq!(sent[0].path, format!("{MODEL_PATH}:countTokens"), "{name}");
                assert_eq!(sent[0].query, None, "{name}");
                let body = body_of(&sent[0]);
                assert!(body.get("model").is_none(), "{name}: {body}");
                assert!(body.get("stream").is_none(), "{name}: {body}");
                let expected = match *name {
                    "counted" => Some(Outcome::Counted(42)),
                    "404" | "405" | "501" | "400 endpoint" => Some(Outcome::Unsupported),
                    _ => None,
                };
                match expected {
                    Some(expected) => assert_eq!(result, expected, "{name}"),
                    None => assert!(matches!(result, Outcome::Failed(_)), "{name}: {result:?}"),
                }
                if *name == "429" {
                    let Outcome::Failed(failure) = &result else {
                        unreachable!()
                    };
                    assert_eq!(failure.retry_after, Some(Duration::from_secs(3)));
                }
                if *name == "quota" {
                    let Outcome::Failed(failure) = &result else {
                        unreachable!()
                    };
                    assert_eq!(failure.kind, "Quota");
                }
            }
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
fn execute_and_count_take_the_sdk_path_and_do_not_replay_a_reset_post() {
    run(async {
        for call in [Call::Execute, Call::Count] {
            let mut failures = Vec::new();
            for side in [Side::Sdk, Side::Adapter] {
                let (port, accepted) = resetting_listener();
                let root = tempfile::tempdir().expect("root");
                let base = format!("http://127.0.0.1:{port}/gateway");
                let provider = provider(side, Auth::Default, &[], &base, root.path());
                let inbound = ProtocolId::GENERATIVE_LANGUAGE;
                let result = match call {
                    Call::Execute => outcome(provider.execute(request(false), inbound).await).await,
                    Call::Count => counted(provider.count_input(request(false), inbound).await),
                };
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(
                    accepted.load(Ordering::SeqCst),
                    1,
                    "{side:?} {call:?} replayed the POST"
                );
                let Outcome::Failed(failure) = result else {
                    panic!("{side:?} {call:?}: {result:?}");
                };
                failures.push(failure);
            }
            // The intended difference, as for Chat: the SDK keeps the host's
            // `Transport`; the adapter labels the reset `Connect`.
            assert_eq!(failures[0].kind, "Transport", "{call:?} {failures:?}");
            assert_eq!(failures[1].kind, "Connect", "{call:?} {failures:?}");
        }
    });
}
