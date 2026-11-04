//! Integration tests for daily check-in scheduling.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_config::{Accounts, Config, Stays, Value};
use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, CheckinOutcome, CheckinResult, ConfigError,
    DynSubscriptionFactory, HostContext, PluginError, PluginId, RawTable, Schema, SecretString,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider,
};
use hofund_plugin_api::{ProtocolId, Request};
use hofund_plugin_sdk::daily_checkin::ENABLED_KEY;

use crate::checkin::{CheckinRunner, tick_with_runner};
use crate::registry::{DaemonRegistry, RegistryAdapter};
use crate::runtime::Runtime;

const PLUGIN: &str = "check-test";
const ACCOUNT: &str = "a-0000000000000001";
const SECRET: &str = "sk-check-test-secret";

static CHECK_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("check-test"),
    "Check-in tick test",
    SignInFlowKind::Device,
)
.with_daily_checkin(true);

struct CheckRegistry {
    inner: Arc<dyn DaemonRegistry>,
    factory: Arc<dyn DynSubscriptionFactory>,
}

impl DaemonRegistry for CheckRegistry {
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
        &'static crate::registry::CompiledPreset,
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

struct CheckFake {
    id: PluginId,
    flow: SignInFlow,
}

struct CheckUpstream;

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for CheckUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<hofund_plugin_api::Completion, PluginError> {
        Err(PluginError::unsupported("unused in check-in tick test"))
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for CheckFake {
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
        Err(PluginError::unsupported("unused"))
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<hofund_plugin_api::ModelList, PluginError> {
        Err(PluginError::unsupported("unused"))
    }

    async fn daily_checkin(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<CheckinResult, PluginError> {
        Ok(CheckinResult::new(
            CheckinOutcome::Failed,
            "fixture refused",
        ))
    }

    fn upstream(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(CheckUpstream))
    }
}

struct CheckFactory;

struct EmptyConfig;

impl hofund_plugin_api::ValidConfig for EmptyConfig {}

impl SubscriptionFactory for CheckFactory {
    type Config = EmptyConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &CHECK_SPEC
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
        Ok(Arc::new(CheckFake {
            id: PluginId::from_static("check-test"),
            flow: SignInFlow::Device(
                hofund_plugin_api::DeviceFlow::new("example.test").expect("device host"),
            ),
        }))
    }
}

fn config(enabled: bool) -> Config {
    let mut options = BTreeMap::new();
    options.insert(ENABLED_KEY.to_owned(), Value::Boolean(enabled));
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
id = "check-test"
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

struct Fixture {
    _dir: tempfile::TempDir,
    runtime: Arc<Runtime>,
    store: Arc<crate::checkin::CheckinStore>,
    registry: Arc<CheckRegistry>,
}

struct RecordingRunner {
    outcomes: Arc<Mutex<Vec<Result<CheckinResult, String>>>>,
}

#[async_trait::async_trait]
impl CheckinRunner for RecordingRunner {
    async fn run(
        &self,
        _state: &crate::subscriptions::SubscriptionState,
        _account: &AccountId,
    ) -> Result<CheckinResult, String> {
        self.outcomes
            .lock()
            .expect("outcomes")
            .pop()
            .unwrap_or_else(|| Ok(CheckinResult::new(CheckinOutcome::Claimed, "")))
    }
}

async fn fixture(enabled: bool) -> Fixture {
    let anchor = UNIX_EPOCH + Duration::from_secs(1_762_195_200);
    let config = config(enabled);
    let dir = tempfile::tempdir().expect("temp");
    let config_path = dir.path().join("hofund.toml");
    std::fs::write(&config_path, "# test").expect("write");
    let now = Arc::new(Mutex::new(anchor));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || *now.lock().expect("now")) as Arc<dyn Fn() -> SystemTime + Send + Sync>
    };
    let factory = Arc::new(CheckFactory) as Arc<dyn DynSubscriptionFactory>;
    let registry = Arc::new(CheckRegistry {
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
    let accounts = state.host().accounts();
    let account = accounts
        .account(AccountId::try_from(ACCOUNT).expect("id"), "label")
        .expect("account row");
    accounts
        .upsert(account, SecretString::from(SECRET.to_owned()))
        .await
        .expect("upsert");
    runtime.refresh_subscriptions().await;
    Fixture {
        _dir: dir,
        store: Arc::clone(runtime.checkin()),
        runtime,
        registry,
    }
}

#[tokio::test]
async fn a3b_1_opt_in_off_skips_scheduled_checkin() {
    let fixture = fixture(false).await;
    let runner = RecordingRunner {
        outcomes: Arc::new(Mutex::new(vec![Ok(CheckinResult::new(
            CheckinOutcome::Claimed,
            "",
        ))])),
    };
    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner,
    )
    .await;
    assert_eq!(runner.outcomes.lock().expect("outcomes").len(), 1);
}

#[tokio::test]
async fn a3b_3_persistence_skips_second_tick_same_day() {
    let fixture = fixture(true).await;
    let runner = RecordingRunner {
        outcomes: Arc::new(Mutex::new(vec![
            Ok(CheckinResult::new(CheckinOutcome::Claimed, "")),
            Ok(CheckinResult::new(CheckinOutcome::Claimed, "")),
        ])),
    };
    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner,
    )
    .await;
    tick_with_runner(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        &runner,
    )
    .await;
    assert_eq!(runner.outcomes.lock().expect("outcomes").len(), 1);
}

#[tokio::test]
async fn a3b_6_manual_run_now_checks_one_account() {
    let fixture = fixture(true).await;
    let record = crate::checkin::run_now(
        &fixture.runtime,
        &fixture.store,
        fixture.registry.as_ref(),
        PLUGIN,
        ACCOUNT,
    )
    .await
    .expect("manual check-in");
    assert_eq!(record.outcome(), CheckinOutcome::Failed);
}
