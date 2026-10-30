//! The surface against a stand-in gateway.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use hofund_plugin_api::{
    Body, Forward, InboundSurface, Method, PluginError, RawAnswer, RawBody, RouteSpec,
};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, async_trait};

use super::{Kernel, Retrieval, failure, serve};

/// Runs `future` on this thread. Everything here is ready at once.
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

/// The stand-in gateway: builds the body for the member model `wire`, keeps
/// what it saw, and answers with `answer`.
struct Gateway {
    wire: &'static str,
    answer: Result<RawAnswer, fn() -> PluginError>,
    seen: Mutex<Vec<(String, String, RawBody)>>,
}

impl Gateway {
    fn answering(status: u16, body: &str) -> Self {
        Self::typed(status, Some("text/plain"), body)
    }

    fn typed(status: u16, kind: Option<&str>, body: &str) -> Self {
        Self {
            wire: "vendor-embed",
            answer: Ok(RawAnswer::new(
                status,
                kind.map(str::to_owned),
                body.as_bytes().to_vec(),
            )),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn failing(error: fn() -> PluginError) -> Self {
        Self {
            wire: "vendor-embed",
            answer: Err(error),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<(String, String, RawBody)> {
        self.seen.lock().expect("seen").clone()
    }
}

#[async_trait]
impl Kernel for Gateway {
    async fn forward(&self, call: Forward) -> Result<RawAnswer, PluginError> {
        let body = call.body_for(self.wire)?;
        self.seen.lock().expect("seen").push((
            call.model().to_owned(),
            call.path().to_owned(),
            body,
        ));
        self.answer.clone().map_err(|error| error())
    }
}

fn post(path: &str, kind: Option<&str>, body: &str) -> HttpRequest {
    let mut request = HttpRequest::new(Body::from_bytes(body.as_bytes().to_vec()));
    *request.uri_mut() = path.parse().expect("path");
    if let Some(kind) = kind {
        request
            .headers_mut()
            .insert("content-type", kind.parse().expect("content type"));
    }
    request
}

/// What `POST path` with `body` answers: status, content type, and body.
fn ask(gateway: &Gateway, path: &str, body: &str) -> (u16, String, String) {
    ask_typed(gateway, path, None, body)
}

/// What `POST path` with `body` of type `kind` answers.
fn ask_typed(
    gateway: &Gateway,
    path: &str,
    kind: Option<&str>,
    body: &str,
) -> (u16, String, String) {
    block_on(async {
        let response = serve(gateway, &post(path, kind, body)).await;
        let status = response.status().as_u16();
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut stream = response.into_body();
        let mut out = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        (status, kind, String::from_utf8(out).expect("utf8"))
    })
}

fn parsed(body: &str) -> Value {
    json::from_str(body).expect("json body")
}

#[test]
fn routes_are_the_four_posts() {
    let routes = Retrieval.routes();
    assert_eq!(Retrieval.id().as_str(), "retrieval");
    assert_eq!(routes.len(), 4);
    let pairs: Vec<_> = routes
        .iter()
        .map(|route: &RouteSpec| (route.method(), route.prefix()))
        .collect();
    assert_eq!(
        pairs,
        [
            (Method::Post, "/v1/embeddings"),
            (Method::Post, "/v1/rerank"),
            (Method::Post, "/v1/audio/transcriptions"),
            (Method::Post, "/v1/audio/translations"),
        ]
    );
}

#[test]
fn an_embedding_goes_out_with_the_member_model_and_comes_back_as_sent() {
    let gateway = Gateway::answering(200, r#"{"data":[{"embedding":[0.5]}]}"#);
    let (status, kind, body) = ask(
        &gateway,
        "/v1/embeddings",
        r#"{"model":"embed","input":["a","b"],"dimensions":8}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(kind, "text/plain");
    assert_eq!(body, r#"{"data":[{"embedding":[0.5]}]}"#);
    let seen = gateway.seen();
    assert_eq!(seen.len(), 1);
    let (model, path, sent) = &seen[0];
    assert_eq!(model, "embed");
    assert_eq!(path, "/embeddings");
    assert_eq!(sent.content_type(), "application/json");
    assert_eq!(
        parsed(std::str::from_utf8(sent.bytes()).expect("utf8")),
        json!({"model": "vendor-embed", "input": ["a", "b"], "dimensions": 8})
    );
}

#[test]
fn a_rerank_goes_to_the_rerank_path_with_its_status() {
    let gateway = Gateway::answering(201, r#"{"results":[]}"#);
    let (status, _, body) = ask(
        &gateway,
        "/v1/rerank",
        r#"{"model":"rank","query":"q","documents":["d"]}"#,
    );
    assert_eq!((status, body.as_str()), (201, r#"{"results":[]}"#));
    assert_eq!(gateway.seen()[0].1, "/rerank");
}

#[test]
fn a_deeper_path_is_not_found_and_nothing_is_forwarded() {
    let gateway = Gateway::answering(200, "{}");
    for path in [
        "/v1/embeddings/x",
        "/v1/rerank/",
        "/v1/rerankx",
        "/v1/audio/transcriptions/x",
        "/v1/audio/translationsx",
        "/v1/audio",
    ] {
        let (status, _, body) = ask(&gateway, path, r#"{"model":"m"}"#);
        assert_eq!(
            (status, body.as_str()),
            (404, "404 page not found"),
            "{path}"
        );
    }
    assert!(gateway.seen().is_empty());
}

#[test]
fn a_body_without_a_model_is_refused_before_the_kernel() {
    let gateway = Gateway::answering(200, "{}");
    for (body, message) in [
        ("not json", "the body must be a JSON object"),
        ("[1]", "the body must be a JSON object"),
        ("{}", "model is required"),
        (r#"{"model":""}"#, "model is required"),
        (r#"{"model":7}"#, "model is required"),
    ] {
        let (status, kind, answer) = ask(&gateway, "/v1/embeddings", body);
        assert_eq!(status, 400, "{body}");
        assert_eq!(kind, "application/json");
        assert_eq!(
            parsed(&answer),
            json!({"error": {"message": message, "type": "invalid_request_error"}}),
            "{body}"
        );
    }
    assert!(gateway.seen().is_empty());
}

#[test]
fn kernel_failures_keep_their_status_or_become_a_gateway_error() {
    type Case = (fn() -> PluginError, u16, &'static str);
    let cases: [Case; 4] = [
        (
            || PluginError::status_error(404, "no provider"),
            404,
            "hofund_error",
        ),
        (
            || PluginError::quota_status(429, "slow down"),
            429,
            "hofund_error",
        ),
        (|| PluginError::timeout("late"), 504, "hofund_error"),
        (|| PluginError::connect("refused"), 502, "hofund_error"),
    ];
    for (error, status, kind) in cases {
        let gateway = Gateway::failing(error);
        let (got, content, body) = ask(&gateway, "/v1/embeddings", r#"{"model":"m"}"#);
        assert_eq!(got, status);
        assert_eq!(content, "application/json");
        let body = parsed(&body);
        assert_eq!(body["error"]["type"], kind);
        assert_eq!(body["error"]["message"], error().message());
    }
}

#[test]
fn a_status_below_400_is_not_passed_off_as_an_error_status() {
    let response = failure(&PluginError::status_error(200, "odd"));
    assert_eq!(response.status().as_u16(), 502);
}

#[test]
fn an_answer_without_a_type_or_with_a_bad_one_is_json() {
    for kind in [None, Some("bad\ntype")] {
        let gateway = Gateway::typed(200, kind, "{}");
        let (_, got, _) = ask(&gateway, "/v1/rerank", r#"{"model":"m"}"#);
        assert_eq!(got, "application/json", "{kind:?}");
    }
}

const FORM: &str = "multipart/form-data; boundary=cut";

fn audio_form(model: &str) -> String {
    format!(
        "--cut\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\nRIFF\r\n--cut\r\n\
         Content-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--cut--\r\n"
    )
}

#[test]
fn speech_goes_out_as_the_form_with_the_member_model() {
    for (route, upstream) in [
        ("/v1/audio/transcriptions", "/audio/transcriptions"),
        ("/v1/audio/translations", "/audio/translations"),
    ] {
        let gateway = Gateway::typed(200, Some("text/vtt"), "WEBVTT");
        let (status, kind, body) = ask_typed(&gateway, route, Some(FORM), &audio_form(" whisper "));
        assert_eq!(
            (status, kind.as_str(), body.as_str()),
            (200, "text/vtt", "WEBVTT")
        );
        let seen = gateway.seen();
        assert_eq!(seen.len(), 1);
        let (model, path, sent) = &seen[0];
        assert_eq!((model.as_str(), path.as_str()), ("whisper", upstream));
        assert_eq!(sent.content_type(), FORM);
        assert_eq!(
            std::str::from_utf8(sent.bytes()).expect("utf8"),
            audio_form("vendor-embed")
        );
    }
}

#[test]
fn a_form_already_naming_the_member_model_goes_out_unchanged() {
    let gateway = Gateway::answering(200, "{}");
    let form = audio_form(" vendor-embed\t");
    ask_typed(&gateway, "/v1/audio/transcriptions", Some(FORM), &form);
    assert_eq!(gateway.seen()[0].2.bytes().as_ref(), form.as_bytes());
}

#[test]
fn a_bad_speech_form_is_refused_before_the_kernel() {
    let gateway = Gateway::answering(200, "{}");
    let not_form = "send the audio as multipart/form-data, as /v1/audio/transcriptions takes it: a file and a model";
    let no_model = "name the model: send a model field with the audio file";
    let empty = "--cut\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\nx\r\n--cut--";
    for (kind, body, message) in [
        (None, audio_form("m"), not_form),
        (Some("application/json"), audio_form("m"), not_form),
        (
            Some("multipart/form-data; boundary=other"),
            audio_form("m"),
            "the multipart form does not parse",
        ),
        (Some(FORM), audio_form("  "), no_model),
        (Some(FORM), empty.to_owned(), no_model),
    ] {
        let (status, got, answer) = ask_typed(&gateway, "/v1/audio/translations", kind, &body);
        assert_eq!(
            (status, got.as_str()),
            (400, "application/json"),
            "{kind:?}"
        );
        assert_eq!(
            parsed(&answer),
            json!({"error": {"message": message, "type": "invalid_request_error"}}),
            "{kind:?} {body}"
        );
    }
    assert!(gateway.seen().is_empty());
}
