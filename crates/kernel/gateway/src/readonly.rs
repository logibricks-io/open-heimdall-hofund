use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::MethodRouter;

use super::{Inner, authenticate_request, json_response};

pub(super) const READ_ONLY_CODE: &str = "gateway_key_read_only";
// `/health` stands in for the reference's `GET /` and `GET /v1` information
// routes (reference: internal/gateway/readonly.go:25-28); it spends nothing.
const READ_ONLY_PATHS: &[&str] = &[
    "/health",
    "/me",
    "/v1/hofund/me",
    "/v1/hofund/quotas",
    "/v1/hofund/quotas/history",
];

pub(super) struct RouteSet {
    pub(super) router: axum::Router<Arc<Inner>>,
    pub(super) patterns: Vec<String>,
}

impl RouteSet {
    pub(super) fn new() -> Self {
        Self {
            router: axum::Router::new(),
            patterns: Vec::new(),
        }
    }

    pub(super) fn route(
        mut self,
        methods: &[&str],
        path: &str,
        route: MethodRouter<Arc<Inner>>,
    ) -> Self {
        self.patterns
            .extend(methods.iter().map(|method| format!("{method} {path}")));
        self.router = self.router.route(path, route);
        self
    }
}

pub(super) async fn guard(
    State(app): State<Arc<Inner>>,
    mut request: Request,
    next: Next,
) -> Response {
    if app.keys.is_some()
        && let Ok(identity) = authenticate_request(
            &app,
            request.headers(),
            request.uri(),
            request.extensions().get().cloned(),
            None,
        )
    {
        if identity.read_only && !allowed(request.method(), request.uri().path()) {
            return refusal();
        }
        request.extensions_mut().insert(identity);
    }
    next.run(request).await
}

fn allowed(method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD) && READ_ONLY_PATHS.contains(&path)
}

