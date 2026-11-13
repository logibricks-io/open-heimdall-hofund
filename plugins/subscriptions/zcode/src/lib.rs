//! Z Code (Z.ai) subscription.
//!
//! Imports the agent's encrypted credential store, resolves team plan keys when
//! needed, lists GLM models, reads personal or team monitor quota, and serves
//! Anthropic Messages on the plan API base.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HeaderName, HostContext,
    ModelList, PluginError, PluginId, QuotaWindow, RawTable, Schema, SecretString, SignInChallenge,
    SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory,
    SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::TokenCache;
use std::time::Duration;

mod agent_sign_in;
mod api;
mod checkin;
mod creds;
mod device;
mod discover;
mod gift;
mod http_auth;
mod models;
mod provider_config;
mod quota;
mod refresh;
mod session;
mod sign_in;
mod start;
mod team;
mod upstream;

/// Anthropic-compatible API for Z.ai individual plans.
pub(crate) const ZAI_ANTHROPIC_BASE: &str = "https://api.z.ai/api/anthropic";
/// Anthropic-compatible API for BigModel individual plans.
pub(crate) const BIGMODEL_ANTHROPIC_BASE: &str = "https://open.bigmodel.cn/api/anthropic";
/// Z Code portal root (device mid header scope on portal URLs).
pub(crate) const ZCODE_PORTAL: &str = "https://zcode.z.ai";
/// Anthropic-compatible API for Z Code Start Plan and other gift balances.
pub(crate) const START_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";
/// Z.ai business API origin.
pub(crate) const ZAI_BIZ: &str = "https://api.z.ai";
/// BigModel business API origin.
pub(crate) const BIGMODEL_BIZ: &str = "https://bigmodel.cn";

/// User-Agent sent on vendor JSON calls, as the reference does.
pub(crate) const APP_VERSION: &str = "3.17.2";

pub(crate) const AGENT_SIGN_IN: AgentSignIn =
    AgentSignIn::new(&["zcode", "z-code"], "ZCODE_HOME", ".zcode");

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("zcode"),
    "Z Code",
    SignInFlowKind::TokenFile,
)
.with_agent_sign_in(AGENT_SIGN_IN)
.with_daily_checkin(true);

/// Registers the Z Code subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for Z Code and plan APIs. Tests aim these at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Plan portal root (device mid header scope).
    pub portal: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            portal: ZCODE_PORTAL.to_owned(),
        }
    }
}

impl Endpoints {
    pub(crate) fn biz_root(&self, anthropic_base: &str) -> &str {
        if anthropic_base.contains("bigmodel.cn") {
            BIGMODEL_BIZ
        } else {
            ZAI_BIZ
        }
    }

    pub(crate) fn start_base(&self) -> String {
        if self.portal == ZCODE_PORTAL {
            START_BASE.to_owned()
        } else {
            format!(
                "{}/api/v1/zcode-plan/anthropic",
                self.portal.trim_end_matches('/')
            )
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> ZcodeFactory {
    ZcodeFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Z Code subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ZcodeFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ZcodeConfig;

impl ValidConfig for ZcodeConfig {}

impl SubscriptionFactory for ZcodeFactory {
    type Config = ZcodeConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(Vec::new(), &[]).expect("empty object schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        if !raw.is_empty() {
            let key = raw.keys().next().expect("non-empty");
            return Err(ConfigError::new("unknown key").with_path(key));
        }
        Ok(ZcodeConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let device_file =
            device::device_path(host).map_err(|error| BuildError::new(error.message()))?;
        Ok(Arc::new(Zcode {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::TokenFile,
            device_file,
            team_keys: Arc::new(TokenCache::new(Duration::ZERO)),
            pending: Mutex::new(HashMap::new()),
            touch_seq: AtomicU64::new(0),
        }))
    }
}

struct Zcode {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    device_file: PathBuf,
    team_keys: Arc<TokenCache<SecretString>>,
    pending: Mutex<HashMap<String, sign_in::Pending>>,
    touch_seq: AtomicU64,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Zcode {
    fn id(&self) -> &PluginId {
        SPEC.plugin()
    }

    fn flow(&self) -> &SignInFlow {
        &self.flow
    }

    async fn start_sign_in(
        &self,
        ctx: &SignInCtx,
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        let (pending, challenge) = sign_in::start(ctx, host, &self.endpoints).await?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), pending);
        Ok(challenge)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        match proof {
            SignInProof::Token(key) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(ctx.session());
                sign_in::import_api_key(ctx, host, key).await
            }
            SignInProof::Await => {
                let session = ctx.session();
                let pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(session)
                    .cloned()
                    .ok_or_else(|| {
                        PluginError::config("no sign-in is in progress for this session")
                    })?;
                let result = sign_in::finish(host, &self.endpoints, pending, || {
                    self.pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(session)
                })
                .await;
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(session);
                result
            }
            _ => Err(PluginError::config(
                "sign-in completes after browser approval or with an API key",
            )),
        }
    }

    async fn cancel_sign_in(
        &self,
        ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        let _ = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(needs_sign_in(
                "no Z Code sign-in is stored for this account",
            ));
        };
        match session::offline_status(&secret) {
            Ok(()) => {
                let auth = creds::parse_secret(&secret)?;
                if let Some(jwt) = auth.jwt
                    && let Err(error) = gift::ensure_live_jwt(host, &jwt)
                {
                    return Ok(needs_sign_in(error.message()));
                }
                Ok(AccountStatus::Ready)
            }
            Err(error) => Ok(needs_sign_in(error.message())),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        let result = refresh::run(host, &self.endpoints, account, &self.device_file).await;
        if result.is_ok() {
            self.team_keys.remove(account);
        }
        result
    }

    async fn daily_checkin(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<hofund_plugin_api::CheckinResult, PluginError> {
        checkin::run(
            account,
            host,
            Arc::clone(&self.endpoints),
            &self.device_file,
            &self.team_keys,
            &self.touch_seq,
        )
        .await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = session::load(
            host,
            &self.endpoints,
            account,
            &self.device_file,
            &self.team_keys,
        )
        .await?;
        quota::read(
            host,
            &self.endpoints,
            account,
            &stored,
            &self.device_file,
            &self.team_keys,
        )
        .await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let stored = session::load(
            host,
            &self.endpoints,
            account,
            &self.device_file,
            &self.team_keys,
        )
        .await?;
        models::list(
            host,
            &self.endpoints,
            account,
            &stored,
            &self.device_file,
            &self.team_keys,
        )
        .await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::ZcodeUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
            self.device_file.clone(),
            Arc::clone(&self.team_keys),
        )))
    }

    async fn write_agent_sign_in(
        &self,
        account: &AccountId,
        home: &Path,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        agent_sign_in::write(account, home, host).await
    }

    async fn discover_agent_sign_in(
        &self,
        home: &Path,
    ) -> Result<Vec<hofund_plugin_api::DiscoveredAgentAccount>, PluginError> {
        discover::list(home)
    }

    async fn import_agent_sign_in(
        &self,
        home: &Path,
        reference: &str,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        discover::import(home, reference, host).await
    }
}

fn needs_sign_in(reason: &str) -> AccountStatus {
    AccountStatus::NeedsSignIn {
        reason: reason.to_owned(),
    }
}

/// Authorization header style for GLM plan keys (bare key, no Bearer prefix).
pub(crate) fn plan_auth_style() -> hofund_plugin_api::CredentialStyle {
    hofund_plugin_api::CredentialStyle::NamedHeader(HeaderName::new("authorization"))
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
