//! Socket-peer admission and local-proxy classification for the model listener.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use hofund_config::{IpNetwork, NetworkSettings};

use crate::issue_keys::PeerAddr;
use crate::{Inner, json_response, list_error};

pub(crate) const FORWARD_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "tailscale-user-login",
];

/// Shared network-access settings. Replacing the snapshot affects the next request.
#[derive(Clone, Debug)]
pub struct NetworkAccess {
    inner: Arc<RwLock<AccessState>>,
}

#[derive(Clone, Debug)]
struct AccessState {
    settings: NetworkSettings,
    networks: Vec<IpNetwork>,
    invalid: bool,
}

impl Default for NetworkAccess {
    fn default() -> Self {
        Self::new(NetworkSettings::default())
    }
}

impl NetworkAccess {
    /// Validates and shares the effective settings.
    #[must_use]
    pub fn new(settings: NetworkSettings) -> Self {
        Self {
            inner: Arc::new(RwLock::new(compile(settings))),
        }
    }

    /// Replaces the effective settings without rebuilding the HTTP router.
    pub fn replace(&self, settings: NetworkSettings) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = compile(settings);
    }

    /// The effective entries and proxy flag for status reporting.
    #[must_use]
    pub fn settings(&self) -> NetworkSettings {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .settings
            .clone()
    }

    pub(crate) fn allows(&self, peer: Option<SocketAddr>) -> bool {
        let state = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.settings.allow_ips.is_empty() && !state.invalid {
            return true;
        }
        let Some(peer) = peer else {
            return false;
        };
        let address = match peer.ip() {
            std::net::IpAddr::V6(address) => address
                .to_ipv4_mapped()
                .map_or(std::net::IpAddr::V6(address), std::net::IpAddr::V4),
            address => address,
        };
        address.is_loopback()
            || !state.invalid
                && state
                    .networks
                    .iter()
                    .any(|network| network.contains(address))
    }

    pub(crate) fn proxied(&self, headers: &HeaderMap) -> bool {
        !self.settings().trust_proxy
            && FORWARD_HEADERS
                .iter()
                .any(|name| headers.contains_key(*name))
    }

    pub(crate) fn is_local(&self, request: &Request) -> bool {
        let peer = request.extensions().get::<PeerAddr>().map(|peer| peer.0);
        peer.is_some_and(|peer| peer.ip().is_loopback()) && !self.proxied(request.headers())
    }
}

fn compile(settings: NetworkSettings) -> AccessState {
    let mut networks = Vec::with_capacity(settings.allow_ips.len());
    let mut invalid = settings.invalid_allow_ips;
    for entry in &settings.allow_ips {
        match IpNetwork::parse(entry) {
            Ok(network) => networks.push(network),
            Err(error) => {
                invalid = true;
                tracing::error!(entry = %entry, %error, "invalid allow-list entry; only loopback peers are allowed");
            }
        }
    }
    if settings.invalid_allow_ips {
        tracing::error!(entries = ?settings.allow_ips, "invalid HOFUND_ALLOW_IPS; only loopback peers are allowed");
    }
    AccessState {
        settings,
        networks,
        invalid,
    }
}

