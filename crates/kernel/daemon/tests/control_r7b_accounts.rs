//! G-6 subscription and account routes with a scripted in-process registry.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, SignInSessionState, SubscriptionFlow, decode_error_envelope,
};
use hofund_daemon::{ControlRuntime, DaemonRegistry, Running, StartOptions, start_linked};
use hofund_plugin_api::{
    AccountId, AccountStatus, AgentDescription, AgentSignIn, BinaryName, BuildError, ConfigError,
    DeviceFlow, DiscoveredAgentAccount, DynUpstreamFactory, HostContext, LoopbackSpec, ModelList,
    PluginError, PluginId, ProtocolId, RawTable, Schema, SecretString, SignInChallenge, SignInCtx,
    SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use serde_json::{Value, json};

const SECRET: &str = "r7b-secret-sentinel-never-return";
const CODE: &str = "r7b-callback-code-never-return";
const PLUGIN_IDS: [&str; 4] = ["fake-pkce", "fake-device", "fake-token", "fake-bridge"];

static PKCE_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fake-pkce"),
    "Fake PKCE",
    SignInFlowKind::Pkce,
)
.with_agent_sign_in(AgentSignIn::new(
    &["fake-agent"],
    "R7B_UNUSED_HOME",
    "/workspace/hofund-r7b-target-a/r7b-fake-agent-home",
));
static DEVICE_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fake-device"),
    "Fake Device",
    SignInFlowKind::Device,
);
static TOKEN_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fake-token"),
    "Fake Token",
    SignInFlowKind::TokenFile,
);
static BRIDGE_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fake-bridge"),
    "Fake Bridge",
    SignInFlowKind::Bridge,
);

#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Ready,
    Hang,
}

#[derive(Default)]
struct Calls {
    mode: Mutex<Mode>,
    redirect: Mutex<Option<String>>,
    cancels: AtomicUsize,
    proofs: Mutex<Vec<&'static str>>,
    refreshes: Mutex<Vec<String>>,
    removals: Mutex<Vec<String>>,
    writes: Mutex<Vec<String>>,
}

struct FakeFactory {
    id: &'static str,
    calls: Arc<Calls>,
}

struct EmptyConfig;
impl ValidConfig for EmptyConfig {}

impl SubscriptionFactory for FakeFactory {
    type Config = EmptyConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        match self.id {
            "fake-pkce" => &PKCE_SPEC,
            "fake-device" => &DEVICE_SPEC,
            "fake-token" => &TOKEN_SPEC,
            _ => &BRIDGE_SPEC,
        }
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
        Ok(EmptyConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let flow = match self.id {
            "fake-pkce" => SignInFlow::Pkce,
            "fake-device" => SignInFlow::Device(DeviceFlow::new("example.test").expect("device")),
            "fake-token" => SignInFlow::TokenFile,
            _ => SignInFlow::Bridge {
                command: BinaryName::new("fake-agent"),
            },
        };
        Ok(Arc::new(FakeSubscription {
            id: PluginId::from_static(self.id),
            flow,
            calls: Arc::clone(&self.calls),
        }))
    }
}

struct FakeSubscription {
    id: PluginId,
    flow: SignInFlow,
    calls: Arc<Calls>,
}

impl FakeSubscription {
    fn mode(&self) -> Mode {
        *self.calls.mode.lock().expect("mode")
    }

