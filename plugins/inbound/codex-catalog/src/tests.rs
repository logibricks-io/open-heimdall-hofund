//! The surface against a stand-in gateway.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use hofund_plugin_api::{Body, BodyStream, InboundSurface, Method, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, HttpResponse, async_trait, text_response};

use super::{
    BASE_INSTRUCTIONS, CodexCatalog, HEAD, Kernel, LEFT_OUT_HEADER, LIST_CAP, MOST, TAIL, catalog,
    failure_message, fits, serve,
};

/// Runs `future` on this thread. Everything here is ready at once.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
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

    async fn send(
        &self,
        _request: HttpRequest,
        _within: Option<std::time::Duration>,
    ) -> Result<HttpResponse, PluginError> {
        Err(PluginError::unsupported("the catalog sends nothing out"))
    }
}

fn get(path: &str) -> HttpRequest {
    let mut request = HttpRequest::new(Body::empty());
    *request.uri_mut() = path.parse().expect("path");
    request
}

/// What `GET path` answers: status, content type, left-out header, and body.
fn ask(gateway: &Gateway, path: &str) -> (u16, String, Option<String>, String) {
    block_on(async {
        let response = serve(gateway, &get(path)).await;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("ascii").to_owned())
            .unwrap_or_default();
        let left_out = response
            .headers()
            .get(LEFT_OUT_HEADER)
            .map(|value| value.to_str().expect("ascii").to_owned());
        let mut body = response.into_body();
        let mut text = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            text.extend_from_slice(&chunk.expect("chunk"));
        }
        (
            status,
            content_type,
            left_out,
            String::from_utf8(text).expect("utf-8"),
        )
    })
}

#[test]
fn the_route_is_codexs() {
    let surface = CodexCatalog::default();
    assert_eq!(surface.id().as_str(), "codex-catalog");
    let routes: Vec<(Method, &str)> = surface
        .routes()
        .iter()
        .map(|route| (route.method(), route.prefix()))
        .collect();
    assert_eq!(
        routes,
        [
            (Method::Get, "/v1/codex/models"),
            (Method::Get, "/backend-api/codex"),
            (Method::Post, "/backend-api/codex"),
        ]
    );
    let _ = super::contribute();
}

#[test]
fn e2_surface_declares_only_the_backend_responses_socket() {
    assert_eq!(
        CodexCatalog::default().responses_websocket_paths(),
        ["/backend-api/codex/responses"]
    );
}

#[test]
fn every_model_is_an_entry_codex_loads() {
    let gateway = Gateway::listing(&json!([
        {"id": "seer", "object": "model", "owned_by": "hofund"},
        {"id": "smith/forge"},
        {"object": "model"},
        {"id": 7},
    ]));
    let (status, kind, left_out, body) = ask(&gateway, "/v1/codex/models?client_version=0.150.0");
    assert_eq!(
        (status, kind.as_str(), left_out),
        (200, "application/json", None)
    );
    assert!(body.ends_with("]}\n"), "{body}");
    let list: Value = json::from_str(&body).expect("json");
    assert_eq!(list.as_object().expect("object").len(), 1);
    let models = list["models"].as_array().expect("models");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["slug"], "seer");
    assert_eq!(models[0]["priority"], 1);
    assert_eq!(
        models[1],
        json!({
            "slug": "smith/forge",
            "display_name": "smith/forge",
            "description": "smith/forge via hofund",
            "base_instructions": BASE_INSTRUCTIONS,
            "default_reasoning_level": null,
            "supported_reasoning_levels": [],
            "shell_type": "unified_exec",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 2,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": "freeform",
            "truncation_policy": {"mode": "tokens", "limit": 10_000},
            "experimental_supported_tools": [],
            "input_modalities": ["text"],
            "service_tiers": [],
            "supports_search_tool": false,
            "supports_parallel_tool_calls": true,
            "supports_reasoning_summaries": false,
        })
    );
    assert_eq!(
        *gateway.seen.lock().expect("seen"),
        [("GET".to_owned(), "/v1/models".to_owned())]
    );
}

#[test]
fn the_instructions_are_codexs_working_rules() {
    assert!(BASE_INSTRUCTIONS.len() > 1_000);
    assert!(BASE_INSTRUCTIONS.contains("apply_patch"));
    assert!(BASE_INSTRUCTIONS.is_ascii());
}

