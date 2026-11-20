//! D-221: control sign-in, import, and discover-add surface an enrollment refusal.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hofund_control_proto::{API_VERSION, ControlErrorKind, decode_error_envelope};
use hofund_daemon::{
    ControlRuntime, DaemonRegistry, Running, StartOptions, set_test_agent_home, start_linked,
};
use hofund_plugin_api::{
    AccountEnrollment, AccountId, AccountStatus, AgentDescription, AgentSignIn, BuildError,
    ConfigError, DiscoveredAgentAccount, HostContext, ModelList, PluginError, PluginId, ProtocolId,
    RawTable, Schema, SecretString, SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind,
    SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec, UpstreamProvider,
    ValidConfig,
};
use serde_json::{Value, json};

const RISK: &str = "saved-account risk line";
const REFUSAL: &str = "new-account refusal line";
const OPEN: &str = "open-sub";
const WITHDRAWN: &str = "withdrawn-sub";

static OPEN_SPEC: SubscriptionSpec =
    SubscriptionSpec::from_static(PluginId::from_static(OPEN), "Open", SignInFlowKind::Pkce)
        .with_agent_sign_in(AgentSignIn::new(
            &["open-agent"],
            "D221_AGENT_HOME",
            "d221-open",
        ));
static WITHDRAWN_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static(WITHDRAWN),
    "Withdrawn",
    SignInFlowKind::Pkce,
)
.with_agent_sign_in(AgentSignIn::new(
    &["withdrawn-agent"],
    "D221_AGENT_HOME",
    "d221-agent",
));

struct Calls {
    starts: AtomicUsize,
    discover_open: AtomicBool,
}

struct Factory {
    id: &'static str,
    calls: Arc<Calls>,
}

struct Empty;
impl ValidConfig for Empty {}

impl SubscriptionFactory for Factory {
    type Config = Empty;

    fn spec(&self) -> &'static SubscriptionSpec {
        if self.id == WITHDRAWN {
            &WITHDRAWN_SPEC
        } else {
            &OPEN_SPEC
        }
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
        Ok(Empty)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Fake {
            id: PluginId::from_static(self.id),
            flow: SignInFlow::Pkce,
            calls: Arc::clone(&self.calls),
        }))
    }
}

struct Fake {
    id: PluginId,
    flow: SignInFlow,
    calls: Arc<Calls>,
}

impl Fake {
    fn withdrawn(&self) -> bool {
        self.id.as_str() == WITHDRAWN
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Fake {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    fn account_enrollment(&self) -> AccountEnrollment {
        if self.withdrawn() {
            AccountEnrollment::Withdrawn {
                risk: RISK,
                refusal: REFUSAL,
            }
        } else {
            AccountEnrollment::Open
        }
    }

    async fn start_sign_in(
        &self,
        _ctx: &SignInCtx,
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        if self.withdrawn() && host.accounts().list().await?.is_empty() {
            return Err(PluginError::config(REFUSAL));
        }
        Ok(SignInChallenge::OpenUrl {
            url: "https://auth.example.test/authorize".to_owned(),
        })
    }

    async fn complete_sign_in(
        &self,
        _ctx: &SignInCtx,
        _proof: SignInProof,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        Err(PluginError::unsupported("not completed in this test"))
    }

    async fn account_status(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, _account: &AccountId, _host: &HostContext) -> Result<(), PluginError> {
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
        Err(PluginError::unsupported("no upstream"))
    }

    async fn prepare_accounts(
        &self,
        _config_dir: &std::path::Path,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        if !self.withdrawn() {
            return Ok(());
        }
        let id = AccountId::from_static("saved-1");
        let account = host.accounts().account(id, "Saved")?;
        host.accounts()
            .upsert(account, SecretString::from("saved-secret".to_owned()))
            .await
    }

    async fn import_accounts(
        &self,
        text: &str,
        _host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        if let Some(refusal) = self.account_enrollment().refusal() {
            return Err(PluginError::config(refusal));
        }
        if text.contains("open-export") {
            return Ok(vec![AccountId::from_static("imported-open")]);
        }
        Err(PluginError::unsupported("not this export"))
    }

    async fn discover_agent_sign_in(
        &self,
        _home: &std::path::Path,
    ) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
        if !self.withdrawn() {
            if !self.calls.discover_open.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }
            return Ok(vec![DiscoveredAgentAccount::new(
                AccountId::from_static("found-open"),
                "Open found",
                "open-reference",
            )?]);
        }
        Ok(vec![DiscoveredAgentAccount::new(
            AccountId::from_static("found-1"),
            "Found",
            "found-reference",
        )?])
    }

    async fn import_agent_sign_in(
        &self,
        _home: &std::path::Path,
        _reference: &str,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        if let Some(refusal) = self.account_enrollment().refusal() {
            return Err(PluginError::config(refusal));
        }
        Ok(AccountId::from_static("imported-open"))
    }
}

struct Registry {
    calls: Arc<Calls>,
}

impl DaemonRegistry for Registry {
    fn factory(&self, _id: &str) -> Option<Arc<dyn hofund_plugin_api::DynUpstreamFactory>> {
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
        matches!(id, OPEN | WITHDRAWN).then(|| {
            Arc::new(Factory {
                id: if id == WITHDRAWN { WITHDRAWN } else { OPEN },
                calls: Arc::clone(&self.calls),
            }) as Arc<dyn hofund_plugin_api::DynSubscriptionFactory>
        })
    }
}

