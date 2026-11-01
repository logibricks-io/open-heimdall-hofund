//! Integration tests for [`super::tick_inner`] retry fairness and status listing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Timelike};
use hofund_config::{Accounts, Config, Stays, Value};
use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, DynSubscriptionFactory, HostContext,
    PluginError, PluginId, RawTable, Schema, SecretString, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider,
};
use hofund_plugin_api::{ProtocolId, Request};
use hofund_plugin_sdk::daily_warmup::TIMES_KEY;
use tokio::sync::Mutex as AsyncMutex;

use crate::registry::{DaemonRegistry, RegistryAdapter};
use crate::runtime::Runtime;
use crate::subscriptions::SubscriptionState;
use crate::warmup::{
    FAILED_RETRY, WarmupStore, daily_due, list_statuses, local_offset, tick_with_runner,
};

use super::WarmupRunner;

const PLUGIN: &str = "warm-test";
const ACCOUNT_A: &str = "a-0000000000000001";
const ACCOUNT_B: &str = "a-0000000000000002";
const SECRET: &str = "sk-warm-test-secret";

static WARM_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("warm-test"),
    "Warm-up tick test",
    SignInFlowKind::Device,
)
.with_daily_warmup(true);

struct WarmRegistry {
    inner: Arc<dyn DaemonRegistry>,
    factory: Arc<dyn DynSubscriptionFactory>,
}

impl DaemonRegistry for WarmRegistry {
    fn factory(&self, id: &str) -> Option<Arc<dyn hofund_plugin_api::DynUpstreamFactory>> {
        self.inner.factory(id)
    }

    fn is_kernel_factory(&self, id: &str) -> bool {
        self.inner.is_kernel_factory(id)
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        self.inner.builtin_factory_ids()
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        self.inner.builtin_protocols()
    }

    fn find_preset(
        &self,
        id: &str,
    ) -> Option<(
        &'static hofund_plugin_registry::CompiledPreset,
        Option<&'static str>,
    )> {
        self.inner.find_preset(id)
    }

    fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
        self.inner.agents()
    }

    fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
        self.inner.surfaces()
    }

    fn subscription(&self, id: &str) -> Option<Arc<dyn DynSubscriptionFactory>> {
        if id == PLUGIN {
            Some(Arc::clone(&self.factory))
        } else {
            self.inner.subscription(id)
        }
    }

    fn host_plugin(&self, protocol: ProtocolId, host: &str) -> Option<&'static str> {
        self.inner.host_plugin(protocol, host)
    }

    fn plan_quota(&self) -> Option<Arc<dyn hofund_plugin_api::PlanQuota>> {
        self.inner.plan_quota()
    }

    fn subscription_ids(&self) -> Vec<&'static str> {
        self.inner.subscription_ids()
    }

    fn agent_sign_in(&self, id: &str) -> Option<hofund_plugin_api::AgentSignIn> {
        self.inner.agent_sign_in(id)
    }

    fn balance_quota(&self) -> Option<Arc<dyn hofund_plugin_api::BalanceQuota>> {
        self.inner.balance_quota()
    }

    fn upstream_max_accounts(&self, id: &str) -> u64 {
        self.inner.upstream_max_accounts(id)
    }

    fn subscription_max_accounts(&self, id: &str) -> u64 {
        self.inner.subscription_max_accounts(id)
    }
}

struct WarmFake {
    id: PluginId,
    flow: SignInFlow,
}

