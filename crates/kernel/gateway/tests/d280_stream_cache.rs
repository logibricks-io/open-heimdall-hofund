//! D-280: a Chat or Generative Language stream that names its prompt more than
//! once, and the cache on only some chunks, is told once on the Responses
//! path and in the ledger.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode, header};
use bytes::Bytes;
use futures_util::stream;
use hofund_codec::{CHAT_PROTOCOL_ID, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_gateway::Gateway;
use hofund_ir::{Request, StreamEvent};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_secrets::KeyStore;
use hofund_usage::Ledger;
use serde_json::{Value, json};
use tower::ServiceExt;

const PROMPT: u64 = 2006;
const CACHE: u64 = 1920;
const OUTPUT: u64 = 300;

struct Upstream {
    member_id: MemberId,
    protocol: &'static str,
    chunks: Vec<String>,
    framed: bool,
}

#[async_trait]
impl Member for Upstream {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "only"
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    async fn execute(&self, request: Request, inbound: &str) -> Result<Completion, PluginError> {
        let bytes = frame(&self.chunks, self.framed);
        if inbound == self.protocol {
            return Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from(bytes))));
        }
        let codec: Arc<dyn Codec> = if self.protocol == CHAT_PROTOCOL_ID {
            Arc::new(OpenAiChat)
        } else {
            Arc::new(GenerativeLanguage)
        };
        let events = codec
            .decode_stream(&bytes)
            .map_err(|error| PluginError::decode(error.to_string()))?;
        if request.stream {
            let events = events.into_iter().map(Ok::<StreamEvent, PluginError>);
            Ok(Completion::Stream(Box::pin(stream::iter(events))))
        } else {
            Ok(Completion::Unary(Box::new(hofund_ir::assemble(&events))))
        }
    }
}

fn frame(chunks: &[String], sse: bool) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        if sse {
            body.push_str("data: ");
        }
        body.push_str(chunk);
        body.push_str("\n\n");
    }
    if sse {
        body.push_str("data: [DONE]\n\n");
    }
    body.into_bytes()
}

fn chat_chunks(kind: &str) -> Vec<String> {
    let text = r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#;
    let stop = r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    match kind {
        "one usage chunk" => vec![
            text.to_owned(),
            format!(
                r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[],"usage":{{"prompt_tokens":{PROMPT},"completion_tokens":{OUTPUT},"total_tokens":2306,"prompt_tokens_details":{{"cached_tokens":{CACHE}}}}}}}"#
            ),
            stop.to_owned(),
        ],
        "cache on the first count only" => vec![
            format!(
                r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{{"index":0,"delta":{{"role":"assistant","content":"hi"}}}}],"usage":{{"prompt_tokens":{PROMPT},"completion_tokens":0,"total_tokens":{PROMPT},"prompt_tokens_details":{{"cached_tokens":{CACHE}}}}}}}"#
            ),
            format!(
                r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{PROMPT},"completion_tokens":{OUTPUT},"total_tokens":2306}}}}"#
            ),
        ],
        "prompt first, cache last" => vec![
            format!(
                r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{{"index":0,"delta":{{"role":"assistant","content":"hi"}}}}],"usage":{{"prompt_tokens":{PROMPT},"completion_tokens":0,"total_tokens":{PROMPT}}}}}"#
            ),
            format!(
                r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":0,"completion_tokens":{OUTPUT},"total_tokens":{OUTPUT},"prompt_tokens_details":{{"cached_tokens":{CACHE}}}}}}}"#
            ),
        ],
        other => panic!("unknown chat shape {other}"),
    }
}

fn generative_chunks(kind: &str) -> Vec<String> {
    match kind {
        "cache on the first count only" => vec![
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Read"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1,"cachedContentTokenCount":40}}"#.to_owned(),
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ing."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}}"#.to_owned(),
        ],
        "prompt first, cache last" => vec![
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Read"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1}}"#.to_owned(),
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ing."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":10,"cachedContentTokenCount":40}}"#.to_owned(),
        ],
        other => panic!("unknown generative shape {other}"),
    }
}

struct Harness {
    app: axum::Router,
    usage: Arc<Ledger>,
    key_id: String,
    secret: String,
    _dir: tempfile::TempDir,
}

