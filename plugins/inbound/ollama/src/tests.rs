//! The surface against a stand-in gateway.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use hofund_plugin_api::{Body, BodyStream, InboundSurface, Method, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, HttpResponse, async_trait, text_response};

use super::{ERROR_CAP, Kernel, Ollama, read_capped, resolve, serve};
use crate::reply::{Writer, failure_message};

/// Runs `future` on this thread. Everything here is ready or woken at once.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

/// A body in `chunks`, then `tail` if one is given.
fn chunked(chunks: Vec<&str>, tail: Option<PluginError>) -> BodyStream {
    let mut items: Vec<Result<Vec<u8>, PluginError>> = chunks
        .into_iter()
        .map(|chunk| Ok(chunk.as_bytes().to_vec()))
        .collect();
    items.extend(tail.map(Err));
    items.reverse();
    BodyStream::unfold(items, |mut items| async move {
        let item = items.pop()?;
        Some((item, items))
    })
}

/// A reply the stand-in gateway gives.
struct Canned {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    chunks: Vec<&'static str>,
    tail: Option<PluginError>,
}

impl Canned {
    fn ok(content_type: &'static str, chunks: Vec<&'static str>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", content_type)],
            chunks,
            tail: None,
        }
    }
}

/// A call the stand-in gateway saw: method, path, body, and content type.
type Seen = (String, String, String, Option<String>);

/// The stand-in gateway: lists `ids` and answers a chat with `chat`.
struct Gateway {
    ids: Vec<&'static str>,
    models_status: u16,
    chat: Mutex<Option<Canned>>,
    fail_chat: bool,
    seen: Mutex<Vec<Seen>>,
}

impl Gateway {
    fn new(ids: Vec<&'static str>) -> Self {
        Self {
            ids,
            models_status: 200,
            chat: Mutex::new(None),
            fail_chat: false,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn with_chat(self, canned: Canned) -> Self {
        *self.chat.lock().expect("chat") = Some(canned);
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }

    /// The Chat body the surface sent.
    fn chat_body(&self) -> Value {
        let seen = self.seen();
        let (_, _, body, _) = seen
            .iter()
            .find(|(_, path, _, _)| path == "/v1/chat/completions")
            .expect("a chat call");
        json::from_str(body).expect("chat json")
    }
}

#[async_trait]
impl Kernel for Gateway {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        let path = request.uri().path().to_owned();
        let body = String::from_utf8(request.body().bytes().to_vec()).expect("utf-8");
        let content_type = request
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.seen.lock().expect("seen").push((
            request.method().as_str().to_owned(),
            path.clone(),
            body,
            content_type,
        ));
        if path == "/v1/models" {
            let data: Vec<Value> = self.ids.iter().map(|id| json!({"id": id})).collect();
            return Ok(text_response(
                self.models_status,
                json!({"object": "list", "data": data}).to_string(),
            ));
        }
        if self.fail_chat {
            return Err(PluginError::unsupported("this host has no gateway"));
        }
        let canned = self.chat.lock().expect("chat").take().expect("one chat");
        let mut response = text_response(canned.status, "");
        for (name, value) in canned.headers {
            response
                .headers_mut()
                .append(name, value.parse().expect("header"));
        }
        *response.body_mut() = chunked(canned.chunks, canned.tail);
        Ok(response)
    }
}

fn post(path: &str, body: &str) -> HttpRequest {
    let mut request = HttpRequest::new(Body::from_bytes(body.as_bytes().to_vec()));
    *request.method_mut() = "POST".parse().expect("method");
    *request.uri_mut() = path.parse().expect("path");
    request
}

fn get(path: &str) -> HttpRequest {
    let mut request = HttpRequest::new(Body::empty());
    *request.uri_mut() = path.parse().expect("path");
    request
}

/// What `request` on the route at `prefix` answers: status, content type,
/// and body.
fn ask(gateway: &Gateway, prefix: &str, request: &HttpRequest) -> (u16, String, String) {
    block_on(async {
        let response = serve(gateway, prefix, request).await;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("ascii").to_owned())
            .unwrap_or_default();
        let mut body = response.into_body();
        let mut text = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            text.extend_from_slice(&chunk.expect("chunk"));
        }
        (
            status,
            content_type,
            String::from_utf8(text).expect("utf-8"),
        )
    })
}

