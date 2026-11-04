//! Z Code (Z.ai) subscription.
//!
//! Imports the agent's encrypted credential store, optional device sign-in on
//! zcode.z.ai, resolves team plan keys when needed, lists GLM models, reads
//! personal or team package resets, and serves Anthropic Messages on the plan
//! API base.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HeaderName, HostContext,
    ModelList, PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx,
    SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod agent_sign_in;
mod api;
mod creds;
mod device;
mod discover;
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
/// Z Code portal used for device sign-in and device headers.
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
    SignInFlowKind::Device,
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
    /// Device-code authorization URL on zcode.z.ai.
    pub device_authorize: String,
    /// Device-code token URL on zcode.z.ai.
    pub device_token: String,
    /// Page where the user enters the device code.
    pub device_page: String,
    /// Plan portal root (device mid header scope).
    pub portal: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        let portal = ZCODE_PORTAL.to_owned();
        Self {
            device_authorize: format!("{portal}/api/v1/oauth/device/code"),
            device_token: format!("{portal}/api/v1/oauth/device/token"),
            device_page: format!("{portal}/device"),
            portal,
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
        device_mid_path: Arc::new(Mutex::new(None)),
    }
}

/// Builds Z Code subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ZcodeFactory {
    endpoints: Arc<Endpoints>,
    device_mid_path: Arc<Mutex<Option<PathBuf>>>,
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
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Zcode {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Device(
                hofund_plugin_api::DeviceFlow::new("zcode.z.ai")
                    .map_err(|error| BuildError::new(error.message()))?,
            ),
            pending: Mutex::new(HashMap::new()),
            device_mid_path: Arc::clone(&self.device_mid_path),
        }))
    }
}

struct Pending {
    device: device::DevicePending,
}

struct Zcode {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
    device_mid_path: Arc<Mutex<Option<PathBuf>>>,
}

impl Zcode {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
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
        let pending = device::start(host, &self.endpoints, &self.device_mid_path).await?;
        let page = device::verification_page(&self.endpoints);
        let challenge = SignInChallenge::device_code(pending.start.user_code().to_owned(), page)?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), Pending { device: pending });
        Ok(challenge)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Z Code device sign-in completes once the code is entered on zcode.z.ai",
            ));
        };
        let Some(pending) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Z Code sign-in is in progress for this session",
            ));
        };
        let signed_in = device::finish(
            host,
            &self.endpoints,
            &pending.device,
            &self.device_mid_path,
        )
        .await?;
        let account = host
            .accounts()
            .account(signed_in.id.clone(), signed_in.label)?;
        host.accounts().upsert(account, signed_in.secret).await?;
        Ok(signed_in.id)
    }

    async fn cancel_sign_in(
        &self,
        ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        let _ = self.take_pending(ctx.session());
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        match session::load(host, &self.endpoints, account).await {
            Ok(stored) if stored.ready(host.clock().now()) => Ok(AccountStatus::Ready),
            Ok(_) => Ok(needs_sign_in("the stored Z Code sign-in has expired")),
            Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
                Ok(needs_sign_in(error.message()))
            }
            Err(error) => Err(error),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        refresh::run(host, &self.endpoints, account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = session::load(host, &self.endpoints, account).await?;
        quota::read(host, &self.endpoints, &stored).await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let stored = session::load(host, &self.endpoints, account).await?;
        models::list(host, &self.endpoints, &stored).await
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