fn harness(protocol: &'static str, chunks: Vec<String>) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("keys");
    let added = store.add("Meter", Some("d280-meter-secret")).expect("key");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let member = Arc::new(Upstream {
        member_id: MemberId::from_preset(
            PresetId::try_from("only").expect("preset"),
            MemberKey::from_static("row"),
        ),
        protocol,
        chunks,
        framed: true,
    });
    let router = Router::new(
        BTreeMap::from([("demo".to_owned(), vec![member as Arc<dyn Member>])]),
        FailoverPolicy::default(),
    );
    let codecs = HashMap::from([
        (
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        ),
        (
            "/v1/responses".to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn Codec>,
        ),
        (
            hofund_gateway::GENERATIVE_STREAM.to_owned(),
            Arc::new(GenerativeLanguage) as Arc<dyn Codec>,
        ),
    ]);
    let app = Gateway::new(
        Snapshot::new(router),
        store.token_file().read().expect("token"),
        codecs,
        Arc::clone(&usage),
    )
    .with_key_store(Arc::clone(&store))
    .into_router();
    Harness {
        app,
        usage,
        key_id: added.key.id,
        secret: added.secret,
        _dir: dir,
    }
}

async fn post(app: axum::Router, secret: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn completed_usage(body: &str, streamed: bool) -> Value {
    if !streamed {
        let value: Value = serde_json::from_str(body).expect("json");
        return value["usage"].clone();
    }
    for block in body.split("\n\n") {
        let Some(data) = block.lines().find_map(|line| line.strip_prefix("data:")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if value["type"] == "response.completed" {
            return value["response"]["usage"].clone();
        }
    }
    panic!("no completed usage in {body}");
}

fn assert_ledger(harness: &Harness, prompt: i64, cache: i64, output: i64) {
    let row = harness
        .usage
        .rows()
        .expect("rows")
        .into_iter()
        .next()
        .expect("row");
    assert_eq!(row.input_tokens, Some(prompt), "protocol total");
    assert_eq!(row.output_tokens, Some(output));
    assert_eq!(row.cache_read_tokens, Some(cache));
    let budget = harness.usage.key_usage(&harness.key_id, 0).expect("budget");
    assert_eq!(
        budget.input_tokens,
        u64::try_from(prompt.saturating_sub(cache)).unwrap_or(0),
        "uncached input is the prompt minus the cache read, once"
    );
    assert_eq!(budget.cache_read_tokens, u64::try_from(cache).unwrap_or(0));
    assert_eq!(budget.output_tokens, u64::try_from(output).unwrap_or(0));
}

#[tokio::test]
async fn d280_responses_client_of_chat_counts_the_cache_once() {
    for kind in [
        "one usage chunk",
        "cache on the first count only",
        "prompt first, cache last",
    ] {
        for stream in [true, false] {
            let harness = harness(CHAT_PROTOCOL_ID, chat_chunks(kind));
            let body = format!(r#"{{"model":"demo","input":"hi","stream":{stream}}}"#);
            let (status, reply) =
                post(harness.app.clone(), &harness.secret, "/v1/responses", &body).await;
            assert_eq!(status, StatusCode::OK, "{kind} stream={stream}: {reply}");
            let usage = completed_usage(&reply, stream);
            assert_eq!(usage["input_tokens"], json!(PROMPT), "{kind} {usage}");
            assert_eq!(usage["output_tokens"], json!(OUTPUT), "{kind} {usage}");
            assert_eq!(usage["total_tokens"], json!(2306), "{kind} {usage}");
            assert_eq!(
                usage["input_tokens_details"]["cached_tokens"],
                json!(CACHE),
                "{kind} {usage}"
            );
            assert_ledger(
                &harness,
                i64::try_from(PROMPT).unwrap(),
                i64::try_from(CACHE).unwrap(),
                i64::try_from(OUTPUT).unwrap(),
            );
        }
    }
}

#[tokio::test]
async fn d280_responses_client_of_generative_counts_the_cache_once() {
    for kind in ["cache on the first count only", "prompt first, cache last"] {
        let harness = harness("generative-language", generative_chunks(kind));
        let (status, reply) = post(
            harness.app.clone(),
            &harness.secret,
            "/v1/responses",
            r#"{"model":"demo","input":"hi","stream":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{kind}: {reply}");
        let usage = completed_usage(&reply, true);
        assert_eq!(usage["input_tokens"], json!(100), "{kind} {usage}");
        assert_eq!(usage["output_tokens"], json!(10), "{kind} {usage}");
        assert_eq!(
            usage["input_tokens_details"]["cached_tokens"],
            json!(40),
            "{kind} {usage}"
        );
        assert_ne!(usage["input_tokens"], json!(140), "{kind}");
        assert_ledger(&harness, 100, 40, 10);
    }
}

#[tokio::test]
async fn d280_chat_passthrough_records_the_cache_once() {
    let harness = harness(CHAT_PROTOCOL_ID, chat_chunks("prompt first, cache last"));
    let (status, reply) = post(
        harness.app.clone(),
        &harness.secret,
        "/v1/chat/completions",
        r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply.contains("prompt_tokens"), "{reply}");
    assert_ledger(
        &harness,
        i64::try_from(PROMPT).unwrap(),
        i64::try_from(CACHE).unwrap(),
        i64::try_from(OUTPUT).unwrap(),
    );
}
