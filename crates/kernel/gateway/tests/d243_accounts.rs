//! Account list and the per-request account pin at the HTTP edge.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::{Gateway, QuotaAccess, QuotaCaller, QuotaScope, QuotaScopeHook};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::account_pin::AccountFace;
use hofund_router::{
    CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Request, Router, Snapshot,
};
use hofund_secrets::KeyStore;
use hofund_usage::Ledger;
use serde_json::Value;
use tower::ServiceExt;

struct Fake {
    member_id: MemberId,
    provider: &'static str,
    key_id: &'static str,
    hits: Arc<AtomicUsize>,
    status: u16,
    message: &'static str,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.provider
    }

    fn scope_provider(&self) -> &str {
        self.provider
    }

    fn provider_key_id(&self) -> &str {
        self.key_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    fn account_refs(&self) -> Vec<String> {
        vec![self.key_id.to_owned()]
    }

    async fn execute(
        &self,
        _request: Request,
        _inbound: &str,
    ) -> Result<hofund_router::Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::status_error(self.status, self.message))
    }
}

fn member(provider: &'static str, key: &'static str, hits: &Arc<AtomicUsize>) -> Arc<Fake> {
    Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from(provider).expect("preset"),
            MemberKey::try_from(key).expect("member key"),
        ),
        provider,
        key_id: key,
        hits: Arc::clone(hits),
        status: 401,
        message: "Incorrect API key",
    })
}

fn router_with(accounts: Vec<AccountFace>, hops: Vec<Hop>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), hops);
    Router::from_hops(routes, FailoverPolicy::default()).with_accounts(accounts)
}

fn gateway(router: Router, scope: Option<QuotaScopeHook>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    let mut gateway = Gateway::new(
        Snapshot::new(router),
        "owner-secret".to_owned(),
        codecs,
        usage,
    );
    if let Some(scope) = scope {
        gateway = gateway.with_quota_scope(scope);
    }
    (gateway.into_router(), dir)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    account: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
    let mut request = HttpRequest::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        request = request.header("authorization", format!("Bearer {auth}"));
    }
    if let Some(account) = account {
        request = request.header("x-hofund-account", account);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8(bytes.to_vec()).expect("text"))
}

fn faces() -> Vec<AccountFace> {
    vec![
        AccountFace::key("keyed", "aaa111bbb2", "work", "api", true).with_provider_name("Keyed"),
        AccountFace::key("keyed", "ccc333ddd4", "spare", "", false),
        AccountFace::account("acct", "login-2", "spare@example.com", "plus", true),
    ]
}