fn lines(body: &str) -> Vec<Value> {
    assert!(body.ends_with('\n'), "{body:?}");
    body.lines()
        .map(|line| json::from_str(line).expect("a JSON line"))
        .collect()
}

fn one(body: &str) -> Value {
    let mut all = lines(body);
    assert_eq!(all.len(), 1, "{body}");
    all.remove(0)
}

fn assert_created_at(line: &Value) {
    let created = line["created_at"].as_str().expect("created_at");
    assert_eq!(created.len(), 30, "{created}");
    assert!(
        created.ends_with('Z') && created.as_bytes()[10] == b'T',
        "{created}"
    );
}

#[test]
fn the_routes_are_ollamas() {
    let surface = Ollama;
    assert_eq!(surface.id().as_str(), "ollama");
    let routes: Vec<(Method, &str)> = surface
        .routes()
        .iter()
        .map(|route| (route.method(), route.prefix()))
        .collect();
    assert_eq!(
        routes,
        [
            (Method::Get, "/api/version"),
            (Method::Get, "/api/tags"),
            (Method::Post, "/api/show"),
            (Method::Post, "/api/chat"),
            (Method::Post, "/api/generate"),
        ]
    );
    let _ = super::contribute();
}

#[test]
fn version_answers_past_every_minimum() {
    let gateway = Gateway::new(vec![]);
    let (status, kind, body) = ask(&gateway, "/api/version", &get("/api/version"));
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    assert_eq!(one(&body), json!({"version": "0.12.6"}));
    assert!(gateway.seen().is_empty());
}

#[test]
fn a_deeper_path_is_not_found() {
    let gateway = Gateway::new(vec!["seer"]);
    for (prefix, path) in [
        ("/api/version", "/api/version/x"),
        ("/api/tags", "/api/tags/seer"),
        ("/api/chat", "/api/chat/"),
    ] {
        let (status, _, body) = ask(&gateway, prefix, &get(path));
        assert_eq!((status, body.as_str()), (404, "404 page not found"));
    }
    let (status, _, _) = ask(&gateway, "/hofund/other", &get("/hofund/other"));
    assert_eq!(status, 404);
    assert!(gateway.seen().is_empty());
}

#[test]
fn tags_list_the_gateways_models() {
    let gateway = Gateway::new(vec!["seer", "smith/forge"]);
    let (status, kind, body) = ask(&gateway, "/api/tags", &get("/api/tags"));
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    let details = json!({"parent_model": "", "format": "", "family": "hofund",
        "families": ["hofund"], "parameter_size": "", "quantization_level": ""});
    assert_eq!(
        one(&body),
        json!({"models": [
            {"name": "seer", "model": "seer", "modified_at": "2025-01-01T00:00:00Z", "size": 0,
             "digest": "bc1227ae633ceb146d19d0e646ff90058cc3a2cc8679c458eb8c3ff3c3f6a818",
             "details": details, "capabilities": ["completion", "tools"]},
            {"name": "smith/forge", "model": "smith/forge", "modified_at": "2025-01-01T00:00:00Z",
             "size": 0, "digest": "aabe14ad8ac2f859ed9472ab6dff23ab267d56a552fdbf456ba81e03c57e9e00", "details": details,
             "capabilities": ["completion", "tools"]},
        ]})
    );
    assert_eq!(
        gateway.seen(),
        [(
            "GET".to_owned(),
            "/v1/models".to_owned(),
            String::new(),
            Some("application/json".to_owned())
        )]
    );
}

