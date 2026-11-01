//! Issue, list, and revoke short-lived gateway keys.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get};
use hofund_secrets::{IssueRequest, KeyStore, MAX_TTL, expiry_text, parse_ttl};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Inner, authenticate_request, bearer, header_secret, json_response, list_error, query_key,
    readonly, relisten,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, relisten::SwapListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, relisten::SwapListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

pub(super) const KEYS_PATH: &str = "/v1/hofund/keys";
const KEYS_ID_PATH: &str = "/v1/hofund/keys/{id}";
const BODY_LIMIT: usize = 1 << 20;
const PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const FORWARD_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "tailscale-user-login",
];

pub(super) fn routes(router: readonly::RouteSet) -> readonly::RouteSet {
    router
        .route(&["GET", "POST"], KEYS_PATH, get(list_keys).post(issue_key))
        .route(&["DELETE"], KEYS_ID_PATH, delete(revoke_key))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct IssueBody {
    name: String,
    ttl: Option<Value>,
    models: Vec<String>,
    accounts: Vec<String>,
    tokens: i64,
    cost: f64,
    read_only: bool,
}

async fn issue_key(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let issuer = match issuer_for(&app, &request) {
        Ok(issuer) => issuer,
        Err(response) => return *response,
    };
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let body: IssueBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "the body is a JSON object: {\"ttl\": \"1h\", \"name\": \"sandbox\", \"models\": []}",
            );
        }
    };
    let ttl = match body
        .ttl
        .as_ref()
        .and_then(|value| parse_api_ttl(value).ok())
    {
        Some(ttl) => ttl,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "ttl is how long the key lives, such as \"1h\", \"30m\" or 3600 (seconds), from 1m to 720h",
            );
        }
    };
    if body.tokens < 0 {
        return error(StatusCode::BAD_REQUEST, "A limit can't be negative");
    }
    let Some(keys) = &app.keys else {
        return error(
            StatusCode::FORBIDDEN,
            "short-lived keys require the named gateway-key store",
        );
    };
    let issued = keys.issue(IssueRequest {
        name: body.name,
        ttl,
        models: body.models,
        accounts: body.accounts,
        tokens: u64::try_from(body.tokens).unwrap_or(0),
        cost: body.cost,
        read_only: body.read_only,
        issuer,
        ..IssueRequest::default()
    });
    match issued {
        Ok(issued) => {
            let mut response = json_response(
                StatusCode::CREATED,
                serde_json::to_vec(&issued)
                    .unwrap_or_else(|_| b"{\"error\":\"could not encode key\"}".to_vec()),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(failure) => error(StatusCode::BAD_REQUEST, &failure.to_string()),
    }
}

async fn list_keys(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let issuer = match issuer_for(&app, &request) {
        Ok(issuer) => issuer,
        Err(response) => return *response,
    };
    let Some(keys) = &app.keys else {
        return error(
            StatusCode::FORBIDDEN,
            "short-lived keys require the named gateway-key store",
        );
    };
    let now = unix_now();
    match keys.issued_by(&issuer) {
        Ok(stored) => {
            let listed: Vec<Value> = stored
                .into_iter()
                .map(|key| {
                    let expires = key.expires.unwrap_or(now);
                    let mut value = json!({
                        "id": key.id,
                        "name": key.name,
                        "key": key.masked,
                        "expires": expiry_text(expires),
                        "expired": now >= expires,
                    });
                    let object = value.as_object_mut().expect("key object");
                    if !key.models.is_empty() {
                        object.insert("models".to_owned(), json!(key.models));
                    }
                    if !key.accounts.is_empty() {
                        object.insert("accounts".to_owned(), json!(key.accounts));
                    }
                    if let Some(limit) = key.limits.first() {
                        object.insert("limit".to_owned(), json!(limit));
                    }
                    if key.read_only {
                        object.insert("readOnly".to_owned(), Value::Bool(true));
                    }
                    value
                })
                .collect();
            json_response(
                StatusCode::OK,
                serde_json::to_vec(&json!({"keys": listed}))
                    .unwrap_or_else(|_| b"{\"keys\":[]}".to_vec()),
            )
        }
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, &failure.to_string()),
    }
}