fn refusal() -> Response {
    json_response(
        StatusCode::FORBIDDEN,
        serde_json::to_vec(&serde_json::json!({
            "error": {
                "message": "this gateway key is read-only: it may read hofund's quotas and its own usage (GET /v1/hofund/quotas, /v1/hofund/quotas/history, /v1/hofund/me) and use no model or write. Use a key with full access for that (hofund gateway-key access <id> full)",
                "type": "permission_error",
                "code": READ_ONLY_CODE,
            }
        }))
        .unwrap_or_else(|_| b"{\"error\":{\"code\":\"gateway_key_read_only\"}}".to_vec()),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use futures_util::future::BoxFuture;
    use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
    use hofund_control_proto::{QuotaHistory, QuotaHistoryQuery, QuotaQuery, QuotaResponse};
    use hofund_ir::{Message, Request as ModelRequest, Response as ModelResponse, Role};
    use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
    use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
    use hofund_secrets::{KeyLimit, KeyStore};
    use hofund_usage::{Ledger, Row};
    use tower::ServiceExt;

    use super::super::{
        GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway, QuotaSource as GatewayQuotaSource,
    };
    use super::{READ_ONLY_CODE, READ_ONLY_PATHS};

    struct FakeMember {
        id: MemberId,
        route: &'static str,
        protocol: &'static str,
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Member for FakeMember {
        fn id(&self) -> &MemberId {
            &self.id
        }

        fn route_id(&self) -> &str {
            self.route
        }

        fn protocol(&self) -> &str {
            self.protocol
        }

        async fn execute(
            &self,
            _request: ModelRequest,
            _inbound: &str,
        ) -> Result<Completion, PluginError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::Unary(Box::new(ModelResponse::new(
                Message::new(Role::Assistant),
            ))))
        }
    }

    struct EmptyQuotas;

    impl GatewayQuotaSource for EmptyQuotas {
        fn report(&self, _query: QuotaQuery) -> BoxFuture<'_, QuotaResponse> {
            Box::pin(async { QuotaResponse::default() })
        }

        fn history(&self, _query: QuotaHistoryQuery) -> Vec<QuotaHistory> {
            Vec::new()
        }
    }

    struct Fixture {
        gateway: Gateway,
        store: Arc<KeyStore>,
        usage: Arc<Ledger>,
        hits: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
        store.list().expect("migrate");
        let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
        let hits = Arc::new(AtomicUsize::new(0));
        let member = |route: &'static str, protocol: &'static str| {
            Arc::new(FakeMember {
                id: MemberId::from_preset(
                    PresetId::try_from(route).expect("preset"),
                    MemberKey::from_static("row"),
                ),
                route,
                protocol,
                hits: Arc::clone(&hits),
            }) as Arc<dyn Member>
        };
        let routes = BTreeMap::from([
            ("demo".to_owned(), vec![member("primary", "plain")]),
            ("other".to_owned(), vec![member("spare", "plain")]),
        ]);
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
                "/v1/messages".to_owned(),
                Arc::new(AnthropicMessages) as Arc<dyn Codec>,
            ),
            (
                GENERATIVE_UNARY.to_owned(),
                Arc::new(GenerativeLanguage) as Arc<dyn Codec>,
            ),
            (
                GENERATIVE_STREAM.to_owned(),
                Arc::new(GenerativeLanguage) as Arc<dyn Codec>,
            ),
        ]);
        let token = store.token_file().read().expect("token");
        let gateway = Gateway::new(
            Snapshot::new(Router::new(routes, FailoverPolicy::default())),
            token,
            codecs,
            Arc::clone(&usage),
        )
        .with_key_store(Arc::clone(&store))
        .with_quotas(Arc::new(EmptyQuotas));
        Fixture {
            gateway,
            store,
            usage,
            hits,
            _dir: dir,
        }
    }

    async fn request(
        app: &axum::Router,
        method: &str,
        uri: &str,
        secret: Option<&str>,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(secret) = secret {
            builder = builder.header("authorization", format!("Bearer {secret}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .expect("body")
            .to_vec();
        (status, headers, body)
    }

    const REFUSED: &[&str] = &[
        "GET /v1/models",
        "POST /v1/chat/completions",
        "POST /v1/messages",
        "POST /v1/messages/count_tokens",
        "GET /v1/responses",
        "POST /v1/responses",
        "GET /responses",
        "POST /responses",
        "GET /v1beta/models",
        "POST /v1beta/models/{*target}",
        "*",
    ];

    fn sample(pattern: &str) -> (&str, String) {
        if pattern == "*" {
            return ("GET", "/not-a-route".to_owned());
        }
        let (method, path) = pattern.split_once(' ').expect("route pattern");
        (method, path.replace("{*target}", "demo:generateContent"))
    }

    #[tokio::test]
    async fn r1_read_only_key_reads_stats_and_everything_else_gets_the_exact_403() {
        let fixture = fixture();
        let display = fixture
            .store
            .add_with_access("Display", Some("display-secret-123"), true)
            .expect("display");
        let full = fixture
            .store
            .add("Worker", Some("worker-secret-123"))
            .expect("worker");
        let app = fixture.gateway.into_router();
        let (status, _, _) = request(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(&full.secret),
            r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
        fixture.hits.store(0, Ordering::SeqCst);

        for (method, path) in [
            ("POST", "/v1/chat/completions"),
            ("GET", "/v1/models"),
            ("POST", "/me"),
            ("GET", "/not-a-route"),
        ] {
            let (status, _, body) =
                request(&app, method, path, Some(&display.secret), "{}", &[]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
            let error: serde_json::Value = serde_json::from_slice(&body).expect("error json");
            assert_eq!(error["error"]["code"], READ_ONLY_CODE);
            assert_eq!(error["error"]["type"], "permission_error");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("read-only")
                        && message.contains("gateway-key access <id> full"))
            );
        }
        for path in READ_ONLY_PATHS {
            let (status, _, _) = request(&app, "GET", path, Some(&display.secret), "", &[]).await;
            assert!(status.is_success(), "read-only GET {path}: {status}");
            let (status, _, body) =
                request(&app, "HEAD", path, Some(&display.secret), "", &[]).await;
            assert!(status.is_success(), "read-only HEAD {path}: {status}");
            assert!(body.is_empty());
        }
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn r2_every_registered_route_is_allowed_or_refused_and_exercised() {
        let fixture = fixture();
        let display = fixture
            .store
            .add_with_access("Display", Some("display-secret-123"), true)
            .expect("display");
        let patterns = super::super::request_routes(&fixture.gateway.inner).patterns;
        let allowed: Vec<String> = READ_ONLY_PATHS
            .iter()
            .map(|path| format!("GET {path}"))
            .collect();
        for pattern in &patterns {
            assert_ne!(
                allowed.contains(pattern),
                REFUSED.contains(&pattern.as_str()),
                "route {pattern} is unclassified or classified twice"
            );
        }
        for pattern in &allowed {
            assert!(
                patterns.contains(pattern),
                "classification has no route {pattern}"
            );
        }
        for pattern in REFUSED {
            assert!(
                patterns.contains(&(*pattern).to_owned()),
                "classification has no route {pattern}"
            );
        }

        let app = fixture.gateway.into_router();
        for pattern in REFUSED {
            let (method, path) = sample(pattern);
            let (status, _, body) =
                request(&app, method, &path, Some(&display.secret), "{}", &[]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "read-only {pattern}");
            let error: serde_json::Value = serde_json::from_slice(&body).expect("error json");
            assert_eq!(error["error"]["code"], READ_ONLY_CODE);
        }
        for pattern in allowed {
            let (_, path) = sample(&pattern);
            let (status, _, _) = request(&app, "GET", &path, Some(&display.secret), "", &[]).await;
            assert!(status.is_success(), "read-only {pattern}: {status}");
        }
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn r3_read_only_key_cannot_upgrade_a_responses_websocket() {
        let fixture = fixture();
        let display = fixture
            .store
            .add_with_access("Display", Some("display-secret-123"), true)
            .expect("display");
        let app = fixture.gateway.into_router();
        for path in ["/v1/responses", "/responses"] {
            let (status, _, body) = request(
                &app,
                "GET",
                path,
                Some(&display.secret),
                "",
                &[
                    ("upgrade", "websocket"),
                    ("connection", "Upgrade"),
                    ("sec-websocket-version", "13"),
                    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let error: serde_json::Value = serde_json::from_slice(&body).expect("error json");
            assert_eq!(error["error"]["code"], READ_ONLY_CODE);
        }
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn r5_me_tells_only_the_calling_key_its_usage_limits_models_and_setup() {
        let fixture = fixture();
        let alice = fixture
            .store
            .add("Alice", Some("alice-secret-123"))
            .expect("alice");
        let bob = fixture
            .store
            .add("Bob", Some("bob-secret-123"))
            .expect("bob");
        fixture
            .store
            .set_models(&alice.key.id, &["primary/*".to_owned()])
            .expect("models");
        let alice_id = alice.key.id.clone();
        let mut stored = fixture.store.export().expect("export");
        stored
            .iter_mut()
            .find(|key| key.id == alice_id)
            .expect("Alice")
            .limits = vec![KeyLimit {
            period: "day".to_owned(),
            tokens: 100,
            ..KeyLimit::default()
        }];
        fixture.store.restore(&stored).expect("limits");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        for (key, name, model, input, output, status) in [
            (&alice_id, "Alice", "demo", 30, 5, "200"),
            (&bob.key.id, "Bob", "other", 40, 7, "200"),
            (&bob.key.id, "Bob", "other", 50, 9, "500"),
        ] {
            fixture
                .usage
                .record(&Row {
                    created_unix: now,
                    model: model.to_owned(),
                    upstream: String::new(),
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    latency_ms: 1,
                    status: status.to_owned(),
                    caller_key_id: key.clone(),
                    caller_key_name: name.to_owned(),
                })
                .expect("usage");
        }
        let app = fixture.gateway.into_router();
        let (status, headers, body) =
            request(&app, "GET", "/v1/hofund/me", Some(&alice.secret), "", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let text = String::from_utf8(body).expect("json text");
        for secret in [&alice.secret, &bob.secret] {
            assert!(!text.contains(secret));
        }
        assert!(!text.contains("Bob"));
        assert!(!text.contains(&bob.key.id));
        let info: serde_json::Value = serde_json::from_str(&text).expect("me json");
        assert_eq!(info["name"], "Alice");
        assert_eq!(info["readOnly"], false);
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        for period in ["today", "7d", "30d"] {
            assert_eq!(info["usage"][period]["calls"], 1);
            assert_eq!(info["usage"][period]["errors"], 0);
            assert_eq!(info["usage"][period]["input"], 30);
            assert_eq!(info["usage"][period]["output"], 5);
            assert_eq!(info["usage"][period]["cache_read"], 0);
            assert_eq!(info["usage"][period]["cache_write"], 0);
            assert_eq!(info["usage"][period]["cost"], 0.0);
            assert_eq!(info["usage"][period]["unpriced"], 0);
        }
        assert_eq!(info["topModels"].as_array().map(Vec::len), Some(1));
        assert_eq!(info["topModels"][0]["id"], "demo");
        assert_eq!(info["topModels"][0]["calls"], 1);
        assert_eq!(info["models"].as_array().map(Vec::len), Some(1));
        assert_eq!(info["models"][0]["id"], "demo");
        assert_eq!(info["models"][0]["label"], "demo");
        assert_eq!(info["limits"].as_array().map(Vec::len), Some(1));
        assert_eq!(info["limits"][0]["period"], "day");
        assert_eq!(info["limits"][0]["tokenLimit"], 100);
        assert_eq!(info["limits"][0]["tokens"], 35);
        assert_eq!(info["limits"][0]["tokensLeft"], 65);
        assert_eq!(info["limits"][0]["spent"], false);

        fixture
            .store
            .set_read_only(&alice_id, true)
            .expect("read-only");
        let (status, _, body) =
            request(&app, "GET", "/v1/hofund/me", Some(&alice.secret), "", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_slice(&body).expect("me json");
        assert_eq!(info["readOnly"], true);
        assert_eq!(info["models"].as_array().map(Vec::len), Some(0));
        assert_eq!(info["usage"]["30d"]["calls"], 1);

        // a limit used to the last token is spent, as in the reference
        let mut stored = fixture.store.export().expect("export");
        stored
            .iter_mut()
            .find(|key| key.id == alice_id)
            .expect("Alice")
            .limits[0]
            .tokens = 35;
        fixture.store.restore(&stored).expect("limits");
        let (_, _, body) =
            request(&app, "GET", "/v1/hofund/me", Some(&alice.secret), "", &[]).await;
        let info: serde_json::Value = serde_json::from_slice(&body).expect("me json");
        assert_eq!(info["limits"][0]["tokensLeft"], 0);
        assert_eq!(info["limits"][0]["spent"], true);

        let (status, headers, body) = request(&app, "GET", "/me", None, "", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let csp = headers["content-security-policy"].to_str().expect("CSP");
        for directive in [
            "default-src 'none'",
            "connect-src 'self'",
            "form-action 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(csp.contains(directive));
        }
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        let page = String::from_utf8(body).expect("page");
        for content in [
            "sessionStorage",
            "Authorization: \"Bearer \" + secret",
            "supports_websockets = true",
            "Put my key in the setup",
        ] {
            assert!(page.contains(content), "page lacks {content}");
        }
        assert!(!page.contains("?key="));
        assert!(!page.contains("alice-secret-123"));
        assert!(!page.contains("bob-secret-123"));

        fixture.store.set_enabled(&bob.key.id, false).expect("off");
        for secret in [None, Some("unknown-secret"), Some(&bob.secret)] {
            let (status, _, _) = request(&app, "GET", "/v1/hofund/me", secret, "", &[]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _, _) = request(
            &app,
            "GET",
            "/v1/hofund/me?key=withheld",
            Some(&alice.secret),
            "",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let legacy = Gateway::new(
            Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
            "legacy-token".to_owned(),
            HashMap::new(),
            Arc::clone(&fixture.usage),
        )
        .into_router();
        let (status, _, _) = request(
            &legacy,
            "GET",
            "/v1/hofund/me",
            Some("legacy-token"),
            "",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
