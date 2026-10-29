//! Test-only subscription fixture for daemon membership and quota polls.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, Completion, ConfigError, HostContext, ModelList,
    PluginError, PluginId, ProtocolId, QuotaWindow, RawTable, RawValue, Request, Schema,
    SignInChallenge, SignInCtx, SignInFlow, SignInProof, StaticUrl, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

pub(crate) fn contribute() -> Contribution {
    Contribution::new().subscription(FixtureFactory)
}

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("fixture-sub"),
    "Fixture Subscription",
    SignInFlow::Device {
        verification_url: StaticUrl::new("https://example.test/device"),
    },
);

struct FixtureFactory;

struct FixtureConfig {
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
            ],
            &[],
        )
        .expect("fixture schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "needs_sign_in" && key != "quota" {
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
        Ok(FixtureConfig {
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
            needs_sign_in: cfg.needs_sign_in.clone(),
            quota: cfg.quota,
        }))
    }
}

struct FixtureSubscription {
    needs_sign_in: BTreeSet<String>,
    quota: QuotaMode,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for FixtureSubscription {
    fn id(&self) -> &PluginId {
        SPEC.plugin()
    }

    fn flow(&self) -> &SignInFlow {
        const FLOW: SignInFlow = SignInFlow::Device {
            verification_url: StaticUrl::new("https://example.test/device"),
        };
        &FLOW
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
        Ok(ModelList::new(Vec::new(), None))
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
