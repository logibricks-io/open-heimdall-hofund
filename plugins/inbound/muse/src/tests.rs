//! The surface against a stand-in gateway.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use hofund_plugin_api::{Body, BodyStream, InboundSurface, Method, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, HttpResponse, async_trait, text_response};

use super::{Kernel, LIST_CAP, Muse, failure_message, serve};

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

/// The stand-in gateway: answers `/v1/models` with `status` and `body`, or
/// fails the call with `error`.
struct Gateway {
    status: u16,
    body: Vec<u8>,
    error: Option<&'static str>,
    seen: Mutex<Vec<(String, String)>>,
}

impl Gateway {
    fn listing(data: &Value) -> Self {
        Self::answering(200, json!({"object": "list", "data": data}).to_string())
    }

    fn answering(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            error: None,
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Kernel for Gateway {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        self.seen.lock().expect("seen").push((
            request.method().as_str().to_owned(),
            request.uri().path().to_owned(),
        ));
        if let Some(error) = self.error {
            return Err(PluginError::unsupported(error));
        }
        let mut response = text_response(self.status, "");
        *response.body_mut() = BodyStream::from_bytes(self.body.clone());
        Ok(response)
    }
}

fn get(path: &str) -> HttpRequest {
    let mut request = HttpRequest::new(Body::empty());
    *request.uri_mut() = path.parse().expect("path");
    request
}

/// What `GET path` answers: status, content type, and body.
fn ask(gateway: &Gateway, path: &str) -> (u16, String, String) {
    block_on(async {
        let response = serve(gateway, &get(path)).await;
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

#[test]
fn the_route_is_muses() {
    let surface = Muse;
    assert_eq!(surface.id().as_str(), "muse");
    let routes: Vec<(Method, &str)> = surface
        .routes()
        .iter()
        .map(|route| (route.method(), route.prefix()))
        .collect();
    assert_eq!(routes, [(Method::Get, "/muse-code/models")]);
    let _ = super::contribute();
}

#[test]
fn every_model_is_a_row_muse_shows() {
    let gateway = Gateway::listing(&json!([
        {"id": "seer", "object": "model", "owned_by": "hofund"},
        {"id": "smith/forge", "owned_by": "smith"},
        {"id": "plain"},
        {"id": "blank", "owned_by": ""},
        {"object": "model"},
        {"id": 7},
    ]));
    let (status, kind, body) = ask(&gateway, "/muse-code/models");
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    let list: Value = json::from_str(&body).expect("json");
    assert_eq!(list["object"], "list");
    let rows = list["data"].as_array().expect("data");
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["seer", "smith/forge", "plain", "blank"]);
    assert_eq!(
        rows[1],
        json!({
            "id": "smith/forge", "object": "model", "created": 0, "owned_by": "smith",
            "metadata": {"muse-code": {
                "name": "smith/forge", "family": "smith", "is_hidden": false,
                "attachment": false, "reasoning": false, "temperature": false,
                "tool_call": true,
                "modalities": {"input": ["text"], "output": ["text"]},
                "options": {"include": []}, "variants": {},
                "description": "smith/forge via hofund",
                "limit": {"context": 128_000, "output": 32_000},
            }},
        })
    );
    // A limit without its window or its reply size hides the model in Muse,
    // and with no reply size Muse asks for replies as long as the window.
    for row in &rows[2..] {
        assert_eq!(row["owned_by"], "hofund");
        assert_eq!(row["metadata"]["muse-code"]["family"], "hofund");
    }
    assert_eq!(
        *gateway.seen.lock().expect("seen"),
        [("GET".to_owned(), "/v1/models".to_owned())]
    );
}

#[test]
fn no_models_is_an_empty_list() {
    for body in [
        json!({"object": "list", "data": []}),
        json!({"object": "list"}),
    ] {
        let gateway = Gateway::answering(200, body.to_string());
        let (status, _, body) = ask(&gateway, "/muse-code/models");
        assert_eq!(status, 200);
        assert_eq!(
            json::from_str::<Value>(&body).expect("json"),
            json!({"object": "list", "data": []})
        );
    }
}

#[test]
fn a_deeper_path_is_not_found_and_asks_nothing() {
    let gateway = Gateway::listing(&json!([{"id": "seer"}]));
    for path in ["/muse-code/models/seer", "/muse-code/models/", "/muse-code"] {
        let (status, _, body) = ask(&gateway, path);
        assert_eq!(
            (status, body.as_str()),
            (404, "404 page not found"),
            "{path}"
        );
    }
    assert!(gateway.seen.lock().expect("seen").is_empty());
}

#[test]
fn a_failing_model_list_keeps_its_status() {
    let gateway = Gateway::answering(401, r#"{"error":{"message":"bad key"}}"#);
    let (status, kind, body) = ask(&gateway, "/muse-code/models");
    assert_eq!((status, kind.as_str()), (401, "application/json"));
    assert_eq!(
        json::from_str::<Value>(&body).expect("json"),
        json!({"error": {"message": "bad key", "type": "hofund_error"}})
    );
    // A redirect is no answer Muse can follow here.
    let gateway = Gateway::answering(302, "moved");
    let (status, _, body) = ask(&gateway, "/muse-code/models");
    assert_eq!(status, 502);
    assert!(body.contains("moved"), "{body}");
}

#[test]
fn a_gateway_that_cannot_list_is_a_bad_gateway() {
    let mut gateway = Gateway::answering(200, "");
    gateway.error = Some("this host has no gateway");
    let (status, _, body) = ask(&gateway, "/muse-code/models");
    assert_eq!(status, 502);
    assert!(body.contains("this host has no gateway"), "{body}");
    let gateway = Gateway::answering(200, "not json");
    let (status, _, body) = ask(&gateway, "/muse-code/models");
    assert_eq!(status, 502);
    assert!(body.contains("the model list"), "{body}");
}

#[test]
fn a_list_past_the_cap_is_refused() {
    let gateway = Gateway::answering(200, vec![b' '; LIST_CAP + 1]);
    let (status, _, body) = ask(&gateway, "/muse-code/models");
    assert_eq!(status, 502);
    assert!(body.contains("over"), "{body}");
    let mut fits = vec![b' '; LIST_CAP - 2];
    fits.splice(0..0, b"{}".iter().copied());
    let gateway = Gateway::answering(200, fits);
    let (status, _, _) = ask(&gateway, "/muse-code/models");
    assert_eq!(status, 200);
}

#[test]
fn failure_messages_take_the_most_specific_text() {
    assert_eq!(failure_message(br#"{"error":{"message":"bad"}}"#), "bad");
    assert_eq!(
        failure_message(br#"{"error":{"message":""}}"#),
        r#"{"error":{"message":""}}"#
    );
    assert_eq!(failure_message(b"  plain  "), "plain");
    assert_eq!(failure_message(b""), "the model list failed");
}