struct Rig {
    running: Running,
    client: reqwest::Client,
    base: String,
    token: String,
    calls: Arc<Calls>,
    _config: tempfile::TempDir,
    _runtime: tempfile::TempDir,
    _home: tempfile::TempDir,
}

impl Rig {
    async fn start() -> Self {
        let home = tempfile::tempdir().expect("agent home");
        set_test_agent_home("D221_AGENT_HOME", home.path().to_path_buf());
        let config = tempfile::tempdir().expect("config");
        let runtime = tempfile::tempdir().expect("runtime");
        std::fs::create_dir_all(runtime.path().join("home")).expect("runtime home");
        let path = config.path().join("hofund.toml");
        std::fs::write(
            &path,
            format!(
                "listen = \"127.0.0.1:0\"\n\
                 [[subscriptions]]\n\
                 id = \"{OPEN}\"\n\
                 accounts = \"*\"\n\
                 \n\
                 [[subscriptions]]\n\
                 id = \"{WITHDRAWN}\"\n\
                 accounts = \"*\"\n"
            ),
        )
        .expect("config");
        let calls = Arc::new(Calls {
            starts: AtomicUsize::new(0),
            discover_open: AtomicBool::new(false),
        });
        let running = start_linked(
            &path,
            Arc::new(Registry {
                calls: Arc::clone(&calls),
            }),
            StartOptions {
                control_listen: Some("127.0.0.1:0".parse().expect("control")),
                control_enabled: true,
                control_runtime: Some(ControlRuntime::new(None, runtime.path().to_path_buf())),
                skip_config_watch: true,
                hofund_addr: Some(String::new()),
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
            running,
            client: support::client(),
            base,
            token,
            calls,
            _config: config,
            _runtime: runtime,
            _home: home,
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
}

fn message(status: reqwest::StatusCode, body: &[u8]) -> String {
    assert_eq!(
        status,
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        String::from_utf8_lossy(body)
    );
    let envelope = decode_error_envelope(API_VERSION, body).expect("error");
    assert_eq!(envelope.error.kind, ControlErrorKind::Unprocessable);
    envelope.error.message
}

#[tokio::test]
async fn d221_control_signs_in_a_stored_account_and_lists_the_risk_line() {
    let rig = Rig::start().await;

    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            &format!("/v1/hofund/subscriptions/{WITHDRAWN}/sign-in"),
            Some(json!({"version": 1})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(rig.calls.starts.load(Ordering::SeqCst), 1);

    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            &format!("/v1/hofund/subscriptions/{OPEN}/sign-in"),
            Some(json!({"version": 1})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(rig.calls.starts.load(Ordering::SeqCst), 2);

    let (status, body) = rig
        .send(reqwest::Method::GET, "/v1/hofund/subscriptions", None)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let list: Value = serde_json::from_slice(&body).expect("list");
    let rows = list["subscriptions"].as_array().expect("rows");
    let open = rows.iter().find(|row| row["id"] == OPEN).expect("open");
    let withdrawn = rows
        .iter()
        .find(|row| row["id"] == WITHDRAWN)
        .expect("withdrawn");
    assert!(open.get("notice").is_none());
    assert_eq!(withdrawn["notice"], RISK);
    assert_eq!(withdrawn["accounts"][0]["id"], "saved-1");

    let export = rig._config.path().join("export.txt");
    std::fs::write(&export, "withdrawn-export").expect("export");
    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            "/v1/hofund/accounts/import",
            Some(json!({"version": 1, "path": export})),
        )
        .await;
    assert_eq!(message(status, &body), REFUSAL);

    std::fs::write(&export, "open-export").expect("export");
    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            "/v1/hofund/accounts/import",
            Some(json!({"version": 1, "path": export})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let imported: Value = serde_json::from_slice(&body).expect("import");
    assert_eq!(imported["imported"][0], "imported-open");

    let (status, body) = rig
        .send(reqwest::Method::GET, "/v1/hofund/accounts/discover", None)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let found: Value = serde_json::from_slice(&body).expect("discover");
    assert_eq!(found["found"][0]["subscription"], WITHDRAWN);

    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            "/v1/hofund/accounts/discover",
            Some(json!({"version": 1, "add": true})),
        )
        .await;
    assert_eq!(message(status, &body), REFUSAL);

    rig.calls.discover_open.store(true, Ordering::SeqCst);
    let (status, body) = rig
        .send(
            reqwest::Method::POST,
            "/v1/hofund/accounts/discover",
            Some(json!({"version": 1, "add": true})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let added: Value = serde_json::from_slice(&body).expect("mixed add");
    assert_eq!(added["imported"][0], "imported-open");
    let rows = added["found"].as_array().expect("rows");
    let open = rows
        .iter()
        .find(|row| row["subscription"] == OPEN)
        .expect("open row");
    let withdrawn = rows
        .iter()
        .find(|row| row["subscription"] == WITHDRAWN)
        .expect("withdrawn row");
    assert!(open.get("refused").is_none());
    assert_eq!(withdrawn["refused"], REFUSAL);

    support::shutdown(rig.running).await;
}
