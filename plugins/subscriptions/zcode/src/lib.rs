//! Z Code (Z.ai) subscription.
//!
//! Imports the agent's encrypted credential store, resolves team plan keys when
//! needed, lists GLM models, reads personal or team monitor quota, and serves
//! Anthropic Messages on the plan API base.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BinaryName, BuildError, ConfigError, HeaderName,
    HostContext, ModelList, PluginError, PluginId, QuotaWindow, RawTable, Schema, SecretString,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::TokenCache;
use std::time::Duration;

mod agent_sign_in;
mod api;
mod creds;
mod device;
mod discover;
mod http_auth;
mod models;
mod quota;
mod refresh;
mod session;
mod team;
mod upstream;

/// Anthropic-compatible API for Z.ai individual plans.
pub(crate) const ZAI_ANTHROPIC_BASE: &str = "https://api.z.ai/api/anthropic";
/// Anthropic-compatible API for BigModel individual plans.
pub(crate) const BIGMODEL_ANTHROPIC_BASE: &str = "https://open.bigmodel.cn/api/anthropic";
/// Z Code portal root (device mid header scope on portal URLs).
pub(crate) const ZCODE_PORTAL: &str = "https://zcode.z.ai";
/// Z.ai business API origin.
pub(crate) const ZAI_BIZ: &str = "https://api.z.ai";
/// BigModel business API origin.
pub(crate) const BIGMODEL_BIZ: &str = "https://bigmodel.cn";

/// User-Agent sent on vendor JSON calls, as the reference does.
pub(crate) const APP_VERSION: &str = "3.14.3";

pub(crate) const AGENT_SIGN_IN: AgentSignIn =
    AgentSignIn::new(&["zcode", "z-code"], "ZCODE_HOME", ".zcode");

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("zcode"),
    "Z Code",
    SignInFlowKind::Bridge,
)
.with_agent_sign_in(AGENT_SIGN_IN);

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
            flow: SignInFlow::Bridge {
                command: BinaryName::new("zcode"),
            },
            device_file,
            team_keys: Arc::new(TokenCache::new(Duration::ZERO)),
        }))
    }
}

struct Zcode {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    device_file: PathBuf,
    team_keys: Arc<TokenCache<SecretString>>,
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
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        Err(PluginError::unsupported(
            "Sign in with the Z Code app, then import with agent discovery or agent connect",
        ))
    }

    async fn complete_sign_in(
        &self,
        _ctx: &SignInCtx,
        _proof: SignInProof,
        _host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        Err(PluginError::unsupported(
            "Sign in with the Z Code app, then import with agent discovery or agent connect",
        ))
    }

    async fn cancel_sign_in(
        &self,
        _ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
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
            Ok(()) => Ok(AccountStatus::Ready),
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
        quota::read(host, account, &stored, &self.device_file, &self.team_keys).await
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
        models::list(host, account, &stored, &self.team_keys).await
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
        _host: &HostContext,
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
