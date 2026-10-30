//! The backend half of the surface against a stand-in kernel.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use hofund_plugin_api::{Body, BodyStream, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, HttpResponse, async_trait, text_response};

use super::backend::{BASES, Bases, COMPACT_MESSAGE, LIST_WAIT, PREFIX};
use super::tests::block_on;
use super::{Kernel, dispatch};

const TEST_BASES: Bases = Bases {
    chatgpt: "https://backend.test/codex",
    api: "https://api.test/v1",
};

/// One call the stand-in saw.
#[derive(Debug)]
struct Seen {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    within: Option<Duration>,
}

impl Seen {
    fn of(request: &HttpRequest, within: Option<Duration>) -> Self {
        let mut headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().expect("ascii").to_owned(),
                )
            })
            .collect();
        headers.sort();
        Self {
            method: request.method().as_str().to_owned(),
            uri: request.uri().to_string(),
            headers,
            body: request.body().bytes().to_vec(),
            within,
        }
    }

    fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// An answer the stand-in gives: status, headers, body, or a failure.
type Answer = Result<(u16, Vec<(&'static str, &'static str)>, BodyStream), PluginError>;

fn answer(status: u16, body: impl Into<Vec<u8>>) -> Answer {
    Ok((status, Vec::new(), BodyStream::from_bytes(body.into())))
}

fn response(answer: Answer) -> Result<HttpResponse, PluginError> {
    let (status, headers, body) = answer?;
    let mut response = text_response(status, "");
    for (name, value) in headers {
        response
            .headers_mut()
            .insert(name, value.parse().expect("header"));
    }
    *response.body_mut() = body;
    Ok(response)
}

/// The stand-in kernel: answers in-process calls and outbound sends in
/// order, and keeps what it was asked.
#[derive(Default)]
struct Stub {
    calls: Mutex<VecDeque<Answer>>,
    sends: Mutex<VecDeque<Answer>>,
    called: Mutex<Vec<Seen>>,
    sent: Mutex<Vec<Seen>>,
}

impl Stub {
    fn calling(self, answer: Answer) -> Self {
        self.calls.lock().expect("calls").push_back(answer);
        self
    }

    fn sending(self, answer: Answer) -> Self {
        self.sends.lock().expect("sends").push_back(answer);
        self
    }

    fn listing(self, ids: &[&str]) -> Self {
        let data: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
        self.calling(answer(
            200,
            json!({"object": "list", "data": data}).to_string(),
        ))
    }

    fn called(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.called.lock().expect("called"))
    }

    fn sent(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.sent.lock().expect("sent"))
    }
}

#[async_trait]
impl Kernel for Stub {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        self.called
            .lock()
            .expect("called")
            .push(Seen::of(&request, None));
        let next = self.calls.lock().expect("calls").pop_front();
        response(next.expect("an in-process call was not expected"))
    }

    async fn send(
        &self,
        request: HttpRequest,
        within: Option<Duration>,
    ) -> Result<HttpResponse, PluginError> {
        self.sent
            .lock()
            .expect("sent")
            .push(Seen::of(&request, within));
        let next = self.sends.lock().expect("sends").pop_front();
        response(next.expect("a send was not expected"))
    }
}

