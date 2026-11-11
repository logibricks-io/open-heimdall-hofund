//! Devin subscription plugin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BinaryName, BuildError, ConfigError, HostContext,
    ModelList, PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx,
    SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod agent_sign_in;
#[allow(dead_code)]
mod connect;
mod creds;
mod discover;
mod models;
mod snapshot_families;
mod upstream;

pub(crate) const COMMAND: BinaryName = BinaryName::new("devin");

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("devin"),
    "Devin",
    SignInFlowKind::Bridge,
)
.with_agent_sign_in(AgentSignIn::new(
    &["devin"],
    "XDG_DATA_HOME",
    ".local/share/devin",
));

/// Registers the Devin subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(DevinFactory)
}

/// Builds Devin subscription instances.
#[derive(Clone, Copy, Debug)]
pub struct DevinFactory;

/// Validated config: no options yet.
#[derive(Clone, Copy, Debug)]
pub struct DevinConfig;

impl ValidConfig for DevinConfig {}

impl SubscriptionFactory for DevinFactory {
    type Config = DevinConfig;

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
        Ok(DevinConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Devin {
            flow: SignInFlow::Bridge { command: COMMAND },
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Devin {
    flow: SignInFlow,
    pending: Mutex<HashMap<String, PathBuf>>,
}

impl Devin {
    fn take_pending(&self, session: &str) -> Option<PathBuf> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Devin {
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
        let path = creds::default_credentials_path()?;
        let credential = creds::read_credentials(&path).map_err(|error| {
            PluginError::auth(format!(
                "Devin is not signed in at {}; {}",
                path.display(),
                error.message()
            ))
        })?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), path);
        let _ = credential;
        SignInChallenge::instructions(
            "Devin is signed in in the CLI; confirm to import its credentials.",
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
                "Devin sign-in completes once the CLI credentials are imported",
            ));
        };
        let Some(path) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Devin sign-in is in progress for this session",
            ));
        };
        let credential = creds::read_credentials(&path)?;
        discover::store(&credential, host, creds::account_id(&credential)).await
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
        creds::Stored::load(host, account).await?;
        Ok(())
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
        let _ = creds::Stored::load(host, account).await?;
        Err(PluginError::config(
            "Devin models come from `devin models list`; hofund does not run the CLI offline",
        ))
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::DevinUpstream::new(
            account.clone(),
            host.clone(),
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

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