pub(crate) async fn guard(State(app): State<Arc<Inner>>, request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<PeerAddr>().map(|peer| peer.0);
    if app.network_access.allows(peer) {
        return next.run(request).await;
    }
    let address = peer.map_or_else(|| "unknown".to_owned(), |peer| peer.ip().to_string());
    tracing::warn!(%address, method = %request.method(), path = %request.uri().path(), "refused a peer outside the allow list");
    let message = format!(
        "the socket peer address {address} is not on the gateway allow list; add it to allow_ips in hofund.toml or set HOFUND_ALLOW_IPS"
    );
    json_response(StatusCode::FORBIDDEN, list_error(&message))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hofund_codec::{Codec, CodecError, StreamDecoder, StreamEncoder};
    use hofund_config::{NetworkSettings, effective_network, load};
    use hofund_ir::{
        Message, Request as ModelRequest, Response as ModelResponse, Role, StreamEvent,
    };
    use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
    use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
    use hofund_usage::Ledger;
    use tower::ServiceExt;

    use super::NetworkAccess;
    use crate::Gateway;
    use crate::issue_keys::PeerAddr;

    struct Plain;

    impl Codec for Plain {
        fn id(&self) -> &'static str {
            "plain"
        }

        fn decode_request(&self, bytes: &[u8]) -> Result<ModelRequest, CodecError> {
            let value: serde_json::Value = serde_json::from_slice(bytes)?;
            let model = value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if model.is_empty() {
                return Err(CodecError::MissingField("model"));
            }
            Ok(ModelRequest::new(model))
        }

        fn encode_request(&self, request: &ModelRequest) -> Result<Vec<u8>, CodecError> {
            serde_json::to_vec(&serde_json::json!({"model": request.model}))
                .map_err(CodecError::from)
        }

        fn decode_response(&self, _bytes: &[u8]) -> Result<ModelResponse, CodecError> {
            Err(CodecError::Unsupported {
                message: "unused".to_owned(),
            })
        }

        fn encode_response(&self, response: &ModelResponse) -> Result<Vec<u8>, CodecError> {
            serde_json::to_vec(&serde_json::json!({"response": response.message.content.len()}))
                .map_err(CodecError::from)
        }

        fn encode_stream(&self, _events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
            Ok(Vec::new())
        }

        fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
            Box::new(NoDecode)
        }

        fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
            Box::new(NoEncode)
        }
    }

    struct NoDecode;

    impl StreamDecoder for NoDecode {
        fn push(&mut self, _chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
            Ok(Vec::new())
        }

        fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
            Ok(Vec::new())
        }
    }

    struct NoEncode;

    impl StreamEncoder for NoEncode {
        fn push(&mut self, _event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
            Ok(Vec::new())
        }
    }

    struct Fake {
        id: MemberId,
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Member for Fake {
        fn id(&self) -> &MemberId {
            &self.id
        }

        fn route_id(&self) -> &str {
            "primary"
        }

        fn protocol(&self) -> &str {
            "plain"
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

    struct Fixture {
        app: axum::Router,
        access: NetworkAccess,
        hits: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    fn gateway_fixture(settings: NetworkSettings) -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage ledger"));
        let hits = Arc::new(AtomicUsize::new(0));
        let member: Arc<dyn Member> = Arc::new(Fake {
            id: MemberId::from_preset(
                PresetId::try_from("primary").expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::clone(&hits),
        });
        let snapshot = Snapshot::new(Router::new(
            BTreeMap::from([("demo".to_owned(), vec![member])]),
            FailoverPolicy::default(),
        ));
        let codecs = HashMap::from([("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>)]);
        let access = NetworkAccess::new(settings);
        let app = Gateway::new(snapshot, "secret".to_owned(), codecs, usage)
            .with_network_access(access.clone())
            .into_router();
        Fixture {
            app,
            access,
            hits,
            _dir: dir,
        }
    }

    fn settings(entries: &[&str]) -> NetworkSettings {
        NetworkSettings {
            allow_ips: entries.iter().map(|entry| (*entry).to_owned()).collect(),
            trust_proxy: false,
            invalid_allow_ips: false,
        }
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        peer: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder
            .body(Body::from(if method == "POST" {
                r#"{"model":"demo"}"#
            } else {
                ""
            }))
            .expect("request");
        let ip: IpAddr = peer.parse().expect("peer IP");
        request
            .extensions_mut()
            .insert(PeerAddr(SocketAddr::new(ip, 5000)));
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn r22_allow_list_runs_before_auth_and_upstream_on_every_route() {
        let fixture = gateway_fixture(settings(&["192.168.1.0/24", "2001:db8::/32"]));
        let (status, body) = call(
            &fixture.app,
            "POST",
            "/v1/plain",
            "198.51.100.8",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("198.51.100.8"), "{body}");
        assert!(body.contains("allow list"), "{body}");
        assert!(body.contains("allow_ips"), "{body}");
        assert!(body.contains("HOFUND_ALLOW_IPS"), "{body}");
        assert!(body.contains("\"error\""), "{body}");
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);

        let (status, _) = call(&fixture.app, "GET", "/nowhere", "198.51.100.8", None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(&fixture.app, "GET", "/health", "198.51.100.8", None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = call(&fixture.app, "GET", "/v1/models", "192.168.1.9", None, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = call(
            &fixture.app,
            "POST",
            "/v1/plain",
            "192.168.1.9",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "2001:db8::9",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn r22_loopback_mapped_ipv4_and_an_empty_list_keep_their_rules() {
        let fixture = gateway_fixture(settings(&["10.0.0.0/8"]));
        for peer in ["127.0.0.1", "127.8.9.10", "::1", "::ffff:127.0.0.1"] {
            let (status, body) = call(&fixture.app, "GET", "/health", peer, None, &[]).await;
            assert_eq!(status, StatusCode::OK, "{peer}: {body}");
            let (status, _) = call(&fixture.app, "GET", "/v1/models", peer, None, &[]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{peer}");
        }
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "::ffff:10.1.2.3",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "::ffff:11.1.2.3",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let open = gateway_fixture(settings(&[]));
        let (status, _) = call(
            &open.app,
            "GET",
            "/v1/models",
            "203.0.113.9",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn r22_forwarding_headers_never_supply_the_allow_list_peer() {
        let fixture = gateway_fixture(settings(&["192.0.2.1"]));
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "198.51.100.8",
            Some("secret"),
            &[("x-forwarded-for", "192.0.2.1")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "192.0.2.1",
            Some("secret"),
            &[("x-forwarded-for", "198.51.100.8")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn r22_malformed_environment_list_lets_only_loopback_in() {
        let config = load("allow_ips = [\"192.0.2.1\"]\n").expect("config");
        let effective = effective_network(&config, Some("203.0.113.0/24 invalid"), None);
        let fixture = gateway_fixture(effective);
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "203.0.113.9",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "192.0.2.1",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(&fixture.app, "GET", "/health", "127.0.0.1", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn r22_replacing_network_settings_takes_effect_without_a_new_router() {
        let fixture = gateway_fixture(settings(&["192.0.2.1"]));
        let peer = "198.51.100.8";
        let (status, _) = call(&fixture.app, "GET", "/v1/models", peer, Some("secret"), &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        fixture.access.replace(settings(&["198.51.100.0/24"]));
        let (status, _) = call(&fixture.app, "GET", "/v1/models", peer, Some("secret"), &[]).await;
        assert_eq!(status, StatusCode::OK);
        fixture.access.replace(settings(&[]));
        let (status, _) = call(
            &fixture.app,
            "GET",
            "/v1/models",
            "203.0.113.9",
            Some("secret"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
