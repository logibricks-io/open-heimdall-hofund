//! Subscription sign-in and the upstream a selected account becomes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{BuildError, ConfigError, RawTable, Schema, ValidConfig};
use crate::error::PluginError;
use crate::host::HostContext;
use crate::id::{AccountId, PluginId};
use crate::records::{
    AccountStatus, ModelList, QuotaWindow, SignInChallenge, SignInCtx, SignInFlow, SignInProof,
    SubscriptionSpec,
};
use crate::upstream::UpstreamProvider;

/// Builds a [`Subscription`] from validated config.
pub trait SubscriptionFactory: Send + Sync + 'static {
    /// The validated config.
    type Config: ValidConfig;

    /// Static metadata.
    fn spec(&self) -> &'static SubscriptionSpec;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Checks `raw` before any subscription exists.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when `raw` is not valid for this plugin.
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;

    /// Builds the subscription.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the subscription cannot be constructed.
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError>;
}

/// The factory the registry stores.
pub trait DynSubscriptionFactory: Send + Sync + 'static {
    /// Static metadata.
    fn spec(&self) -> &'static SubscriptionSpec;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Validates `raw` and builds the subscription.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when validation or construction fails, or the
    /// built instance's [`SignInFlow::kind`] differs from the spec's flow kind.
    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError>;
}

impl<F> DynSubscriptionFactory for F
where
    F: SubscriptionFactory,
{
    fn spec(&self) -> &'static SubscriptionSpec {
        SubscriptionFactory::spec(self)
    }

    fn config_schema(&self) -> Schema {
        SubscriptionFactory::config_schema(self)
    }

    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let cfg = self.validate(raw)?;
        let built = self.build(&cfg, host)?;
        let spec = SubscriptionFactory::spec(self);
        if built.flow().kind() != spec.flow() {
            return Err(BuildError::new(format!(
                "subscription {} built a {:?} sign-in flow but its spec declares {:?}",
                spec.plugin(),
                built.flow().kind(),
                spec.flow()
            )));
        }
        Ok(built)
    }
}

/// One subscription plugin. The instance does not own the account list.
#[async_trait]
pub trait Subscription: Send + Sync + 'static {
    /// The plugin id.
    fn id(&self) -> &PluginId;

    /// How the UI should sign in. The HTTP steps stay in the plugin.
    fn flow(&self) -> &SignInFlow;

    /// Starts sign-in.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the plugin cannot start.
    async fn start_sign_in(
        &self,
        ctx: &SignInCtx,
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError>;

    /// Finishes sign-in, stores the account, and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the proof is rejected or the store write fails.
    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError>;

    /// Stops a sign-in started with `ctx`, such as a loopback listener or a
    /// device-code poll. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the plugin cannot stop the sign-in.
    async fn cancel_sign_in(&self, ctx: &SignInCtx, host: &HostContext) -> Result<(), PluginError> {
        let _ = (ctx, host);
        Ok(())
    }

    /// Whether `account` can serve calls. The default is [`AccountStatus::Ready`].
    ///
    /// A plugin returns [`AccountStatus::NeedsSignIn`] when a refresh found
    /// the grant revoked, so the UI asks for a new sign-in.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the status cannot be read.
    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        let _ = (account, host);
        Ok(AccountStatus::Ready)
    }

    /// Rotates the secret through `host.accounts()`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when refresh fails.
    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError>;

    /// Polls quota windows for one account.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the poll fails.
    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError>;

    /// Lists models for one account.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the list fails.
    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError>;

    /// The upstream for this account.
    ///
    /// Later `execute` calls read the current secret from `host`. This method
    /// does not capture an [`Account`](crate::Account) value.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the account is not in the store.
    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError>;

    /// Whether the subscription can take a call. The default is success.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the subscription should be left out.
    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Stops background work before `deadline`. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when shutdown fails.
    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }

    /// Writes a stored account into the agent's own sign-in files under `home`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the account is missing or the write fails.
    async fn write_agent_sign_in(
        &self,
        account: &AccountId,
        home: &std::path::Path,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        let _ = (account, home, host);
        Err(PluginError::unsupported(
            "this subscription does not switch an agent sign-in",
        ))
    }

    /// Imports accounts from an export file. [`PluginErrorKind::Unsupported`](crate::PluginErrorKind)
    /// means this subscription does not read that file.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the file is this subscription's format and cannot be stored.
    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        let _ = (text, host);
        Err(PluginError::unsupported(
            "this subscription does not import accounts from a file",
        ))
    }

    /// Sends the smallest vendor request that starts a rolling allowance window.
    ///
    /// The daemon calls this only when daily warm-up is configured and due.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the warm-up cannot run. Unsupported plugins
    /// keep the default error.
    async fn daily_warmup(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        let _ = (account, host);
        Err(PluginError::unsupported(
            "this subscription does not support daily warm-up",
        ))
    }
}
