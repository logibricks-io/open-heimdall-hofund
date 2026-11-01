//! Test-only subscription fixture for daemon membership and quota polls.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, Completion, ConfigError, DeviceFlow, HostContext,
    ModelList, ModelSpec, PluginError, PluginId, ProtocolId, QuotaWindow, RawTable, RawValue, Request, Schema,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

pub(crate) fn contribute() -> Contribution {
    Contribution::new().subscription(FixtureFactory)
}

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fixture-sub"),
    "Fixture Subscription",
    SignInFlowKind::Device,
);

struct FixtureFactory;

static MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);
static MODEL_FAIL: AtomicBool = AtomicBool::new(false);

/// Number of model-list calls made by the fixture.
#[doc(hidden)]
#[must_use]
pub fn model_calls() -> usize {
    MODEL_CALLS.load(Ordering::SeqCst)
}

/// Makes subsequent fixture model-list calls fail or succeed.
#[doc(hidden)]
pub fn set_model_fail(fail: bool) {
    MODEL_FAIL.store(fail, Ordering::SeqCst);
}

/// Resets the fixture's model-list state.
#[doc(hidden)]
pub fn reset_models() {
    MODEL_CALLS.store(0, Ordering::SeqCst);
    MODEL_FAIL.store(false, Ordering::SeqCst);
}

struct FixtureConfig {
    flow: SignInFlow,
    needs_sign_in: BTreeSet<String>,
    quota: QuotaMode,
}

#[derive(Clone, Copy)]
enum QuotaMode {
    Ok,
    Exhausted,
    Scoped,
    NonBlocking,
    Unsupported,
}

impl ValidConfig for FixtureConfig {}

impl SubscriptionFactory for FixtureFactory {
    type Config = FixtureConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![
                ("needs_sign_in".to_owned(), Schema::string()),
                ("quota".to_owned(), Schema::string()),
                ("host".to_owned(), Schema::string()),
                ("flow".to_owned(), Schema::string()),
            ],
            &[],
        )
        .expect("fixture schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if !matches!(key, "needs_sign_in" | "quota" | "host" | "flow") {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let needs_sign_in = match raw.get("needs_sign_in") {
            None => BTreeSet::new(),
            Some(RawValue::String(value)) => value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(_) => {
                return Err(
                    ConfigError::new("needs_sign_in must be a string").with_path("needs_sign_in")
                );
            }
        };
        let quota = match raw.get("quota") {
            None => QuotaMode::Ok,
            Some(RawValue::String(value)) => match value.as_str() {
                "ok" => QuotaMode::Ok,
                "exhausted" => QuotaMode::Exhausted,
                "scoped" => QuotaMode::Scoped,
                "nonblocking" => QuotaMode::NonBlocking,
                "unsupported" => QuotaMode::Unsupported,
                _ => {
                    return Err(ConfigError::new("unknown quota mode").with_path("quota"));
                }
            },
            Some(_) => return Err(ConfigError::new("quota must be a string").with_path("quota")),
        };
        let host = match raw.get("host") {
            None => "example.test",
            Some(RawValue::String(value)) => value.as_str(),
            Some(_) => return Err(ConfigError::new("host must be a string").with_path("host")),
        };
        let device = DeviceFlow::new(host)
            .and_then(|flow| flow.with_path("/device"))
            .map_err(|error| ConfigError::new(error.to_string()).with_path("host"))?;
        // `flow = "token-file"` builds an instance whose flow disagrees with the
        // spec, so tests can exercise the factory's kind check.
        let flow = match raw.get("flow") {
            None => SignInFlow::Device(device),
            Some(RawValue::String(value)) if value == "token-file" => SignInFlow::TokenFile,
            Some(_) => return Err(ConfigError::new("unknown flow").with_path("flow")),
        };
        Ok(FixtureConfig {
            flow,
            needs_sign_in,
            quota,
        })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(FixtureSubscription {
            flow: cfg.flow.clone(),
            needs_sign_in: cfg.needs_sign_in.clone(),
            quota: cfg.quota,
        }))
    }
}

struct FixtureSubscription {
    flow: SignInFlow,
    needs_sign_in: BTreeSet<String>,
    quota: QuotaMode,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for FixtureSubscription {
    fn id(&self) -> &PluginId {
        SPEC.plugin()
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    async fn start_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        Ok(SignInChallenge::Instructions {
            text: "fixture".to_owned(),
        })
    }

    async fn complete_sign_in(
        &self,
        _ctx: &SignInCtx,
        _proof: SignInProof,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        Ok(AccountId::from_static("fixture"))
    }

    async fn account_status(
        &self,
        account: &AccountId,
        _host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        if self.needs_sign_in.contains(account.as_str()) {
            Ok(AccountStatus::NeedsSignIn {
                reason: "fixture needs sign-in".to_owned(),
            })
        } else {
            Ok(AccountStatus::Ready)
        }
    }

    async fn refresh(&self, _account: &AccountId, _host: &HostContext) -> Result<(), PluginError> {
        Ok(())
    }

    async fn quota(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        match self.quota {
            QuotaMode::Unsupported => Err(PluginError::unsupported("fixture quota unsupported")),
            QuotaMode::Ok => Ok(vec![QuotaWindow::new("ok", 1, Some(10), None)?]),
            QuotaMode::Exhausted => {
                let reset = SystemTime::now() + Duration::from_secs(60);
                Ok(vec![QuotaWindow::new("spent", 10, Some(10), Some(reset))?])
            }
            QuotaMode::Scoped => {
                let reset = SystemTime::now() + Duration::from_secs(60);
                Ok(vec![
                    QuotaWindow::new("scoped", 10, Some(10), Some(reset))?
                        .with_scope_model("gpt-test"),
                ])
            }
            QuotaMode::NonBlocking => {
                let reset = SystemTime::now() + Duration::from_secs(60);
                Ok(vec![
                    QuotaWindow::new("credits", 10, Some(10), Some(reset))?.with_blocking(false),
                ])
            }
        }
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        MODEL_CALLS.fetch_add(1, Ordering::SeqCst);
        if MODEL_FAIL.load(Ordering::SeqCst) {
            return Err(PluginError::transport("fixture model list failed"));
        }
        Ok(ModelList::new(
            vec![ModelSpec::new("fixture-model", None)?],
            None,
        ))
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(FixtureUpstream {
            account: account.clone(),
            host: host.clone(),
        }))
    }
}

/// Reports a spent quota through its member host, then declines the call.
struct FixtureUpstream {
    account: AccountId,
    host: HostContext,
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for FixtureUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let _ = &self.account;
        let observe = self.host.observe();
        observe.observe(
            observe
                .observation()
                .with_remaining(0)
                .with_limit(10)
                .with_window_reset(Duration::from_secs(60)),
        )?;
        Err(PluginError::unsupported("fixture-sub does not execute"))
    }
}