#[tokio::test]
async fn d243_list_hides_secrets_filters_and_scopes() {
    let secret = "sk-live-secret-one";
    let (app, _dir) = gateway(router_with(faces(), Vec::new()), None);
    let (status, body) = call(&app, "GET", "/v1/hofund/accounts", None, None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, body) = call(
        &app,
        "GET",
        "/v1/hofund/accounts",
        Some("owner-secret"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(secret));
    assert!(!body.contains("refresh_token"));
    let listed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(listed["object"], "list");
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(3));
    let work = listed["data"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["id"] == "aaa111bbb2")
        .expect("work");
    assert_eq!(work["provider"], "keyed");
    assert_eq!(work["providerName"], "Keyed");
    assert_eq!(work["kind"], "key");
    assert_eq!(work["pin"], "keyed/aaa111bbb2");
    assert_eq!(work["on"], true);
    assert_eq!(work["label"], "work");
    assert!(work.get("key").is_none());

    let (status, body) = call(
        &app,
        "GET",
        "/v1/hofund/accounts?provider=ACCT",
        Some("owner-secret"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["data"][0]["user"], "spare@example.com");
    assert_eq!(listed["data"][0]["kind"], "account");

    let held = Arc::new(|_caller: QuotaCaller<'_>| QuotaAccess::Refused) as QuotaScopeHook;
    let (app, _dir) = gateway(router_with(faces(), Vec::new()), Some(held));
    let (status, body) = call(
        &app,
        "GET",
        "/v1/hofund/accounts",
        Some("owner-secret"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let error: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(error["error"]["code"], "gateway_key_held");

    struct OnlyUser;
    impl QuotaScope for OnlyUser {
        fn card(&self, provider: &str, user: &str) -> bool {
            provider == "acct" && user.eq_ignore_ascii_case("spare@example.com")
        }
    }
    let scoped = Arc::new(|_caller: QuotaCaller<'_>| QuotaAccess::Scoped(Arc::new(OnlyUser)))
        as QuotaScopeHook;
    let (app, _dir) = gateway(router_with(faces(), Vec::new()), Some(scoped));
    let (status, body) = call(
        &app,
        "GET",
        "/v1/hofund/accounts",
        Some("owner-secret"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["data"][0]["id"], "login-2");
    assert!(!body.contains(secret));
    assert!(!body.contains("aaa111bbb2"));
}

#[tokio::test]
async fn d243_read_only_key_lists_accounts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migrate");
    let token = store.token_file().read().expect("token");
    let reader = store
        .add("Reader", Some("reader-secret-value"))
        .expect("reader");
    store
        .set_read_only(&reader.key.id, true)
        .expect("read-only");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    let app = Gateway::new(
        Snapshot::new(router_with(faces(), Vec::new())),
        token,
        codecs,
        usage,
    )
    .with_key_store(store)
    .into_router();
    let (status, body) = call(
        &app,
        "GET",
        "/v1/hofund/accounts",
        Some("reader-secret-value"),
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("reader-secret-value"));
    assert!(body.contains("aaa111bbb2"));
}

#[tokio::test]
async fn d243_request_pin_returns_that_accounts_error() {
    let bad_hits = Arc::new(AtomicUsize::new(0));
    let good_hits = Arc::new(AtomicUsize::new(0));
    let bad = member("keyed", "aaa111bbb2", &bad_hits);
    let good = member("keyed", "ccc333ddd4", &good_hits);
    let accounts = vec![
        AccountFace::key("keyed", "aaa111bbb2", "broken", "api", true),
        AccountFace::key("keyed", "ccc333ddd4", "work", "api", true),
    ];
    let (app, _dir) = gateway(
        router_with(
            accounts,
            vec![
                Hop::new(bad as Arc<dyn Member>),
                Hop::new(good as Arc<dyn Member>),
            ],
        ),
        None,
    );
    let (status, body) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        Some("owner-secret"),
        Some("keyed/aaa111bbb2"),
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("Incorrect API key"), "{body}");
    assert!(!body.contains("sk-"));
    assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    assert_eq!(good_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d243_pin_http_statuses_are_429_403_400_and_404() {
    let chat = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;
    let resting_hits = Arc::new(AtomicUsize::new(0));
    let other_hits = Arc::new(AtomicUsize::new(0));
    let resting = member("keyed", "aaa111bbb2", &resting_hits);
    let other = member("keyed", "ccc333ddd4", &other_hits);
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    cooldowns.record_failure_for_model(
        resting.id(),
        "demo",
        &PluginError::status_error(429, "slow down"),
    );
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            Hop::new(Arc::clone(&resting) as Arc<dyn Member>),
            Hop::new(Arc::clone(&other) as Arc<dyn Member>),
        ],
    );
    let resting_router = Router::from_hops(routes, FailoverPolicy::default())
        .with_cooldowns(cooldowns)
        .with_accounts(vec![
            AccountFace::key("keyed", "aaa111bbb2", "tired", "", true),
            AccountFace::key("keyed", "ccc333ddd4", "work", "", true),
        ]);
    let (app, _dir) = gateway(resting_router, None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        Some("owner-secret"),
        Some("tired"),
        chat,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(body.contains("rests"), "{body}");
    assert!(body.contains("no other account is tried"), "{body}");
    assert_eq!(resting_hits.load(Ordering::SeqCst), 0);
    assert_eq!(other_hits.load(Ordering::SeqCst), 0);

    let held_hits = Arc::new(AtomicUsize::new(0));
    let open_hits = Arc::new(AtomicUsize::new(0));
    let held = member("keyed", "aaa111bbb2", &held_hits);
    let open = member("keyed", "ccc333ddd4", &open_hits);
    let accounts = vec![
        AccountFace::key("keyed", "aaa111bbb2", "held", "", true),
        AccountFace::key("keyed", "ccc333ddd4", "open", "", true),
    ];
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migrate");
    let token = store.token_file().read().expect("token");
    let limited = store
        .add("Limited", Some("limited-secret-value"))
        .expect("key");
    store
        .set_accounts(
            &limited.key.id,
            &["keyed/ccc333ddd4".to_owned()],
            &["keyed/aaa111bbb2".to_owned(), "keyed/ccc333ddd4".to_owned()],
        )
        .expect("accounts");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    let app = Gateway::new(
        Snapshot::new(router_with(
            accounts,
            vec![
                Hop::new(held as Arc<dyn Member>),
                Hop::new(open as Arc<dyn Member>),
            ],
        )),
        token,
        codecs,
        usage,
    )
    .with_key_store(store)
    .into_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        Some("limited-secret-value"),
        Some("keyed/aaa111bbb2"),
        chat,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("may not use"), "{body}");
    assert_eq!(held_hits.load(Ordering::SeqCst), 0);
    assert_eq!(open_hits.load(Ordering::SeqCst), 0);

    let plan_hits = Arc::new(AtomicUsize::new(0));
    let planned = member("keyed", "aaa111bbb2", &plan_hits);
    let (app, _dir) = gateway(
        router_with(
            vec![
                AccountFace::key("keyed", "aaa111bbb2", "narrow", "plus", true)
                    .with_models(vec!["other".to_owned()]),
            ],
            vec![Hop::new(planned as Arc<dyn Member>)],
        ),
        None,
    );
    let (status, body) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        Some("owner-secret"),
        Some("narrow"),
        chat,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("plan doesn't list demo"), "{body}");
    assert_eq!(plan_hits.load(Ordering::SeqCst), 0);

    let miss_hits = Arc::new(AtomicUsize::new(0));
    let live = member("keyed", "aaa111bbb2", &miss_hits);
    let (app, _dir) = gateway(
        router_with(
            vec![AccountFace::key("keyed", "aaa111bbb2", "work", "", true)],
            vec![Hop::new(live as Arc<dyn Member>)],
        ),
        None,
    );
    let (status, body) = call(
        &app,
        "POST",
        "/v1/chat/completions",
        Some("owner-secret"),
        Some("missing"),
        chat,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("no account"), "{body}");
    assert_eq!(miss_hits.load(Ordering::SeqCst), 0);
}