async fn revoke_key(
    State(app): State<Arc<Inner>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let issuer = match issuer_for(&app, &request) {
        Ok(issuer) => issuer,
        Err(response) => return *response,
    };
    let Some(keys) = &app.keys else {
        return error(StatusCode::NOT_FOUND, "no key issued here has this id");
    };
    match keys.revoke_issued(&id, &issuer) {
        Ok(()) => json_response(
            StatusCode::OK,
            serde_json::to_vec(&json!({"revoked": id}))
                .unwrap_or_else(|_| b"{\"revoked\":true}".to_vec()),
        ),
        Err(_) => error(
            StatusCode::NOT_FOUND,
            &format!("no key issued here has the id {id:?}"),
        ),
    }
}

fn issuer_for(app: &Inner, request: &Request) -> Result<String, Box<Response>> {
    match authenticate_request(
        app,
        request.headers(),
        request.uri(),
        request.extensions().get().cloned(),
        None,
    ) {
        Ok(identity) => {
            if identity.key_id.is_empty() {
                return Err(Box::new(error(
                    StatusCode::FORBIDDEN,
                    "a named gateway key that may issue keys is required",
                )));
            }
            if !identity.issuer.is_empty() {
                return Err(Box::new(error(
                    StatusCode::FORBIDDEN,
                    "this gateway key was issued itself, so it cannot issue keys",
                )));
            }
            if identity.read_only {
                return Err(Box::new(error(
                    StatusCode::FORBIDDEN,
                    "a read-only gateway key cannot issue keys",
                )));
            }
            if !identity.issues || identity.expires.is_some() {
                return Err(Box::new(error(
                    StatusCode::FORBIDDEN,
                    "this gateway key cannot issue keys: enable it with hofund gateway-key issuer <id> on",
                )));
            }
            Ok(identity.key_id)
        }
        Err(unauthorized) => {
            if has_credential(request.headers(), request.uri()) {
                return Err(unauthorized);
            }
            if let Some(message) = local_profile_refused(request) {
                return Err(Box::new(error(StatusCode::FORBIDDEN, &message)));
            }
            Ok(String::new())
        }
    }
}

fn has_credential(headers: &HeaderMap, uri: &Uri) -> bool {
    bearer(headers).is_some()
        || header_secret(headers, "x-api-key").is_some()
        || header_secret(headers, "x-goog-api-key").is_some()
        || uri.query().and_then(query_key).is_some()
}

fn local_profile_refused(request: &Request) -> Option<String> {
    let peer = request.extensions().get::<PeerAddr>().map(|peer| peer.0);
    if !peer.is_some_and(|peer| peer.ip().is_loopback()) {
        return Some(
            "short-lived keys without a gateway key may be issued only from loopback".to_owned(),
        );
    }
    if FORWARD_HEADERS
        .iter()
        .any(|name| request.headers().contains_key(*name))
    {
        return Some(
            "a forwarded request cannot issue short-lived keys without a gateway key".to_owned(),
        );
    }
    if request.headers().contains_key(header::ORIGIN)
        || request
            .headers()
            .keys()
            .any(|name| name.as_str().starts_with("sec-fetch-"))
        || request
            .headers()
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|agent| agent.starts_with("Mozilla/"))
    {
        return Some(
            "a browser page cannot issue short-lived keys without a gateway key".to_owned(),
        );
    }
    let loopback_host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|host| {
            let name = host.host().trim_matches(['[', ']']);
            name.eq_ignore_ascii_case("localhost")
                || name
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if !loopback_host {
        return Some(
            "the Host must be a loopback address to issue short-lived keys without a gateway key"
                .to_owned(),
        );
    }
    None
}