#[test]
fn a_failing_model_list_is_an_ollama_error() {
    let mut gateway = Gateway::new(vec![]);
    gateway.models_status = 401;
    let (status, kind, body) = ask(&gateway, "/api/tags", &get("/api/tags"));
    assert_eq!((status, kind.as_str()), (401, "application/json"));
    assert_eq!(
        one(&body),
        json!({"error": "{\"object\":\"list\",\"data\":[]}"})
    );
}

#[test]
fn show_tells_one_model() {
    let gateway = Gateway::new(vec!["seer", "plain:latest"]);
    let expected = |id: &str| {
        json!({
            "modelfile": "", "parameters": "", "template": "",
            "details": {"parent_model": "", "format": "", "family": "hofund",
                "families": ["hofund"], "parameter_size": "", "quantization_level": ""},
            "model_info": {"general.architecture": "hofund", "general.basename": id,
                "general.parameter_count": 0, "hofund.context_length": 32768,
                "hofund.embedding_length": 0},
            "capabilities": ["completion", "tools"], "modified_at": "2025-01-01T00:00:00Z",
        })
    };
    for (body, id) in [
        (r#"{"model":"seer"}"#, "seer"),
        (r#"{"model":" seer:latest "}"#, "seer"),
        (r#"{"name":"seer"}"#, "seer"),
        (r#"{"model":"","name":"seer"}"#, "seer"),
        (r#"{"model":"plain:latest"}"#, "plain:latest"),
    ] {
        let (status, kind, reply) = ask(&gateway, "/api/show", &post("/api/show", body));
        assert_eq!((status, kind.as_str()), (200, "application/json"), "{body}");
        assert_eq!(one(&reply), expected(id), "{body}");
    }
    for (body, status, error) in [
        (r#"{"model":"ghost"}"#, 404, "model 'ghost' not found"),
        (
            r#"{"model":"ghost:latest"}"#,
            404,
            "model 'ghost:latest' not found",
        ),
        (r#"{"model":"  "}"#, 400, "model is required"),
        ("{}", 400, "model is required"),
        ("[1]", 400, "invalid request: the body is not a JSON object"),
    ] {
        let (got, _, reply) = ask(&gateway, "/api/show", &post("/api/show", body));
        assert_eq!(got, status, "{body}");
        assert_eq!(one(&reply), json!({"error": error}), "{body}");
    }
    let (status, _, reply) = ask(&gateway, "/api/show", &post("/api/show", "{"));
    assert_eq!(status, 400);
    assert!(
        one(&reply)["error"]
            .as_str()
            .expect("error")
            .starts_with("invalid request: EOF")
    );
}

#[test]
fn an_empty_chat_loads_the_model() {
    let gateway = Gateway::new(vec!["seer"]);
    let (status, kind, body) = ask(
        &gateway,
        "/api/chat",
        &post("/api/chat", r#"{"model":"seer:latest","messages":[]}"#),
    );
    assert_eq!((status, kind.as_str()), (200, "application/x-ndjson"));
    let line = one(&body);
    assert_created_at(&line);
    assert_eq!(line["model"], "seer");
    assert_eq!(line["done"], true);
    assert_eq!(line["done_reason"], "load");
    assert_eq!(line["message"], json!({"role": "assistant", "content": ""}));
    assert!(line.get("response").is_none());

    let (status, kind, body) = ask(
        &gateway,
        "/api/generate",
        &post("/api/generate", r#"{"model":"seer","stream":false}"#),
    );
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    let line = one(&body);
    assert_eq!(line["response"], "");
    assert_eq!(line["done_reason"], "load");
    assert!(line.get("message").is_none());

    let (status, _, body) = ask(
        &gateway,
        "/api/generate",
        &post("/api/generate", r#"{"model":"ghost"}"#),
    );
    assert_eq!(status, 404);
    assert_eq!(one(&body), json!({"error": "model 'ghost' not found"}));
    assert!(
        gateway
            .seen()
            .iter()
            .all(|(_, path, _, _)| path == "/v1/models")
    );
}

#[test]
fn bad_chats_are_refused_before_the_gateway() {
    let gateway = Gateway::new(vec!["seer"]);
    for (path, body, error) in [
        (
            "/api/chat",
            r#"{"messages":[{"role":"user"}]}"#,
            "model is required",
        ),
        (
            "/api/chat",
            "7",
            "invalid request: the body is not a JSON object",
        ),
        (
            "/api/chat",
            r#"{"model":"seer","messages":[{"role":"user","content":"hi"}],"format":1}"#,
            "invalid format: 1",
        ),
        (
            "/api/generate",
            r#"{"model":"seer","prompt":"hi","think":[]}"#,
            "invalid think value: []",
        ),
    ] {
        let (status, _, reply) = ask(&gateway, path, &post(path, body));
        assert_eq!(status, 400, "{body}");
        assert_eq!(one(&reply), json!({"error": error}), "{body}");
    }
    assert!(
        gateway
            .seen()
            .iter()
            .all(|(_, path, _, _)| path == "/v1/models")
    );
}

const STREAM: [&str; 6] = [
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"hm\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"con",
    "tent\":\"lo\"}}]}\r\n\r\n: comment\n\nevent: x\ndata: not json\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":1}\"}},{\"index\":2,\"function\":{\"name\":\"note\",\"arguments\":\"oops\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n",
];

#[test]
fn a_chat_streams_back_as_ndjson() {
    let mut canned = Canned::ok("text/event-stream; charset=utf-8", STREAM.to_vec());
    canned.headers.push(("x-hofund-member", "one"));
    canned.headers.push(("retry-after", "3"));
    canned.headers.push(("cache-control", "no-cache"));
    let gateway = Gateway::new(vec!["seer"]).with_chat(canned);
    let request = post(
        "/api/chat",
        r#"{"model":"seer:latest","messages":[{"role":"user","content":"hi"}],"options":{"num_predict":5}}"#,
    );
    let response = block_on(serve(&gateway, "/api/chat", &request));
    assert_eq!(response.headers()["x-hofund-member"], "one");
    assert_eq!(response.headers()["retry-after"], "3");
    assert!(response.headers().get("cache-control").is_none());
    let (status, kind, body) = ask(
        &Gateway::new(vec!["seer"]).with_chat(Canned::ok("text/event-stream", STREAM.to_vec())),
        "/api/chat",
        &request,
    );
    assert_eq!((status, kind.as_str()), (200, "application/x-ndjson"));
    let all = lines(&body);
    assert_eq!(all.len(), 5, "{body}");
    for line in &all {
        assert_created_at(line);
        assert_eq!(line["model"], "seer");
    }
    assert_eq!(
        all[0]["message"],
        json!({"role": "assistant", "content": "", "thinking": "hm"})
    );
    assert_eq!(
        all[1]["message"],
        json!({"role": "assistant", "content": "Hel"})
    );
    assert_eq!(
        all[2]["message"],
        json!({"role": "assistant", "content": "lo"})
    );
    assert_eq!(all[0]["done"], false);
    assert_eq!(all[3]["done"], false);
    assert_eq!(
        all[3]["message"],
        json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "call_a", "function": {"index": 0, "name": "read", "arguments": {"path": 1}}},
            {"id": "call_2", "function": {"index": 2, "name": "note", "arguments": "oops"}},
        ]})
    );
    let last = &all[4];
    assert_eq!(last["done"], true);
    assert_eq!(last["done_reason"], "stop");
    assert_eq!(last["message"], json!({"role": "assistant", "content": ""}));
    assert_eq!(last["prompt_eval_count"], 11);
    assert_eq!(last["eval_count"], 7);
    assert_eq!(last["load_duration"], 0);
    for key in ["total_duration", "prompt_eval_duration", "eval_duration"] {
        assert!(last[key].is_u64(), "{key}");
    }
    let sent = gateway.seen();
    assert_eq!(sent.len(), 2);
    let (method, path, _, content_type) = &sent[1];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/v1/chat/completions")
    );
    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert_eq!(
        gateway.chat_body(),
        json!({"model": "seer", "messages": [{"role": "user", "content": "hi"}],
            "stream": true, "stream_options": {"include_usage": true}, "max_tokens": 5})
    );
}

#[test]
fn a_chat_asked_without_streaming_is_gathered() {
    let gateway =
        Gateway::new(vec!["seer"]).with_chat(Canned::ok("text/event-stream", STREAM.to_vec()));
    let (status, kind, body) = ask(
        &gateway,
        "/api/chat",
        &post(
            "/api/chat",
            r#"{"model":"seer","stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    let line = one(&body);
    assert_eq!(line["done"], true);
    assert_eq!(line["done_reason"], "stop");
    assert_eq!(
        line["message"],
        json!({"role": "assistant", "content": "Hello", "thinking": "hm", "tool_calls": [
            {"id": "call_a", "function": {"index": 0, "name": "read", "arguments": {"path": 1}}},
            {"id": "call_2", "function": {"index": 2, "name": "note", "arguments": "oops"}},
        ]})
    );
    assert_eq!(gateway.chat_body()["stream"], true);
}

#[test]
fn generate_answers_with_response_and_drops_thinking_when_off() {
    let chunks = vec![
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"why\",\"content\":\"A\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"so\"},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]",
    ];
    let gateway =
        Gateway::new(vec!["seer"]).with_chat(Canned::ok("text/event-stream", chunks.clone()));
    let (_, _, body) = ask(
        &gateway,
        "/api/generate",
        &post("/api/generate", r#"{"model":"seer","prompt":"q"}"#),
    );
    let all = lines(&body);
    assert_eq!(all.len(), 3, "{body}");
    assert_eq!(
        (&all[0]["response"], &all[0]["thinking"]),
        (&json!("A"), &json!("why"))
    );
    assert_eq!(all[1]["response"], "");
    assert_eq!(all[1]["thinking"], "so");
    assert!(all[2].get("thinking").is_none());
    assert_eq!(all[2]["done_reason"], "length");
    assert!(all.iter().all(|line| line.get("message").is_none()));

    let gateway = Gateway::new(vec!["seer"]).with_chat(Canned::ok("text/event-stream", chunks));
    let (_, _, body) = ask(
        &gateway,
        "/api/generate",
        &post(
            "/api/generate",
            r#"{"model":"seer","prompt":"q","think":false,"stream":false}"#,
        ),
    );
    let line = one(&body);
    assert_eq!(line["response"], "A");
    assert!(line.get("thinking").is_none());
    assert_eq!(line["done_reason"], "length");
}

#[test]
fn a_whole_completion_is_read_as_one() {
    let whole = vec![
        "{\"choices\":[{\"message\":{\"content\":\"Hi\",\"tool_calls\":[{\"function\":{\"name\":\"go\"}}]},",
        "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
    ];
    let gateway = Gateway::new(vec!["seer"]).with_chat(Canned::ok("application/json", whole));
    let (_, _, body) = ask(
        &gateway,
        "/api/chat",
        &post(
            "/api/chat",
            r#"{"model":"seer","messages":[{"role":"user","content":"x"}]}"#,
        ),
    );
    let all = lines(&body);
    assert_eq!(all.len(), 3, "{body}");
    assert_eq!(all[0]["message"]["content"], "Hi");
    assert_eq!(
        all[1]["message"]["tool_calls"],
        json!([{"id": "call_0", "function": {"index": 0, "name": "go", "arguments": {}}}])
    );
    assert_eq!(
        (&all[2]["prompt_eval_count"], &all[2]["eval_count"]),
        (&json!(3), &json!(2))
    );
}

#[test]
fn a_refused_chat_is_an_ollama_error_with_its_status() {
    let cases = [
        (
            429,
            "{\"error\":{\"message\":\"slow down\",\"type\":\"rate_limit_error\"}}",
            "slow down",
        ),
        (404, "{\"error\":\"no such model\"}", "no such model"),
        (503, "  upstream is down \n", "upstream is down"),
        (500, "", "Internal Server Error"),
    ];
    for (code, text, message) in cases {
        let canned = Canned {
            status: code,
            headers: vec![
                ("retry-after", "9"),
                ("x-request-id", "r1"),
                ("content-length", "5"),
            ],
            chunks: vec![text],
            tail: None,
        };
        let gateway = Gateway::new(vec!["seer"]).with_chat(canned);
        let request = post(
            "/api/chat",
            r#"{"model":"seer","messages":[{"role":"user","content":"x"}]}"#,
        );
        let response = block_on(serve(&gateway, "/api/chat", &request));
        assert_eq!(response.headers()["retry-after"], "9");
        assert_eq!(response.headers()["x-request-id"], "r1");
        assert!(response.headers().get("content-length").is_none());
        let gateway = Gateway::new(vec!["seer"]).with_chat(Canned {
            status: code,
            headers: vec![],
            chunks: vec![text],
            tail: None,
        });
        let (status, kind, body) = ask(&gateway, "/api/chat", &request);
        assert_eq!((status, kind.as_str()), (code, "application/json"));
        assert_eq!(one(&body), json!({"error": message}));
    }
}

#[test]
fn failures_after_the_start_end_the_stream_with_an_error_line() {
    let chunks = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"upstream reset\",\"type\":\"server_error\"}}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"c\"}}]}\n\n",
    ];
    let gateway = Gateway::new(vec!["seer"]).with_chat(Canned::ok("text/event-stream", chunks));
    let request = post(
        "/api/chat",
        r#"{"model":"seer","messages":[{"role":"user","content":"x"}]}"#,
    );
    let (status, _, body) = ask(&gateway, "/api/chat", &request);
    assert_eq!(status, 200);
    let all = lines(&body);
    assert_eq!(all.len(), 2, "{body}");
    assert_eq!(all[0]["message"]["content"], "a");
    assert_eq!(all[1], json!({"error": "upstream reset"}));

    let mut canned = Canned::ok(
        "text/event-stream",
        vec!["data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n"],
    );
    canned.tail = Some(PluginError::interrupted("the connection dropped"));
    let gateway = Gateway::new(vec!["seer"]).with_chat(canned);
    let (status, _, body) = ask(&gateway, "/api/chat", &request);
    assert_eq!(status, 200);
    let all = lines(&body);
    assert_eq!(all.len(), 2, "{body}");
    assert_eq!(all[1], json!({"error": "the connection dropped"}));

    let mut canned = Canned::ok("text/event-stream", vec![]);
    canned.tail = Some(PluginError::interrupted("gone"));
    let gateway = Gateway::new(vec!["seer"]).with_chat(canned);
    let (_, _, body) = ask(
        &gateway,
        "/api/chat",
        &post(
            "/api/chat",
            r#"{"model":"seer","stream":false,"messages":[{"role":"user","content":"x"}]}"#,
        ),
    );
    assert_eq!(one(&body), json!({"error": "gone"}));
}

#[test]
fn a_gateway_that_cannot_run_the_chat_is_a_bad_gateway() {
    let mut gateway = Gateway::new(vec!["seer"]);
    gateway.fail_chat = true;
    let (status, _, body) = ask(
        &gateway,
        "/api/chat",
        &post(
            "/api/chat",
            r#"{"model":"seer","messages":[{"role":"user","content":"x"}]}"#,
        ),
    );
    assert_eq!(status, 502);
    assert_eq!(one(&body), json!({"error": "this host has no gateway"}));
}

#[test]
fn durations_count_from_the_request_to_the_first_token_and_the_end() {
    let start = Instant::now();
    let at = |millis: u64| start + Duration::from_millis(millis);
    let mut writer = Writer::new("seer", false, true, false, true, start);
    assert!(
        writer
            .feed(b"data: {\"choices\":[{\"delta\":{}}]}\n", at(10))
            .is_empty()
    );
    let first = writer.feed(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
        at(40),
    );
    assert_eq!(
        one(&String::from_utf8(first).expect("utf-8"))["done"],
        false
    );
    writer.feed(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"y\"}}]}\n",
        at(70),
    );
    let last = writer.finish(at(100));
    let last = one(&String::from_utf8(last).expect("utf-8"));
    assert_eq!(last["total_duration"], 100_000_000);
    assert_eq!(last["prompt_eval_duration"], 40_000_000);
    assert_eq!(last["eval_duration"], 60_000_000);

    let mut writer = Writer::new("seer", false, true, false, true, start);
    let last = writer.finish(at(25));
    let last = one(&String::from_utf8(last).expect("utf-8"));
    assert_eq!(last["total_duration"], 25_000_000);
    assert_eq!(last["prompt_eval_duration"], 25_000_000);
    assert_eq!(last["eval_duration"], 0);
}

#[test]
fn a_writer_ends_once() {
    let start = Instant::now();
    let mut writer = Writer::new("seer", true, true, false, true, start);
    let failed = writer.fail("broke");
    assert_eq!(failed, b"{\"error\":\"broke\"}\n");
    assert!(writer.is_done());
    assert!(writer.fail("again").is_empty());
    assert!(
        writer
            .feed(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
                start
            )
            .is_empty()
    );
    assert!(writer.finish(start).is_empty());
}

#[test]
fn failure_messages_take_the_most_specific_text() {
    assert_eq!(
        failure_message(400, br#"{"error":{"message":"bad"}}"#),
        "bad"
    );
    assert_eq!(
        failure_message(400, br#"{"error":{"message":""}}"#),
        r#"{"message":""}"#
    );
    assert_eq!(failure_message(400, br#"{"error":"plain"}"#), "plain");
    assert_eq!(failure_message(400, br#"{"error":7}"#), "7");
    assert_eq!(failure_message(400, br#"{"error":""}"#), r#"{"error":""}"#);
    assert_eq!(
        failure_message(400, br#"{"detail":"x"}"#),
        r#"{"detail":"x"}"#
    );
    assert_eq!(failure_message(429, b" \n"), "Too Many Requests");
    assert_eq!(failure_message(418, b""), "request failed");
}

#[test]
fn a_name_is_its_own_id_before_its_latest_tag_is_dropped() {
    let ids = vec![
        "seer".to_owned(),
        "sage:latest".to_owned(),
        "sage".to_owned(),
    ];
    assert_eq!(resolve(&ids, "seer:latest"), "seer");
    assert_eq!(resolve(&ids, "seer"), "seer");
    assert_eq!(resolve(&ids, "sage:latest"), "sage:latest");
    assert_eq!(resolve(&ids, "sage"), "sage");
    assert_eq!(resolve(&ids, "gone:latest"), "gone:latest");
}

#[test]
fn a_failed_reply_is_read_up_to_the_cap() {
    let big = "e".repeat(ERROR_CAP + 10);
    let read = block_on(read_capped(chunked(
        vec!["ab", &big],
        Some(PluginError::internal("never read")),
    )));
    let read = read.expect("the cap stops before the broken tail");
    assert_eq!(read.len(), ERROR_CAP);
    assert!(read.starts_with(b"abee"));
    let broken = block_on(read_capped(chunked(
        vec!["ab"],
        Some(PluginError::internal("broken")),
    )));
    assert_eq!(broken.unwrap_err().message(), "broken");
}
