//! Zed subscription plugin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod body;
mod creds;
mod discover;
mod models;
mod refresh;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("zed"),
    "Zed",
    SignInFlowKind::TokenFile,
)
.with_agent_sign_in(AgentSignIn::new(&["zed"], "ZED_CONFIG_DIR", ".config/zed"));

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
            flow: SignInFlow::TokenFile,
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Zed {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, PathBuf>>,
}

impl Zed {
    fn take_pending(&self, session: &str) -> Option<PathBuf> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
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
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        drop(self.take_pending(ctx.session()));
        let path = default_credential_path()?;
        creds::read_file(&path).map_err(|error| {
            PluginError::auth(format!(
                "Zed is not signed in at {}; {}",
                path.display(),
                error.message()
            ))
        })?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), path);
        SignInChallenge::instructions(
            "Zed credentials are on this machine; confirm to import them.",
        )
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Zed sign-in completes once the credential file is imported",
            ));
        };
        let Some(path) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Zed sign-in is in progress for this session",
            ));
        };
        let credential = creds::read_file(&path)?;
        discover::store(&credential, host).await
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
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        Ok(Vec::new())
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

fn default_credential_path() -> Result<PathBuf, PluginError> {
    if let Ok(dir) = std::env::var("ZED_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join(creds::CREDENTIAL_FILE));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| PluginError::io("cannot resolve the user home for sign-in"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("zed")
        .join(creds::CREDENTIAL_FILE))
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