struct WarmUpstream;

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for WarmUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<hofund_plugin_api::Completion, PluginError> {
        Err(PluginError::unsupported("unused in warm-up tick test"))
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for WarmFake {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    async fn start_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn complete_sign_in(
        &self,
        _ctx: &SignInCtx,
        _proof: SignInProof,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn cancel_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn account_status(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, _account: &AccountId, _host: &HostContext) -> Result<(), PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn quota(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<hofund_plugin_api::QuotaWindow>, PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<hofund_plugin_api::ModelList, PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    fn upstream(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(WarmUpstream))
    }
}

struct WarmFactory;

struct EmptyConfig;

impl hofund_plugin_api::ValidConfig for EmptyConfig {}

impl SubscriptionFactory for WarmFactory {
    type Config = EmptyConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &WARM_SPEC
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
        Ok(Arc::new(WarmFake {
            id: PluginId::from_static("warm-test"),
            flow: SignInFlow::Device(
                hofund_plugin_api::DeviceFlow::new("example.test").expect("device host"),
            ),
        }))
    }
}

fn config_with_times(times: &str) -> Config {
    let mut options = BTreeMap::new();
    options.insert(
        TIMES_KEY.to_owned(),
        Value::Array(vec![Value::String(times.to_owned())]),
    );
    let mut config = hofund_config::load(
        r#"
[[providers]]
id = "p"
protocol = "openai-chat"
base_url = "http://127.0.0.1:9"
api_key = "k"

[[routes]]
model = "m"
upstreams = ["p"]

[[subscriptions]]
id = "warm-test"
accounts = "*"
"#,
    )
    .expect("base config");
    config.subscriptions[0].options = options;
    config.subscriptions[0].accounts = Accounts::All;
    config.subscriptions[0].routing = "smart".to_owned();
    config.subscriptions[0].stays = Stays::Auto;
    config
}

fn due_times(now: SystemTime) -> String {
    let zone = local_offset(now);
    assert!(
        daily_due(&[chrono_offset_time(now, zone)], "", false, now, zone).is_some(),
        "test anchor must be due"
    );
    chrono_offset_time(now, zone)
}

fn chrono_offset_time(now: SystemTime, zone: chrono::FixedOffset) -> String {
    let secs = now.duration_since(UNIX_EPOCH).expect("epoch").as_secs();
    let local = zone
        .timestamp_opt(secs as i64, 0)
        .single()
        .expect("local time");
    format!("{:02}:{:02}", local.hour(), local.minute())
}

struct Fixture {
    _dir: tempfile::TempDir,
    runtime: Arc<Runtime>,
    store: Arc<WarmupStore>,
    registry: Arc<WarmRegistry>,
    now: Arc<Mutex<SystemTime>>,
    member_a: String,
}

async fn fixture() -> Fixture {
    let anchor = SystemTime::now();
    let times = due_times(anchor);
    let config = config_with_times(&times);
    let dir = tempfile::tempdir().expect("temp");
    let config_path = dir.path().join("hofund.toml");
    std::fs::write(&config_path, "# test").expect("write");
    let now = Arc::new(Mutex::new(anchor));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().expect("now")) as Arc<dyn Fn() -> SystemTime + Send + Sync>
    };
    let factory = Arc::new(WarmFactory) as Arc<dyn DynSubscriptionFactory>;
    let registry = Arc::new(WarmRegistry {
        inner: Arc::new(RegistryAdapter),
        factory,
    });
    let runtime = Runtime::new_with_quota_clock(
        Arc::clone(&registry) as Arc<dyn DaemonRegistry>,
        &config_path,
        None,
        clock,
    );
    runtime
        .initial_snapshot(&config, None)
        .expect("initial snapshot");
    let state = runtime
        .subscription_states()
        .get(PLUGIN)
        .expect("subscription")
        .clone();
    for id in [ACCOUNT_A, ACCOUNT_B] {
        let accounts = state.host().accounts();
        let account = accounts
            .account(AccountId::try_from(id).expect("id"), "label")
            .expect("account row");
        accounts
            .upsert(account, SecretString::from(SECRET.to_owned()))
            .await
            .expect("upsert");
    }
    runtime.refresh_subscriptions().await;
    let members = runtime
        .subscription_states()
        .get(PLUGIN)
        .expect("subscription")
        .members();
    let member_a = members
        .iter()
        .find(|member| member.account.as_str() == ACCOUNT_A)
        .expect("member A")
        .id
        .to_string();
    let _member_b = members
        .iter()
        .find(|member| member.account.as_str() == ACCOUNT_B)
        .expect("member B")
        .id
        .to_string();
    Fixture {
        _dir: dir,
        store: Arc::clone(runtime.warmup()),
        runtime,
        registry,
        now,
        member_a,
    }
}

struct RecordingRunner {
    seen: Arc<AsyncMutex<Vec<String>>>,
    fail_a: Arc<std::sync::atomic::AtomicBool>,
}

#[hofund_plugin_sdk::async_trait]
impl WarmupRunner for RecordingRunner {
    async fn run(&self, _state: &SubscriptionState, account: &AccountId) -> Result<(), String> {
        let id = account.as_str().to_owned();
        self.seen.lock().await.push(id.clone());
        if id == ACCOUNT_A && self.fail_a.load(std::sync::atomic::Ordering::SeqCst) {
            Err("injected warm-up failure".to_owned())
        } else {
            Ok(())
        }
    }
}

fn runner(
    seen: &Arc<AsyncMutex<Vec<String>>>,
    fail_a: &Arc<std::sync::atomic::AtomicBool>,
) -> RecordingRunner {
    RecordingRunner {
        seen: Arc::clone(seen),
        fail_a: Arc::clone(fail_a),
    }
}

#[tokio::test]
async fn a3a_26_tick_skips_cooling_account_and_clears_retry_after_success() {
    let fixture = fixture().await;
    let seen = Arc::new(AsyncMutex::new(Vec::<String>::new()));
    let fail_a = Arc::new(std::sync::atomic::AtomicBool::new(true));

    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner(&seen, &fail_a),
    )
    .await;
    assert_eq!(*seen.lock().await, vec![ACCOUNT_A.to_owned()]);
    assert!(
        fixture
            .store
            .failed_retry_until(&fixture.member_a)
            .is_some()
    );

    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner(&seen, &fail_a),
    )
    .await;
    assert_eq!(
        *seen.lock().await,
        vec![ACCOUNT_A.to_owned(), ACCOUNT_B.to_owned()]
    );

    {
        let mut guard = fixture.now.lock().expect("now");
        *guard += FAILED_RETRY + Duration::from_secs(1);
    }
    fail_a.store(false, std::sync::atomic::Ordering::SeqCst);

    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner(&seen, &fail_a),
    )
    .await;
    assert_eq!(
        seen.lock().await.last().map(String::as_str),
        Some(ACCOUNT_A)
    );
    assert!(
        fixture
            .store
            .failed_retry_until(&fixture.member_a)
            .is_none()
    );
}

#[tokio::test]
async fn a3a_27_list_statuses_includes_never_run_configured_account() {
    let fixture = fixture().await;
    let rows = list_statuses(
        &fixture.runtime,
        fixture.registry.as_ref(),
        fixture.store.as_ref(),
    );
    let row = rows
        .iter()
        .find(|row| row.account().as_str() == ACCOUNT_B)
        .expect("configured account B");
    assert!(row.last().is_none());
}