fn request(
    method: &str,
    target: &str,
    headers: &[(&'static str, &str)],
    body: &str,
) -> HttpRequest {
    let mut request = HttpRequest::new(Body::from_bytes(body.as_bytes().to_vec()));
    *request.method_mut() = method.parse().expect("method");
    *request.uri_mut() = target.parse().expect("target");
    for (name, value) in headers {
        request
            .headers_mut()
            .append(*name, value.parse().expect("header"));
    }
    request
}

/// What the backend answers: status, content type, and body.
fn ask_with(stub: &Stub, bases: Bases, request: HttpRequest) -> (u16, String, String) {
    block_on(async {
        let response = super::backend::serve(stub, bases, request).await;
        read(response).await
    })
}

fn ask(stub: &Stub, request: HttpRequest) -> (u16, String, String) {
    ask_with(stub, TEST_BASES, request)
}

async fn read(response: HttpResponse) -> (u16, String, String) {
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
}

fn message(body: &str) -> String {
    let value: Value = json::from_str(body).expect("json");
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .expect("message")
        .to_owned()
}

fn slugs(body: &str) -> Vec<(String, u64)> {
    let value: Value = json::from_str(body).expect("json");
    value["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|model| {
            (
                model["slug"].as_str().expect("slug").to_owned(),
                model["priority"].as_u64().unwrap_or(0),
            )
        })
        .collect()
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

#[test]
fn the_real_bases_are_the_chatgpt_backend_and_the_api() {
    assert_eq!(BASES.chatgpt, "https://chatgpt.com/backend-api/codex");
    assert_eq!(BASES.api, "https://api.openai.com/v1");
    assert_eq!(PREFIX, "/backend-api/codex");
    assert_eq!(LIST_WAIT, Duration::from_secs(3));
}

#[test]
fn the_backend_route_goes_to_the_backend_and_the_catalog_route_to_the_catalog() {
    let stub = Stub::default().sending(answer(204, ""));
    let backend = request("POST", "/backend-api/codex/responses", &[], "{}");
    let (status, _, _) =
        block_on(async { read(dispatch(&stub, "/backend-api/codex", backend).await).await });
    assert_eq!(status, 204);
    assert_eq!(
        stub.sent()[0].uri,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    let stub = Stub::default().listing(&["smith/forge"]);
    let catalog = request("GET", "/v1/codex/models", &[], "");
    let (status, _, body) =
        block_on(async { read(dispatch(&stub, "/v1/codex/models", catalog).await).await });
    assert_eq!(status, 200);
    assert_eq!(slugs(&body), [("smith/forge".to_owned(), 1)]);
    assert!(stub.sent().is_empty());
}

#[test]
fn a_websocket_upgrade_is_refused_at_once() {
    let stub = Stub::default();
    let (status, _, body) = ask(
        &stub,
        request(
            "GET",
            "/backend-api/codex/responses",
            &[("connection", "Upgrade"), ("upgrade", "WebSocket")],
            "",
        ),
    );
    assert_eq!(status, 426);
    assert_eq!(body, "hofund serves Codex over HTTP; use HTTP");
    assert!(stub.sent().is_empty());
    assert!(stub.called().is_empty());
}

#[test]
fn codexs_own_model_is_relayed_as_it_came_less_the_hop_headers() {
    let stub = Stub::default().sending(Ok((
        200,
        vec![("content-type", "text/event-stream")],
        BodyStream::from_bytes(b"data: one\n\n".to_vec()),
    )));
    let body = r#"{"model":"gpt-5","stream":true}"#;
    let (status, kind, text) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses?trace=1",
            &[
                ("authorization", "Bearer chatgpt"),
                ("chatgpt-account-id", "acct"),
                ("content-type", "application/json"),
                ("host", "127.0.0.1:1"),
                ("accept-encoding", "gzip"),
                ("content-length", "31"),
                ("connection", "keep-alive, X-Drop"),
                ("x-drop", "1"),
                ("keep-alive", "timeout=5"),
                ("proxy-connection", "keep-alive"),
                ("transfer-encoding", "chunked"),
                ("te", "trailers"),
                ("trailer", "x-sum"),
                ("upgrade", "h2c"),
                ("x-keep", "yes"),
            ],
            body,
        ),
    );
    assert_eq!(
        (status, kind.as_str(), text.as_str()),
        (200, "text/event-stream", "data: one\n\n")
    );
    let sent = stub.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].uri, "https://backend.test/codex/responses?trace=1");
    assert_eq!(sent[0].body, body.as_bytes());
    assert_eq!(sent[0].within, None);
    assert_eq!(
        sent[0].headers,
        [
            pair("authorization", "Bearer chatgpt"),
            pair("chatgpt-account-id", "acct"),
            pair("content-type", "application/json"),
            pair("x-keep", "yes"),
        ]
    );
    assert!(stub.called().is_empty());
}