#[test]
fn no_models_is_an_empty_catalog() {
    for body in [
        json!({"object": "list", "data": []}),
        json!({"object": "list"}),
    ] {
        let gateway = Gateway::answering(200, body.to_string());
        let (status, _, left_out, body) = ask(&gateway, "/v1/codex/models");
        assert_eq!(
            (status, left_out, body.as_str()),
            (200, None, "{\"models\":[]}\n")
        );
    }
}

#[test]
fn a_catalog_past_codexs_limit_leaves_out_the_last() {
    let ids: Vec<String> = (0..600).map(|n| format!("many/model-{n:03}")).collect();
    let (body, left_out) = catalog(&ids);
    assert!(body.len() <= MOST, "{}", body.len());
    let models: Value = json::from_str(&body).expect("json");
    let models = models["models"].as_array().expect("models");
    assert!(left_out > 0);
    assert_eq!(models.len() + left_out, 600);
    assert_eq!(models[0]["slug"], "many/model-000");
    let last = models.len() - 1;
    assert_eq!(models[last]["slug"], format!("many/model-{last:03}"));
    // The first left out would not have fitted.
    let next = super::entry(&ids[models.len()], models.len() + 1).to_string();
    assert!(!fits(body.len() - TAIL.len(), next.len()));

    let gateway = Gateway::listing(&Value::Array(
        ids.iter().map(|id| json!({"id": id})).collect(),
    ));
    let (status, _, header, served) = ask(&gateway, "/v1/codex/models");
    assert_eq!(status, 200);
    assert_eq!(header, Some(left_out.to_string()));
    assert_eq!(served, body);

    // One left out is said too.
    let gateway = Gateway::listing(&Value::Array(
        ids[..=models.len()]
            .iter()
            .map(|id| json!({"id": id}))
            .collect(),
    ));
    let (_, _, header, served) = ask(&gateway, "/v1/codex/models");
    assert_eq!((header.as_deref(), served), (Some("1"), body));
}

#[test]
fn an_entry_that_ends_at_the_limit_fits() {
    let room = MOST - HEAD.len() - 1 - TAIL.len();
    assert!(fits(HEAD.len(), room));
    assert!(!fits(HEAD.len(), room + 1));
    assert!(!fits(HEAD.len() + 1, room));
}

#[test]
fn a_deeper_path_is_not_found_and_asks_nothing() {
    let gateway = Gateway::listing(&json!([{"id": "seer"}]));
    for path in ["/v1/codex/models/seer", "/v1/codex/models/", "/v1/codex"] {
        let (status, _, _, body) = ask(&gateway, path);
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
    let (status, kind, _, body) = ask(&gateway, "/v1/codex/models");
    assert_eq!((status, kind.as_str()), (401, "application/json"));
    assert_eq!(
        json::from_str::<Value>(&body).expect("json"),
        json!({"error": {"message": "bad key", "type": "hofund_error"}})
    );
    // A redirect is no answer Codex can follow here.
    let gateway = Gateway::answering(302, "moved");
    let (status, _, _, body) = ask(&gateway, "/v1/codex/models");
    assert_eq!(status, 502);
    assert!(body.contains("moved"), "{body}");
}

#[test]
fn a_gateway_that_cannot_list_is_a_bad_gateway() {
    let mut gateway = Gateway::answering(200, "");
    gateway.error = Some("this host has no gateway");
    let (status, _, _, body) = ask(&gateway, "/v1/codex/models");
    assert_eq!(status, 502);
    assert!(body.contains("this host has no gateway"), "{body}");
    let gateway = Gateway::answering(200, "not json");
    let (status, _, _, body) = ask(&gateway, "/v1/codex/models");
    assert_eq!(status, 502);
    assert!(body.contains("the model list"), "{body}");
}

#[test]
fn a_list_past_the_cap_is_refused() {
    let gateway = Gateway::answering(200, vec![b' '; LIST_CAP + 1]);
    let (status, _, _, body) = ask(&gateway, "/v1/codex/models");
    assert_eq!(status, 502);
    assert!(body.contains("over"), "{body}");
    let mut fits = vec![b' '; LIST_CAP - 2];
    fits.splice(0..0, b"{}".iter().copied());
    let gateway = Gateway::answering(200, fits);
    let (status, _, _, _) = ask(&gateway, "/v1/codex/models");
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