    async fn store(
        &self,
        host: &HostContext,
        id: &str,
        label: &str,
    ) -> Result<AccountId, PluginError> {
        let id = AccountId::try_from(id).expect("account id");
        let account = host.accounts().account(id.clone(), label).expect("account");
        host.accounts()
            .upsert(account, SecretString::from(SECRET.to_owned()))
            .await?;
        Ok(id)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for FakeSubscription {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    async fn start_sign_in(
        &self,
        ctx: &SignInCtx,
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        match self.id.as_str() {
            "fake-pkce" => {
                let binding = host
                    .loopback()
                    .bind(ctx, &LoopbackSpec::ephemeral().expect_state("right"))?;
                *self.calls.redirect.lock().expect("redirect") =
                    Some(binding.redirect_uri().to_owned());
                Ok(SignInChallenge::OpenUrl {
                    url: "https://auth.example.test/authorize?state=right".to_owned(),
                })
            }
            "fake-device" => Ok(SignInChallenge::DeviceCode {
                user_code: "FAKE-CODE".to_owned(),
                verification_url: "https://example.test/device".to_owned(),
            }),
            _ => Ok(SignInChallenge::Instructions {
                text: "Paste fake credential text.".to_owned(),
            }),
        }
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        if matches!(self.mode(), Mode::Hang) {
            std::future::pending::<()>().await;
        }
        let seen = match proof {
            SignInProof::Callback { .. } => "callback",
            SignInProof::Token(_) => "token",
            SignInProof::Await => "await",
            _ => "other",
        };
        self.calls.proofs.lock().expect("proofs").push(seen);
        let account = format!("{}-account", self.id.as_str());
        self.store(
            host,
            &account,
            ctx.option("label").unwrap_or("Fake Account"),
        )
        .await
    }

    async fn cancel_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        self.calls.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn account_status(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, _host: &HostContext) -> Result<(), PluginError> {
        self.calls
            .refreshes
            .lock()
            .expect("refreshes")
            .push(account.to_string());
        Ok(())
    }

    async fn quota(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<hofund_plugin_api::QuotaWindow>, PluginError> {
        Ok(Vec::new())
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        Ok(ModelList::new(Vec::new(), None))
    }

    fn upstream(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Err(PluginError::unsupported("fake upstream"))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        if self.id.as_str() != "fake-pkce" {
            return Err(PluginError::unsupported("not this plugin's export"));
        }
        let Some(id) = text.strip_prefix("export:") else {
            return Err(PluginError::unsupported("not a fake export"));
        };
        let id = id.trim();
        if id.is_empty() {
            return Err(PluginError::unsupported("not a fake export"));
        }
        Ok(vec![self.store(host, id, "Imported Account").await?])
    }

    async fn discover_agent_sign_in(
        &self,
        _home: &std::path::Path,
    ) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
        if self.id.as_str() != "fake-pkce" {
            return Ok(Vec::new());
        }
        Ok(vec![DiscoveredAgentAccount::new(
            AccountId::from_static("discovered"),
            "Discovered Account",
            "fake-reference",
        )?])
    }

    async fn import_agent_sign_in(
        &self,
        _home: &std::path::Path,
        reference: &str,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        assert_eq!(reference, "fake-reference");
        self.store(host, "discovered", "Discovered Account").await
    }

    async fn write_agent_sign_in(
        &self,
        account: &AccountId,
        home: &std::path::Path,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        std::fs::create_dir_all(home).expect("agent home");
        std::fs::write(home.join("account.txt"), account.as_str()).expect("agent account");
        self.calls
            .writes
            .lock()
            .expect("writes")
            .push(account.to_string());
        Ok(())
    }

    async fn remove_account(
        &self,
        account: &AccountId,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        self.calls
            .removals
            .lock()
            .expect("removals")
            .push(account.to_string());
        Ok(())
    }
}

struct FakeRegistry {
    calls: Arc<Calls>,
}

impl DaemonRegistry for FakeRegistry {
    fn factory(&self, _id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        None
    }

    fn is_kernel_factory(&self, _id: &str) -> bool {
        false
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        &[]
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        Vec::new()
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

    fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
        Vec::new()
    }

    fn subscription(&self, id: &str) -> Option<Arc<dyn hofund_plugin_api::DynSubscriptionFactory>> {
        PLUGIN_IDS.contains(&id).then(|| {
            Arc::new(FakeFactory {
                id: PLUGIN_IDS
                    .into_iter()
                    .find(|candidate| *candidate == id)
                    .expect("id"),
                calls: Arc::clone(&self.calls),
            }) as Arc<dyn hofund_plugin_api::DynSubscriptionFactory>
        })
    }

    fn subscription_ids(&self) -> Vec<&'static str> {
        PLUGIN_IDS.to_vec()
    }
}

struct Rig {
    calls: Arc<Calls>,
    running: Running,
    client: reqwest::Client,
    base: String,
    token: String,
    _config: tempfile::TempDir,
    _runtime: tempfile::TempDir,
}

impl Rig {
    async fn new(mode: Mode, deadline: Option<Duration>) -> Self {
        let config = tempfile::tempdir().expect("config dir");
        let runtime = tempfile::tempdir().expect("runtime dir");
        std::fs::create_dir_all(runtime.path().join("home")).expect("runtime home");
        let path = config.path().join("hofund.toml");
        let subscriptions = PLUGIN_IDS
            .map(|id| format!("[[subscriptions]]\nid = \"{id}\"\naccounts = \"*\"\n"))
            .join("\n");
        std::fs::write(&path, format!("listen = \"127.0.0.1:0\"\n{subscriptions}"))
            .expect("config");
        let calls = Arc::new(Calls::default());
        *calls.mode.lock().expect("mode") = mode;
        let running = start_linked(
            &path,
            Arc::new(FakeRegistry {
                calls: Arc::clone(&calls),
            }),
            StartOptions {
                control_listen: Some("127.0.0.1:0".parse().expect("control")),
                control_enabled: true,
                control_runtime: Some(ControlRuntime::new(None, runtime.path().to_path_buf())),
                skip_config_watch: true,
                hofund_addr: Some(String::new()),
                sign_in_deadline: deadline,
                ..StartOptions::default()
            },
        )
        .await
        .expect("daemon");
        let token = std::fs::read_to_string(config.path().join("hofund.token"))
            .expect("token")
            .trim()
            .to_owned();
        let base = format!("http://{}", running.control_local_addr().expect("control"));
        Self {
            calls,
            running,
            client: support::client(),
            base,
            token,
            _config: config,
            _runtime: runtime,
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("response");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn get(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn start(&self, id: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.post(
            &format!("/v1/hofund/subscriptions/{id}/sign-in"),
            json!({"version": 1}),
        )
        .await
    }

    async fn status(&self, id: &str, session: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.get(&format!("/v1/hofund/subscriptions/{id}/sign-in/{session}"))
            .await
    }

    async fn wait_state(&self, id: &str, session: &str, expected: SignInSessionState) -> Vec<u8> {
        for _ in 0..100 {
            let (status, body) = self.status(id, session).await;
            assert_eq!(status, reqwest::StatusCode::OK);
            if state(&body) == expected {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session did not reach {expected:?}");
    }

    async fn shutdown(self) {
        support::shutdown(self.running).await;
    }
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("JSON")
}

fn state(body: &[u8]) -> SignInSessionState {
    serde_json::from_slice::<hofund_control_proto::SignInStatusResponse>(body)
        .expect("status DTO")
        .state
}

fn error_kind(
    status: reqwest::StatusCode,
    body: &[u8],
    expected: reqwest::StatusCode,
) -> ControlErrorKind {
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(body));
    decode_error_envelope(API_VERSION, body)
        .expect("error")
        .error
        .kind
}

fn port(redirect: &str) -> u16 {
    url::Url::parse(redirect)
        .expect("redirect")
        .port()
        .expect("port")
}

async fn assert_port_released(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("loopback port {port} is still accepting");
}

#[tokio::test]
async fn subscriptions_and_account_operations_persist_in_the_running_daemon() {
    let rig = Rig::new(Mode::Ready, None).await;
    let export = std::env::temp_dir().join(format!("r7b-export-{}.json", std::process::id()));
    std::fs::write(&export, "export:one").expect("export");
    let (status, body) = rig
        .post(
            "/v1/hofund/accounts/import",
            json!({"version": 1, "path": export.to_string_lossy()}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(json(&body)["imported"], json!(["one"]));
    assert!(!String::from_utf8_lossy(&body).contains(SECRET));

    let (status, body) = rig.get("/v1/hofund/subscriptions").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains(SECRET));
    let listed = json(&body);
    let flows: Vec<SubscriptionFlow> = listed["subscriptions"]
        .as_array()
        .expect("subscriptions")
        .iter()
        .map(|row| serde_json::from_value(row["flow"].clone()).expect("flow"))
        .collect();
    assert_eq!(
        flows,
        [
            SubscriptionFlow::Bridge,
            SubscriptionFlow::Device,
            SubscriptionFlow::Pkce,
            SubscriptionFlow::TokenFile,
        ]
    );
    assert!(text.contains("\"id\":\"one\""));
    assert!(text.contains("\"ready\":true"));

    let (status, body) = rig
        .post(
            "/v1/hofund/subscriptions/fake-pkce/refresh",
            json!({"version": 1, "account": "one"}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(json(&body)["refreshed"], json!(["one"]));
    assert_eq!(
        rig.calls.refreshes.lock().expect("refreshes").as_slice(),
        ["one"]
    );

    let (status, body) = rig.get("/v1/hofund/accounts/discover").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(json(&body)["found"][0]["already_stored"], false);
    let (status, body) = rig
        .post(
            "/v1/hofund/accounts/discover",
            json!({"version": 1, "add": true}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(json(&body)["imported"], json!(["discovered"]));

    let (status, body) = rig
        .post(
            "/v1/hofund/accounts/switch",
            json!({"version": 1, "agent": "fake-agent", "target": "discovered"}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(json(&body)["account"], "discovered");
    assert_eq!(
        std::fs::read_to_string("/workspace/hofund-r7b-target-a/r7b-fake-agent-home/account.txt")
            .expect("agent write"),
        "discovered"
    );
    let (status, body) = rig
        .post(
            "/v1/hofund/accounts/switch",
            json!({"version": 1, "agent": "unsupported", "target": "next"}),
        )
        .await;
    assert_eq!(
        error_kind(status, &body, reqwest::StatusCode::UNPROCESSABLE_ENTITY),
        ControlErrorKind::Unprocessable
    );

    let (status, body) = rig
        .post(
            "/v1/hofund/subscriptions/fake-pkce/accounts/one/remove",
            json!({"version": 1}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(json(&body)["removed"], true);
    assert_eq!(
        rig.calls.removals.lock().expect("removals").as_slice(),
        ["one"]
    );
    let (_, body) = rig.get("/v1/hofund/subscriptions").await;
    assert!(!String::from_utf8_lossy(&body).contains("\"id\":\"one\""));
    assert!(String::from_utf8_lossy(&body).contains("\"id\":\"discovered\""));
    let _ = std::fs::remove_file(export);
    rig.shutdown().await;
}

#[tokio::test]
async fn sign_in_start_poll_proof_cancel_and_loopback_cleanup_work_without_blocking() {
    let rig = Rig::new(Mode::Hang, None).await;
    let (status, body) = rig.start("missing").await;
    assert_eq!(
        error_kind(status, &body, reqwest::StatusCode::NOT_FOUND),
        ControlErrorKind::NotFound
    );

    let started = Instant::now();
    let (status, body) = rig.start("fake-pkce").await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "start waited for the whole flow"
    );
    let session = json(&body)["session"].as_str().expect("session").to_owned();
    assert_eq!(
        state(&rig.status("fake-pkce", &session).await.1),
        SignInSessionState::NeedsProof
    );
    let (status, body) = rig.start("fake-pkce").await;
    assert_eq!(
        error_kind(status, &body, reqwest::StatusCode::CONFLICT),
        ControlErrorKind::Conflict
    );
    let (status, body) = rig
        .post(
            &format!("/v1/hofund/subscriptions/fake-pkce/sign-in/{session}/proof"),
            json!({"version": 1, "token": "wrong-kind"}),
        )
        .await;
    assert_eq!(
        error_kind(status, &body, reqwest::StatusCode::UNPROCESSABLE_ENTITY),
        ControlErrorKind::Unprocessable
    );
    let redirect = rig
        .calls
        .redirect
        .lock()
        .expect("redirect")
        .clone()
        .expect("redirect");
    let (status, body) = rig
        .post(
            &format!("/v1/hofund/subscriptions/fake-pkce/sign-in/{session}/cancel"),
            json!({"version": 1}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(json(&body)["state"], "cancelled");
    assert_port_released(port(&redirect)).await;
    assert!(rig.calls.cancels.load(Ordering::SeqCst) >= 1);

    *rig.calls.mode.lock().expect("mode") = Mode::Ready;
    let (status, body) = rig.start("fake-pkce").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let session = json(&body)["session"].as_str().expect("session").to_owned();
    let redirect = rig
        .calls
        .redirect
        .lock()
        .expect("redirect")
        .clone()
        .expect("redirect");
    let callback = format!("{redirect}?code={CODE}&state=right");
    let (status, body) = rig
        .post(
            &format!("/v1/hofund/subscriptions/fake-pkce/sign-in/{session}/proof"),
            json!({"version": 1, "url": callback}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!String::from_utf8_lossy(&body).contains(CODE));
    rig.wait_state("fake-pkce", &session, SignInSessionState::Ready)
        .await;
    let (status, body) = rig
        .post(
            &format!("/v1/hofund/subscriptions/fake-pkce/sign-in/{session}/proof"),
            json!({"version": 1, "url": "http://localhost/callback"}),
        )
        .await;
    assert_eq!(
        error_kind(status, &body, reqwest::StatusCode::CONFLICT),
        ControlErrorKind::Conflict
    );
    let (_, body) = rig.get("/v1/hofund/subscriptions").await;
    assert!(String::from_utf8_lossy(&body).contains("fake-pkce-account"));

    let (status, body) = rig.start("fake-token").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let token_session = json(&body)["session"].as_str().expect("session").to_owned();
    let (status, body) = rig
        .post(
            &format!("/v1/hofund/subscriptions/fake-token/sign-in/{token_session}/proof"),
            json!({"version": 1, "token": "/tmp/not-an-account-export"}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    rig.wait_state("fake-token", &token_session, SignInSessionState::Ready)
        .await;
    assert_eq!(
        rig.calls.proofs.lock().expect("proofs").as_slice(),
        ["callback", "token"]
    );

    let (status, body) = rig.start("fake-device").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let device_session = json(&body)["session"].as_str().expect("session").to_owned();
    rig.wait_state("fake-device", &device_session, SignInSessionState::Ready)
        .await;
    rig.shutdown().await;
}

#[tokio::test]
async fn sign_in_session_expires_as_failed() {
    let rig = Rig::new(Mode::Hang, Some(Duration::from_millis(50))).await;
    let (status, body) = rig.start("fake-pkce").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let session = json(&body)["session"].as_str().expect("session").to_owned();
    rig.wait_state("fake-pkce", &session, SignInSessionState::Failed)
        .await;
    let redirect = rig
        .calls
        .redirect
        .lock()
        .expect("redirect")
        .clone()
        .expect("redirect");
    assert_port_released(port(&redirect)).await;
    let (status, _) = rig.status("fake-pkce", "missing").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    rig.shutdown().await;
}

#[test]
fn production_sign_in_deadline_is_ten_minutes() {
    assert_eq!(hofund_daemon::SIGN_IN_DEADLINE, Duration::from_secs(600));
}