#[test]
fn any_other_path_is_relayed_to_the_same_backend() {
    let stub = Stub::default()
        .sending(answer(200, "usage"))
        .sending(answer(404, "none"));
    let (status, _, body) = ask(
        &stub,
        request("GET", "/backend-api/codex/wham/usage", &[], ""),
    );
    assert_eq!((status, body.as_str()), (200, "usage"));
    let (status, _, body) = ask(&stub, request("GET", "/backend-api/codex", &[], ""));
    assert_eq!((status, body.as_str()), (404, "none"));
    let sent = stub.sent();
    assert_eq!(sent[0].uri, "https://backend.test/codex/wham/usage");
    assert_eq!(sent[0].method, "GET");
    assert_eq!(sent[1].uri, "https://backend.test/codex");
}

#[test]
fn an_api_key_goes_to_the_api() {
    let stub = Stub::default().sending(answer(200, "ok"));
    let (status, _, _) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses",
            &[("authorization", "Bearer  sk-proj-1")],
            r#"{"model":"gpt-5"}"#,
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(stub.sent()[0].uri, "https://api.test/v1/responses");
}

#[test]
fn a_relayed_model_list_with_an_api_key_stays_on_the_backend() {
    let stub = Stub::default().sending(answer(200, "ok"));
    let (status, _, _) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/models",
            &[("authorization", "Bearer sk-proj-1")],
            "{}",
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(stub.sent()[0].uri, "https://backend.test/codex/models");
}

#[test]
fn a_bearer_that_is_not_an_api_key_stays_on_the_backend() {
    let stub = Stub::default()
        .sending(answer(200, "ok"))
        .sending(answer(200, "ok"));
    for authorization in ["Bearer eyJsk-", "Basic sk-1"] {
        ask(
            &stub,
            request(
                "POST",
                "/backend-api/codex/responses",
                &[("authorization", authorization)],
                "{}",
            ),
        );
    }
    let sent = stub.sent();
    assert_eq!(sent[0].uri, "https://backend.test/codex/responses");
    assert_eq!(sent[1].uri, "https://backend.test/codex/responses");
}

#[test]
fn a_relay_that_fails_says_why() {
    let stub = Stub::default()
        .sending(Err(PluginError::connect("refused")))
        .sending(Err(PluginError::status_error(429, "slow down")));
    let (status, kind, body) = ask(
        &stub,
        request("POST", "/backend-api/codex/responses", &[], "{}"),
    );
    assert_eq!((status, kind.as_str()), (502, "application/json"));
    assert_eq!(message(&body), "refused");
    let (status, _, body) = ask(
        &stub,
        request("POST", "/backend-api/codex/responses", &[], "{}"),
    );
    assert_eq!(status, 429);
    assert_eq!(message(&body), "slow down");
}

#[test]
fn a_hofund_model_is_served_in_process() {
    let stub = Stub::default().calling(Ok((
        200,
        vec![("content-type", "text/event-stream")],
        BodyStream::from_bytes(b"data: ours\n\n".to_vec()),
    )));
    let body = r#"{"model":"smith/forge","input":"hi"}"#;
    let (status, kind, text) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses?trace=1",
            &[
                ("authorization", "Bearer chatgpt"),
                ("chatgpt-account-id", "acct"),
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
                ("x-other", "1"),
            ],
            body,
        ),
    );
    assert_eq!(
        (status, kind.as_str(), text.as_str()),
        (200, "text/event-stream", "data: ours\n\n")
    );
    let called = stub.called();
    assert_eq!(called.len(), 1);
    assert_eq!(called[0].method, "POST");
    assert_eq!(called[0].uri, "/v1/responses");
    assert_eq!(called[0].body, body.as_bytes());
    assert_eq!(
        called[0].headers,
        [
            pair("accept", "text/event-stream"),
            pair("content-type", "application/json"),
        ]
    );
    assert!(stub.sent().is_empty());
}