fn parse_api_ttl(value: &Value) -> Result<Duration, ()> {
    match value {
        Value::String(text) => parse_ttl(text).map_err(|_| ()),
        Value::Number(number) => {
            let seconds = number.as_f64().filter(|seconds| {
                seconds.is_finite() && *seconds > 0.0 && *seconds <= MAX_TTL.as_secs_f64()
            });
            seconds.map(Duration::from_secs_f64).ok_or(())
        }
        _ => Err(()),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    json_response(status, list_error(message))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

pub(super) async fn refused_guard(
    State(app): State<Arc<Inner>>,
    request: Request,
    next: Next,
) -> Response {
    if request
        .extensions()
        .get::<hofund_secrets::Identity>()
        .is_none()
        && let Some(keys) = &app.keys
    {
        let query = request.uri().query().and_then(query_key);
        let mut refused = None;
        let mut enabled = false;
        for secret in [
            bearer(request.headers()),
            header_secret(request.headers(), "x-api-key"),
            header_secret(request.headers(), "x-goog-api-key"),
        ]
        .into_iter()
        .flatten()
        .chain(query.as_deref())
        {
            match keys.check(secret) {
                Ok(hofund_secrets::KeyAuthentication::Enabled(_)) => {
                    enabled = true;
                    break;
                }
                Ok(hofund_secrets::KeyAuthentication::Refused(message)) => {
                    refused.get_or_insert(message);
                }
                Ok(hofund_secrets::KeyAuthentication::Unknown) | Err(_) => {}
            }
        }
        if !enabled && let Some(message) = refused {
            return error(StatusCode::UNAUTHORIZED, &message);
        }
    }
    next.run(request).await
}

pub(super) fn prune_task(keys: Arc<KeyStore>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(PRUNE_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let keys = Arc::clone(&keys);
            let _ = tokio::task::spawn_blocking(move || keys.prune()).await;
            ticks.tick().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::IpAddr;

    use axum::body::Body;
    use hofund_router::{FailoverPolicy, Router, Snapshot};
    use hofund_secrets::{IssuedKey, KeyLimit};
    use hofund_usage::Ledger;
    use tower::ServiceExt;

    use super::*;
    use crate::Gateway;

    struct Fixture {
        gateway: Gateway,
        store: Arc<KeyStore>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
        store.list().expect("migrate");
        let token = store.token_file().read().expect("token");
        let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
        let gateway = Gateway::new(
            Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
            token,
            std::collections::HashMap::new(),
            usage,
        )
        .with_key_store(Arc::clone(&store));
        Fixture {
            gateway,
            store,
            _dir: dir,
        }
    }

    async fn request(
        app: &axum::Router,
        method: &str,
        path: &str,
        peer: &str,
        secret: Option<&str>,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:3425");
        if let Some(secret) = secret {
            builder = builder.header("authorization", format!("Bearer {secret}"));
        }
        let mut request = builder.body(Body::from(body.to_owned())).expect("request");
        for (name, value) in headers {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                axum::http::HeaderValue::from_str(value).expect("header value"),
            );
        }
        let ip: IpAddr = peer.parse().expect("peer ip");
        request
            .extensions_mut()
            .insert(PeerAddr(SocketAddr::new(ip, 5000)));
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
            .await
            .expect("body")
            .to_vec();
        (status, headers, body)
    }

    fn issued(body: &[u8]) -> IssuedKey {
        serde_json::from_slice(body).expect("issued key")
    }

    #[tokio::test]
    async fn s2_loopback_and_permitted_keys_may_issue_but_browser_profiles_may_not() {
        let fixture = fixture();
        let app = fixture.gateway.into_router();
        for (name, peer, headers) in [
            ("remote", "192.168.1.9", vec![]),
            (
                "forwarded",
                "127.0.0.1",
                vec![("x-forwarded-for", "192.0.2.1")],
            ),
            (
                "origin",
                "127.0.0.1",
                vec![("origin", "https://example.invalid")],
            ),
            ("fetch", "127.0.0.1", vec![("sec-fetch-site", "cross-site")]),
            ("agent", "127.0.0.1", vec![("user-agent", "Mozilla/5.0")]),
            ("host", "127.0.0.1", vec![("host", "example.invalid")]),
        ] {
            let (status, _, _) = request(
                &app,
                "POST",
                KEYS_PATH,
                peer,
                None,
                r#"{"ttl":"1h"}"#,
                &headers,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}");
        }
        let (status, headers, body) = request(
            &app,
            "POST",
            KEYS_PATH,
            "127.0.0.1",
            None,
            r#"{"ttl":60}"#,
            &[("user-agent", "sandbox-agent")],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers["cache-control"], "no-store");
        assert!(!issued(&body).key.is_empty());

        let parent = fixture.store.add("Parent", None).expect("parent");
        let (status, _, _) = request(
            &app,
            "POST",
            KEYS_PATH,
            "192.168.1.9",
            Some(&parent.secret),
            r#"{"ttl":"1h"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        fixture
            .store
            .set_issuer(&parent.key.id, true)
            .expect("issuer");
        let (status, _, body) = request(
            &app,
            "POST",
            KEYS_PATH,
            "192.168.1.9",
            Some(&parent.secret),
            r#"{"ttl":"1h"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let child = issued(&body);
        let (status, _, _) = request(
            &app,
            "POST",
            KEYS_PATH,
            "192.168.1.9",
            Some(&child.key),
            r#"{"ttl":"1h"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        fixture
            .store
            .set_read_only(&parent.key.id, true)
            .expect("read only");
        let (status, _, _) = request(
            &app,
            "POST",
            KEYS_PATH,
            "192.168.1.9",
            Some(&parent.secret),
            r#"{"ttl":"1h"}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn s4_key_routes_return_secret_once_list_masked_and_revoke_only_own_keys() {
        let fixture = fixture();
        let parent = fixture.store.add("Parent", None).expect("parent");
        fixture
            .store
            .set_issuer(&parent.key.id, true)
            .expect("issuer");
        let app = fixture.gateway.into_router();
        for body in ["{}", r#"{"ttl":"1s"}"#, r#"{"ttl":-5}"#, "not json"] {
            let (status, _, _) = request(
                &app,
                "POST",
                KEYS_PATH,
                "192.168.1.9",
                Some(&parent.secret),
                body,
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, _, body) = request(
            &app,
            "POST",
            KEYS_PATH,
            "192.168.1.9",
            Some(&parent.secret),
            r#"{"ttl":600,"name":"Sandbox","readOnly":true}"#,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let child = issued(&body);
        assert_eq!(child.issuer, parent.key.id);
        assert!(child.read_only);
        let (status, _, body) = request(
            &app,
            "GET",
            KEYS_PATH,
            "192.168.1.9",
            Some(&parent.secret),
            "",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let list: Value = serde_json::from_slice(&body).expect("list");
        assert_eq!(list["keys"][0]["id"], child.id);
        assert!(list["keys"][0]["key"] != Value::String(child.key.clone()));
        assert!(
            list["keys"][0]["key"]
                .as_str()
                .is_some_and(|key| key.contains('…'))
        );
        assert_eq!(list["keys"][0]["readOnly"], true);
        let (status, _, body) = request(&app, "GET", KEYS_PATH, "127.0.0.1", None, "", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let list: Value = serde_json::from_slice(&body).expect("local list");
        assert_eq!(list["keys"].as_array().map(Vec::len), Some(0));
        let path = format!("{KEYS_PATH}/{}", child.id);
        let (status, _, _) = request(&app, "DELETE", &path, "127.0.0.1", None, "", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = request(
            &app,
            "DELETE",
            &path,
            "192.168.1.9",
            Some(&parent.secret),
            "",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            fixture
                .store
                .authenticate(&child.key)
                .expect("revoked")
                .is_none()
        );
    }

    #[tokio::test]
    async fn s1_expired_key_is_refused_on_open_and_authenticated_routes() {
        let fixture = fixture();
        let issued = fixture
            .store
            .issue(IssueRequest {
                name: "Expired sandbox".to_owned(),
                ttl: Duration::from_secs(60),
                ..IssueRequest::default()
            })
            .expect("issue");
        let mut keys = fixture.store.export().expect("export");
        keys.iter_mut()
            .find(|key| key.id == issued.id)
            .expect("issued")
            .expires = Some(unix_now() - 1);
        fixture.store.restore(&keys).expect("restore");
        let app = fixture.gateway.into_router();
        for (method, path) in [
            ("GET", "/health"),
            ("GET", "/v1/models"),
            ("GET", "/v1/hofund/limit"),
            ("POST", KEYS_PATH),
        ] {
            let (status, _, body) = request(
                &app,
                method,
                path,
                "127.0.0.1",
                Some(&issued.key),
                r#"{"ttl":"1h"}"#,
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
            let error: Value = serde_json::from_slice(&body).expect("error");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("expired at")
                        && message.contains("ask for a new one"))
            );
        }
        let (status, _, _) = request(&app, "GET", "/health", "127.0.0.1", None, "", &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn s7_limit_route_reports_expiry_and_issuer_limits() {
        let fixture = fixture();
        let parent = fixture.store.add("Parent", None).expect("parent");
        fixture
            .store
            .set_issuer(&parent.key.id, true)
            .expect("issuer");
        fixture
            .store
            .set_limit(
                &parent.key.id,
                Some(KeyLimit {
                    period: "day".to_owned(),
                    tokens: 100,
                    ..KeyLimit::default()
                }),
                None,
                unix_now(),
            )
            .expect("limit");
        let child = fixture
            .store
            .issue(IssueRequest {
                issuer: parent.key.id,
                ttl: Duration::from_secs(3_600),
                ..IssueRequest::default()
            })
            .expect("child");
        let app = fixture.gateway.into_router();
        let (status, _, body) = request(
            &app,
            "GET",
            "/v1/hofund/limit",
            "127.0.0.1",
            Some(&child.key),
            "",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let limit: Value = serde_json::from_slice(&body).expect("limit");
        assert_eq!(limit["expires"], child.expires);
        assert_eq!(limit["issuerLimits"][0]["tokenLimit"], 100);
    }
}
