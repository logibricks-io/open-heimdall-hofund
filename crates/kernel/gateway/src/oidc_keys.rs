//! POST `/v1/hofund/keys/oidc`: trade an OIDC token for a short-lived gateway key.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use hofund_secrets::{IssueRequest, MAX_TTL};
use serde::Deserialize;
use serde_json::Value;

use super::issue_keys;
use super::oidc::{match_token, oidc_key_name};
use super::{Inner, bearer, json_response, list_error, readonly};

pub(super) const OIDC_KEYS_PATH: &str = "/v1/hofund/keys/oidc";
const BODY_LIMIT: usize = 1 << 20;

pub(super) fn routes(router: readonly::RouteSet) -> readonly::RouteSet {
    router.route(&["POST"], OIDC_KEYS_PATH, post(oidc_issue_key))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExchangeBody {
    ttl: Option<Value>,
    models: Vec<String>,
}

async fn oidc_issue_key(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let token = bearer(request.headers())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let Some(token) = token else {
        return error(
            StatusCode::UNAUTHORIZED,
            "send the OIDC token as Authorization: Bearer <token>",
        );
    };
    let Some(keys) = &app.keys else {
        return error(
            StatusCode::FORBIDDEN,
            "short-lived keys require the named gateway-key store",
        );
    };
    let Some(rules) = &app.oidc_rules else {
        return error(
            StatusCode::UNAUTHORIZED,
            "no rule takes tokens from this issuer (hofund gateway-key oidc add)",
        );
    };
    let (rule, claims) = match match_token(&token, rules).await {
        Ok(matched) => matched,
        Err(why) => return error(StatusCode::UNAUTHORIZED, &why),
    };
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let body: ExchangeBody = if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        ExchangeBody::default()
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "the body is empty or a JSON object: {\"ttl\": \"30m\", \"models\": […]}",
                );
            }
        }
    };
    let mut ttl = rule.lifetime();
    if let Some(value) = body.ttl.as_ref() {
        let asked = match issue_keys::parse_api_ttl(value) {
            Ok(duration) => duration,
            Err(()) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "ttl is how long the key lives, such as \"1h\", \"30m\" or 3600 (seconds), from 1m to 720h",
                );
            }
        };
        ttl = ttl.min(asked);
    }
    if ttl > MAX_TTL {
        ttl = MAX_TTL;
    }
    let issued = keys.issue(IssueRequest {
        name: oidc_key_name(&claims),
        ttl,
        models: body.models,
        issuer: rule.key,
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
        Err(failure) => {
            let message = failure.to_string();
            error(StatusCode::BAD_REQUEST, &message)
        }
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    json_response(status, list_error(message))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use hofund_router::{FailoverPolicy, Router, Snapshot};
    use hofund_secrets::oidc::{RuleDraft, RuleStore};
    use hofund_usage::Ledger;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::oidc::test_issuer::TestIssuer;
    use crate::oidc::{reset_oidc_verifier_for_tests, set_fetch_client_for_tests};
    use crate::{Gateway, issue_keys::PeerAddr};

    struct Fixture {
        _guard: tokio::sync::MutexGuard<'static, ()>,
        gateway: Gateway,
        store: Arc<hofund_secrets::KeyStore>,
        rules: RuleStore,
        issuer: Arc<TestIssuer>,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let guard = crate::oidc::lock_oidc_tests().await;
        reset_oidc_verifier_for_tests().await;
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(hofund_secrets::KeyStore::beside_token(
            dir.path().join("hofund.token"),
        ));
        store.list().expect("migrate");
        let rules = RuleStore::beside_token(dir.path().join("hofund.token"));
        let issuer = Arc::new(TestIssuer::new());
        set_fetch_client_for_tests(Arc::new({
            let issuer = Arc::clone(&issuer);
            move |url| issuer.fetch(url)
        }));
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
            _guard: guard,
            gateway,
            store,
            rules,
            issuer,
            _dir: dir,
        }
    }

    async fn post_oidc(
        app: &axum::Router,
        peer: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri(OIDC_KEYS_PATH)
            .header("host", "127.0.0.1:3425");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut request = builder.body(Body::from(body.to_owned())).expect("request");
        let ip: IpAddr = peer.parse().expect("peer");
        request
            .extensions_mut()
            .insert(PeerAddr(std::net::SocketAddr::new(ip, 5000)));
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
            .await
            .expect("body")
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn d28_exchange_issues_key_from_remote_without_gateway_key() {
        let fixture = fixture().await;
        let parent = fixture.store.add("CI", None).expect("parent");
        fixture
            .store
            .set_issuer(&parent.key.id, true)
            .expect("issuer");
        fixture
            .store
            .set_models(&parent.key.id, &["plan/m1".to_owned()])
            .expect("models");
        let rule = fixture
            .rules
            .add(
                RuleDraft {
                    key: parent.key.id.clone(),
                    issuer: fixture.issuer.url.clone(),
                    claims: BTreeMap::from([
                        ("repository".to_owned(), "acme/app".to_owned()),
                        ("ref".to_owned(), "refs/heads/main".to_owned()),
                    ]),
                    ..RuleDraft::default()
                },
                &fixture.store,
            )
            .expect("rule");
        let app = fixture.gateway.into_router();
        let token = |aud: &str, over: BTreeMap<String, Value>| {
            fixture.issuer.sign("k1", fixture.issuer.claims(aud, &over))
        };
        let (status, body) = post_oidc(
            &app,
            "192.168.1.9",
            Some(&token("hofund-x", BTreeMap::new())),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("no rule"));
        let (status, body) = post_oidc(
            &app,
            "192.168.1.9",
            Some(&token(&rule.audience, BTreeMap::new())),
            r#"{"ttl":"48h"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let issued: hofund_secrets::IssuedKey = serde_json::from_slice(&body).expect("issued");
        assert_eq!(issued.issuer, parent.key.id);
        assert_eq!(issued.name, "acme/app@main (run 42)");
        let (status, _) = post_oidc(
            &app,
            "192.168.1.9",
            Some(&token(
                &rule.audience,
                BTreeMap::from([(
                    "ref".to_owned(),
                    Value::String("refs/heads/fork".to_owned()),
                )]),
            )),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = post_oidc(&app, "192.168.1.9", None, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = post_oidc(
            &app,
            "192.168.1.9",
            Some(&token(&rule.audience, BTreeMap::new())),
            r#"{"models":["other/m2"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