#[test]
fn a_hofund_model_that_fails_in_process_says_why() {
    let stub = Stub::default().calling(Err(PluginError::status_error(404, "no such model")));
    let (status, _, body) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses",
            &[],
            r#"{"model":"smith/none"}"#,
        ),
    );
    assert_eq!(status, 404);
    assert_eq!(message(&body), "no such model");
}

#[test]
fn a_hofund_model_cannot_compact_and_codexs_own_can() {
    let stub = Stub::default().sending(answer(200, "compacted"));
    let (status, kind, body) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses/compact",
            &[],
            r#"{"model":"smith/forge"}"#,
        ),
    );
    assert_eq!((status, kind.as_str()), (400, "application/json"));
    assert_eq!(message(&body), COMPACT_MESSAGE);
    assert_eq!(
        COMPACT_MESSAGE,
        "/responses/compact is not supported for hofund models; use a compaction_trigger on /responses"
    );
    let value: Value = json::from_str(&body).expect("json");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert!(stub.called().is_empty());
    assert!(stub.sent().is_empty());
    let (status, _, body) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses/compact",
            &[],
            r#"{"model":"gpt-5"}"#,
        ),
    );
    assert_eq!((status, body.as_str()), (200, "compacted"));
    assert_eq!(
        stub.sent()[0].uri,
        "https://backend.test/codex/responses/compact"
    );
}

