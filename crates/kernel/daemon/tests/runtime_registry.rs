//! End-to-end daemon startup and requests through an injected registry.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hofund_daemon::{DaemonRegistry, start_with_registry};
use hofund_plugin_api::{
    AgentDescription, BodyStream, BuildError, Completion, ConfigError, CredentialStyle,
    DynUpstreamFactory, HostContext, InboundSurface, ListKind, Method, PluginError, PluginId,
    ProtocolId, RawTable, RawValue, Request, RouteSpec, Schema, SurfaceHost, SurfaceId,
    UpstreamFactory, UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::{Call, HttpRequest, HttpResponse};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("openai-compat"),
    &[ProtocolId::OPENAI_CHAT],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

struct FakeConfig {
    base_url: String,
}

impl ValidConfig for FakeConfig {}

struct FakeFactory;

impl UpstreamFactory for FakeFactory {
    type Config = FakeConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let Some(RawValue::String(base_url)) = raw.get("base_url") else {
            return Err(ConfigError::new("base_url must be a string"));
        };
        Ok(FakeConfig {
            base_url: base_url.clone(),
        })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(FakeUpstream {
            base_url: cfg.base_url.clone(),
            host: host.clone(),
        }))
    }
}

struct FakeUpstream {
    base_url: String,
    host: HostContext,
}

#[async_trait]
impl UpstreamProvider for FakeUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let reply = Call::post(format!("{}/chat/completions", self.base_url))
            .json(b"{}")
            .send(self.host.http())
            .await?;
        Ok(Completion::Raw(BodyStream::from_bytes(reply.into_body())))
    }
}

static RELAY: SurfaceId = SurfaceId::from_static("relay");
static RELAY_ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Post, "/relay")];

/// Sends the inbound body to `<target>/relayed` on the host's pooled client.
struct Relay {
    target: String,
}

#[async_trait]
impl InboundSurface for Relay {
    fn id(&self) -> &SurfaceId {
        &RELAY
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &RELAY_ROUTES
    }

    async fn handle(
        &self,
        _route: &RouteSpec,
        request: HttpRequest,
        host: &SurfaceHost,
    ) -> Result<HttpResponse, PluginError> {
        let http = host
            .http()
            .ok_or_else(|| PluginError::unsupported("no pooled client"))?;
        let mut outbound = HttpRequest::new(request.into_body());
        *outbound.method_mut() = "POST".parse().expect("method");
        *outbound.uri_mut() = format!("{}/relayed", self.target).parse().expect("uri");
        http.send(outbound).await
    }
}

struct FakeRegistry {
    consultations: Arc<AtomicUsize>,
    surfaces: Vec<Arc<dyn InboundSurface>>,
}

impl DaemonRegistry for FakeRegistry {
    fn factory(&self, id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        self.consultations.fetch_add(1, Ordering::SeqCst);
        (id == "openai-compat").then(|| Arc::new(FakeFactory) as Arc<dyn DynUpstreamFactory>)
    }

    fn is_kernel_factory(&self, id: &str) -> bool {
        id == "openai-compat"
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        &["openai-compat"]
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        vec![ProtocolId::OPENAI_CHAT]
    }

    fn find_preset(
        &self,
        _id: &str,
    ) -> Option<(&'static hofund_daemon::CompiledPreset, Option<&'static str>)> {
        None
    }

    fn agents(&self) -> &'static [&'static AgentDescription] {
        &[]
    }

    fn surfaces(&self) -> Vec<Arc<dyn InboundSurface>> {
        self.surfaces.clone()
    }
}

#[tokio::test]
async fn daemon_runs_on_a_fake_registry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("directory");
    let config = dir.path().join("hofund.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"fake\"\nprotocol = \"openai-chat\"\nbase_url = \"{}\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"fake\"]\n",
            server.uri()
        ),
    )
    .expect("config");
    let consultations = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(FakeRegistry {
        consultations: Arc::clone(&consultations),
        surfaces: Vec::new(),
    });
    let running = start_with_registry(&config, registry).await.expect("start");
    let token = std::fs::read_to_string(dir.path().join("hofund.token")).expect("token");
    let response = support::client()
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({"model": "demo", "messages": []}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert!(response.text().await.expect("body").contains("ok"));
    assert!(consultations.load(Ordering::SeqCst) > 0);
    support::shutdown(running).await;
}

#[tokio::test]
async fn a_surface_relays_on_the_daemons_pooled_client() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/relayed"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("from upstream", "text/plain"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("directory");
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let registry = Arc::new(FakeRegistry {
        consultations: Arc::new(AtomicUsize::new(0)),
        surfaces: vec![Arc::new(Relay {
            target: server.uri(),
        }) as Arc<dyn InboundSurface>],
    });
    let running = start_with_registry(&config, registry).await.expect("start");
    let token = std::fs::read_to_string(dir.path().join("hofund.token")).expect("token");
    let response = support::client()
        .post(format!("http://{}/relay", running.local_addr))
        .bearer_auth(token.trim())
        .body("payload")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.expect("body"), "from upstream");
    support::shutdown(running).await;
}
