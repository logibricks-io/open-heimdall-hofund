//! Zed subscription plugin with native RSA loopback sign-in and credential import.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, HostContext, ModelList, PluginError,
    PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod body;
mod creds;
mod errors;
mod headers;
mod import;
mod models;
mod org;
mod quota;
mod refresh;
mod sign_in;
mod stream;
mod thread;
mod tokens;
mod upstream;

static SPEC: SubscriptionSpec =
    SubscriptionSpec::from_static(PluginId::from_static("zed"), "Zed", SignInFlowKind::Pkce);

/// Registers the Zed subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Zed cloud endpoints. Tests aim these at mocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Zed cloud origin (`https://cloud.zed.dev`).
    pub cloud: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            cloud: "https://cloud.zed.dev".to_owned(),
        }
    }
}

/// Builds a factory aimed at custom cloud endpoints (tests and mocks).
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> ZedFactory {
    ZedFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Subscription factory carrying endpoint configuration.
#[derive(Clone, Debug)]
pub struct ZedFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[derive(Clone, Copy, Debug)]
pub struct ZedConfig;

impl ValidConfig for ZedConfig {}

impl SubscriptionFactory for ZedFactory {
    type Config = ZedConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(Vec::new(), &[]).expect("empty schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        if !raw.is_empty() {
            let key = raw.keys().next().expect("non-empty");
            return Err(ConfigError::new("unknown key").with_path(key));
        }
        Ok(ZedConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Zed {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Zed {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::Pending>>,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Zed {
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
        let (pending, challenge) = sign_in::start(ctx, host)?;
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
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session())
            .ok_or_else(|| PluginError::config("no Zed sign-in is in progress for this session"))?;
        host.loopback().release(ctx);
        sign_in::complete(host, Arc::clone(&self.endpoints), pending, proof).await
    }

    async fn cancel_sign_in(&self, ctx: &SignInCtx, host: &HostContext) -> Result<(), PluginError> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        host.loopback().release(ctx);
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        match creds::Stored::load(host, account).await {
            Ok(_) => Ok(AccountStatus::Ready),
            Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
                Ok(AccountStatus::NeedsSignIn {
                    reason: error.to_string(),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        refresh::run(host, Arc::clone(&self.endpoints), account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = creds::Stored::load(host, account).await?;
        quota::windows(host, Arc::clone(&self.endpoints), &stored).await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        models::list_account(host, Arc::clone(&self.endpoints), account).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::ZedUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
        )))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        import::import_accounts(host, text).await
    }
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