#[test]
fn only_a_post_to_responses_naming_a_slash_model_stays_in_process() {
    let stub = Stub::default();
    let cases = [
        ("POST", "/backend-api/codex/responses", "not json"),
        ("POST", "/backend-api/codex/responses", r#"{"model":7}"#),
        ("POST", "/backend-api/codex/responses", r#"{"input":"a/b"}"#),
        ("GET", "/backend-api/codex/responses", r#"{"model":"a/b"}"#),
        (
            "POST",
            "/backend-api/codex/responses/x",
            r#"{"model":"a/b"}"#,
        ),
        ("POST", "/backend-api/codex/memories", r#"{"model":"a/b"}"#),
    ];
    let stub = cases
        .iter()
        .fold(stub, |stub, _| stub.sending(answer(200, "relayed")));
    for (method, target, body) in cases {
        let (status, _, text) = ask(&stub, request(method, target, &[], body));
        assert_eq!(
            (status, text.as_str()),
            (200, "relayed"),
            "{method} {target}"
        );
    }
    assert_eq!(stub.sent().len(), cases.len());
    assert!(stub.called().is_empty());
}

fn models_request(headers: &[(&'static str, &str)]) -> HttpRequest {
    request(
        "GET",
        "/backend-api/codex/models?client_version=0.150.0",
        headers,
        "",
    )
}

#[test]
fn the_model_list_is_the_backends_then_hofunds() {
    let own = json!({"models": [
        {"slug": "gpt-5", "priority": 5},
        {"slug": "gpt-5-mini", "priority": 2},
    ]});
    let stub = Stub::default()
        .sending(answer(200, own.to_string()))
        .listing(&["smith/forge", "seer/eye"]);
    let (status, kind, body) = ask(
        &stub,
        models_request(&[
            ("authorization", "Bearer sk-signed-in"),
            ("chatgpt-account-id", "acct"),
            ("if-none-match", "\"tag\""),
            ("if-modified-since", "yesterday"),
            ("accept-encoding", "gzip"),
            ("host", "127.0.0.1:1"),
        ]),
    );
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    assert_eq!(
        slugs(&body),
        [
            ("gpt-5".to_owned(), 5),
            ("gpt-5-mini".to_owned(), 2),
            ("smith/forge".to_owned(), 6),
            ("seer/eye".to_owned(), 7),
        ]
    );
    let value: Value = json::from_str(&body).expect("json");
    assert_eq!(value["models"][2]["description"], "smith/forge via hofund");
    let sent = stub.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "GET");
    assert_eq!(
        sent[0].uri,
        "https://backend.test/codex/models?client_version=0.150.0"
    );
    assert_eq!(sent[0].within, Some(LIST_WAIT));
    assert_eq!(
        sent[0].header_names(),
        ["authorization", "chatgpt-account-id"]
    );
    assert!(sent[0].body.is_empty());
    let called = stub.called();
    assert_eq!(called.len(), 1);
    assert_eq!(called[0].uri, "/v1/models");
}

#[test]
fn a_refused_sign_in_is_codexs_to_see() {
    for status in [401_u16, 403] {
        let stub = Stub::default().sending(Ok((
            status,
            vec![("content-type", "application/json")],
            BodyStream::from_bytes(br#"{"detail":"sign in again"}"#.to_vec()),
        )));
        let (seen, kind, body) = ask(&stub, models_request(&[]));
        assert_eq!(
            (seen, kind.as_str(), body.as_str()),
            (status, "application/json", r#"{"detail":"sign in again"}"#)
        );
        assert!(stub.called().is_empty());
    }
}

#[test]
fn no_list_from_the_backend_leaves_hofunds_models_alone() {
    let failures = [
        answer(500, r#"{"models":[{"slug":"gpt-5"}]}"#),
        answer(300, r#"{"models":[{"slug":"gpt-5"}]}"#),
        answer(200, "not json"),
        answer(200, r#"{"models":{"slug":"gpt-5"}}"#),
        answer(200, r#"{"data":[]}"#),
        Err(PluginError::timeout("the upstream did not answer in time")),
        Ok((
            200,
            Vec::new(),
            BodyStream::unfold(0_u8, |step| async move {
                match step {
                    0 => Some((Ok(br#"{"models":[{"slug":"gpt"#.to_vec()), 1)),
                    1 => Some((Err(PluginError::interrupted("cut")), 2)),
                    _ => None,
                }
            }),
        )),
    ];
    for failure in failures {
        let stub = Stub::default().sending(failure).listing(&["smith/forge"]);
        let (status, _, body) = ask(&stub, models_request(&[]));
        assert_eq!(status, 200);
        assert_eq!(slugs(&body), [("smith/forge".to_owned(), 1)]);
    }
}

#[test]
fn a_backend_list_over_the_cap_is_not_read() {
    let cap = 16 * 1024 * 1024;
    let head = r#"{"models":[{"slug":"gpt-5"}]}"#;
    let fits = format!("{head}{}", " ".repeat(cap - head.len()));
    let over = format!("{fits} ");
    let stub = Stub::default()
        .sending(answer(200, fits))
        .listing(&[])
        .sending(answer(200, over))
        .listing(&[]);
    let (_, _, body) = ask(&stub, models_request(&[]));
    assert_eq!(slugs(&body), [("gpt-5".to_owned(), 0)]);
    let (_, _, body) = ask(&stub, models_request(&[]));
    assert!(slugs(&body).is_empty());
}

#[test]
fn hofunds_models_follow_a_list_without_priorities_and_a_failed_list_leaves_the_backends() {
    let stub = Stub::default()
        .sending(answer(200, r#"{"models":[{"slug":"gpt-5"}]}"#))
        .listing(&["smith/forge"])
        .sending(answer(200, r#"{"models":[{"slug":"gpt-5","priority":1}]}"#))
        .calling(Err(PluginError::status_error(503, "down")));
    let (_, _, body) = ask(&stub, models_request(&[]));
    assert_eq!(
        slugs(&body),
        [("gpt-5".to_owned(), 0), ("smith/forge".to_owned(), 1)]
    );
    let (status, _, body) = ask(&stub, models_request(&[]));
    assert_eq!(status, 200);
    assert_eq!(slugs(&body), [("gpt-5".to_owned(), 1)]);
}

#[test]
fn the_model_list_always_asks_the_chatgpt_backend() {
    let stub = Stub::default()
        .sending(answer(200, r#"{"models":[]}"#))
        .listing(&[]);
    ask(&stub, models_request(&[("authorization", "Bearer sk-key")]));
    assert_eq!(
        stub.sent()[0].uri,
        "https://backend.test/codex/models?client_version=0.150.0"
    );
}

fn trigger(model: &str) -> String {
    json!({
        "model": model,
        "stream": true,
        "tools": [{"type": "function", "name": "shell"}],
        "input": [{"type": "message", "role": "user", "content": []}, {"type": "compaction_trigger"}],
    })
    .to_string()
}

fn compaction(stub: &Stub) -> (u16, String, String) {
    ask(
        stub,
        request(
            "POST",
            "/backend-api/codex/responses",
            &[
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
                ("authorization", "Bearer chatgpt"),
            ],
            &trigger("smith/forge"),
        ),
    )
}

/// The events of a stream as `(event, data)`.
fn events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| {
            let (name, data) = event.split_once('\n').expect("two lines");
            (
                name.strip_prefix("event: ").expect("event").to_owned(),
                json::from_str(data.strip_prefix("data: ").expect("data")).expect("json"),
            )
        })
        .collect()
}

#[test]
fn a_hofund_models_compaction_is_its_summary_as_one_compaction_item() {
    let reply = json!({
        "id": "resp_7",
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "content": [{"type": "output_text", "text": "Sum"}, {"type": "output_text", "text": "mary"}]},
            {"type": "message", "content": [{"type": "refusal"}]},
        ],
        "usage": {"input_tokens": 3},
    });
    let stub = Stub::default().calling(answer(200, reply.to_string()));
    let (status, kind, body) = compaction(&stub);
    assert_eq!((status, kind.as_str()), (200, "text/event-stream"));
    assert!(body.ends_with("\n\n"));
    let item =
        json!({"type": "compaction", "id": "cmp_7", "encrypted_content": "hofund1:U3VtbWFyeQ=="});
    assert_eq!(
        events(&body),
        [
            (
                "response.created".to_owned(),
                json!({"type": "response.created", "response": {"id": "resp_7", "object": "response", "status": "in_progress", "output": []}})
            ),
            (
                "response.output_item.added".to_owned(),
                json!({"type": "response.output_item.added", "output_index": 0, "item": item})
            ),
            (
                "response.output_item.done".to_owned(),
                json!({"type": "response.output_item.done", "output_index": 0, "item": item})
            ),
            (
                "response.completed".to_owned(),
                json!({"type": "response.completed", "response": {"id": "resp_7", "object": "response", "status": "completed", "output": [item], "usage": {"input_tokens": 3}}})
            ),
        ]
    );
    let called = stub.called();
    assert_eq!(called.len(), 1);
    assert_eq!(
        (called[0].method.as_str(), called[0].uri.as_str()),
        ("POST", "/v1/responses")
    );
    assert_eq!(
        called[0].headers,
        [
            pair("accept", "application/json"),
            pair("content-type", "application/json")
        ]
    );
    let asked: Value = json::from_slice(&called[0].body).expect("json");
    assert_eq!(asked["stream"], false);
    assert_eq!(asked["tools"], json!([]));
    assert_eq!(
        asked["input"][1]["content"][0]["text"],
        super::input::COMPACT_PROMPT
    );
    assert!(stub.sent().is_empty());
}

#[test]
fn a_summary_streamed_back_is_read_from_its_items() {
    let stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_s\"}}\n\n",
        "data: not json\n\n",
        ": a comment\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"streamed\"}]}}\n\n",
        "data:{\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    let stub = Stub::default().calling(answer(200, stream));
    let (status, _, body) = compaction(&stub);
    assert_eq!(status, 200);
    let events = events(&body);
    assert_eq!(events.len(), 4);
    let done = &events[3].1["response"];
    assert_eq!(done["id"], "resp_s");
    assert_eq!(done["output"][0]["id"], "cmp_s");
    assert_eq!(
        done["output"][0]["encrypted_content"],
        super::input::sealed("streamed")
    );
    assert_eq!(done.get("usage"), None);
}

#[test]
fn a_summary_without_an_id_gets_one() {
    let reply = json!({"output": [{"type": "message", "content": [{"text": "s"}]}]});
    let stub = Stub::default().calling(answer(200, reply.to_string()));
    let (status, _, body) = compaction(&stub);
    assert_eq!(status, 200);
    let events = events(&body);
    let id = events[0].1["response"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let nanos = id.strip_prefix("resp_hofund_").expect("made up");
    assert!(nanos.parse::<u128>().expect("nanos") > 0);
    assert_eq!(events[1].1["item"]["id"], format!("cmp_hofund_{nanos}"));
}

#[test]
fn a_summary_cut_short_is_no_summary() {
    for (reply, why) in [
        (
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [{"type": "message", "content": [{"text": "half"}]}]}).to_string(),
            "max_output_tokens",
        ),
        (json!({"status": "incomplete"}).to_string(), "unknown"),
        (
            "data: {\"type\":\"response.incomplete\",\"response\":{}}\n\n".to_owned(),
            "unknown",
        ),
    ] {
        let stub = Stub::default().calling(answer(200, reply));
        let (status, _, body) = compaction(&stub);
        assert_eq!(status, 502);
        assert_eq!(
            message(&body),
            format!("compaction: the summary was cut short ({why}), so there is no summary")
        );
    }
}

#[test]
fn a_reply_without_a_summary_says_what_it_held() {
    for (output, held) in [
        (json!([]), " (it answered with nothing)"),
        (
            json!([
                {"type": "function_call", "name": "shell"},
                {"type": "reasoning", "name": ""},
                {"type": "function_call", "name": "shell"},
                {"name": "x"},
                {"type": ""},
                {"type": "message", "content": [{"text": "  \n"}]},
            ]),
            " (it answered with function_call shell, reasoning, an untyped item x, an untyped item, message)",
        ),
    ] {
        let reply = json!({"id": "resp_1", "status": "completed", "output": output});
        let stub = Stub::default().calling(answer(200, reply.to_string()));
        let (status, _, body) = compaction(&stub);
        assert_eq!(status, 502);
        assert_eq!(
            message(&body),
            format!("compaction: the model wrote no summary{held}")
        );
    }
}

#[test]
fn a_refused_summary_request_says_who_refused_it_and_why() {
    for (status, reply, said) in [
        (429, r#"{"error":{"message":"slow down"}}"#, "slow down"),
        (
            400,
            r#"{"error":{"message":" "}}"#,
            r#"{"error":{"message":" "}}"#,
        ),
        (503, " busy \n", "busy"),
        (500, "", "nothing more"),
    ] {
        let stub = Stub::default().calling(answer(status, reply));
        let (got, _, body) = compaction(&stub);
        assert_eq!(got, status);
        assert_eq!(
            message(&body),
            format!("compaction: hofund answered the summary request {status}: {said}")
        );
    }
    let stub = Stub::default().calling(answer(399, "{}"));
    assert_eq!(compaction(&stub).0, 502);
    let stub = Stub::default().calling(Err(PluginError::status_error(404, "no such model")));
    let (status, _, body) = compaction(&stub);
    assert_eq!((status, message(&body).as_str()), (404, "no such model"));
}

#[test]
fn a_failed_summary_stream_says_what_failed() {
    for (event, why) in [
        (
            json!({"type": "response.failed", "response": {"error": {"message": "boom"}}, "message": "other"}),
            "boom",
        ),
        (json!({"type": "error", "message": "plain"}), "plain"),
        (
            json!({"type": "error", "message": "", "error": {"message": "nested"}}),
            "nested",
        ),
        (
            json!({"type": "response.failed", "response": {"error": {"message": ""}}}),
            "the model failed",
        ),
    ] {
        let stream = format!("data: {event}\n\ndata: {{\"type\":\"response.completed\"}}\n\n");
        let stub = Stub::default().calling(answer(200, stream));
        let (status, _, body) = compaction(&stub);
        assert_eq!(status, 502);
        assert_eq!(
            message(&body),
            format!("compaction: {why}, so there is no summary")
        );
    }
}

#[test]
fn a_summary_reply_that_cannot_be_read_is_no_summary() {
    let stub = Stub::default().calling(answer(200, " {oops"));
    let (status, _, body) = compaction(&stub);
    assert_eq!(status, 502);
    assert_eq!(
        message(&body),
        "compaction: the summary reply is not JSON, so there is no summary"
    );
    let big = vec![b' '; 16 * 1024 * 1024 + 1];
    let stub = Stub::default().calling(answer(200, big));
    let (status, _, body) = compaction(&stub);
    assert_eq!(status, 502);
    assert_eq!(
        message(&body),
        "compaction: the summary reply could not be read"
    );
    let mut fits = vec![b' '; 16 * 1024 * 1024 - 64];
    fits.extend_from_slice(br#"{"output":[{"type":"message","content":[{"text":"s"}]}]}"#);
    let stub = Stub::default().calling(answer(200, fits));
    assert_eq!(compaction(&stub).0, 200);
    let stub = Stub::default().calling(Ok((
        200,
        Vec::new(),
        BodyStream::unfold(true, |first| async move {
            first.then(|| (Err(PluginError::internal("cut")), false))
        }),
    )));
    assert_eq!(
        message(&compaction(&stub).2),
        "compaction: the summary reply could not be read"
    );
}

#[test]
fn codexs_own_models_get_their_input_put_right_and_their_trigger_relayed() {
    let stub = Stub::default()
        .sending(answer(200, "relayed"))
        .sending(answer(200, "compacted"));
    let body = json!({
        "model": "gpt-5",
        "input": [
            {"type": "custom_tool_call", "id": "fc_1"},
            {"type": "compaction", "encrypted_content": super::input::sealed("s")},
            {"type": "compaction_trigger"},
        ],
    })
    .to_string();
    for (path, said) in [
        ("/responses", "relayed"),
        ("/responses/compact", "compacted"),
    ] {
        let (status, _, text) = ask(
            &stub,
            request("POST", &format!("/backend-api/codex{path}"), &[], &body),
        );
        assert_eq!((status, text.as_str()), (200, said));
    }
    let sent = stub.sent();
    assert_eq!(sent.len(), 2);
    for seen in &sent {
        let value: Value = json::from_slice(&seen.body).expect("json");
        assert_eq!(value["input"][0]["id"], "ctc_1");
        assert_eq!(value["input"][1]["type"], "message");
        assert_eq!(value["input"][2], json!({"type": "compaction_trigger"}));
    }
    assert!(stub.called().is_empty());
    let get = Stub::default().sending(answer(200, "got"));
    let (_, _, text) = ask(
        &get,
        request("GET", "/backend-api/codex/responses", &[], &body),
    );
    assert_eq!(text, "got");
    assert_eq!(get.sent()[0].body, body.as_bytes());
}

#[test]
fn a_hofund_model_gets_its_summaries_opened() {
    let stub = Stub::default().calling(answer(200, "ok"));
    let body = json!({
        "model": "smith/forge",
        "input": [{"type": "compaction", "encrypted_content": super::input::sealed("s")}],
    })
    .to_string();
    let (status, _, text) = ask(
        &stub,
        request(
            "POST",
            "/backend-api/codex/responses",
            &[("accept", "text/event-stream")],
            &body,
        ),
    );
    assert_eq!((status, text.as_str()), (200, "ok"));
    let called = stub.called();
    let value: Value = json::from_slice(&called[0].body).expect("json");
    assert_eq!(value["input"][0]["role"], "user");
    assert_eq!(value.get("stream"), None);
    assert_eq!(called[0].headers, [pair("accept", "text/event-stream")]);
}
