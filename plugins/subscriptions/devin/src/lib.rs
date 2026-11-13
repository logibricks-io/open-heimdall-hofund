//! Devin subscription plugin: import CLI credentials, models, and quota.
//! Subscription chat is not implemented.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HostContext, LoopbackSpec,
    ModelList, PluginError, PluginId, QuotaWindow, RawTable, RedirectHost, Schema, SignInChallenge,
    SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory,
    SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{constant_time_eq, secret_text};

mod agent_sign_in;
mod catalog;
mod cli;
mod connect;
mod creds;
mod discover;
mod errors;
mod models;
mod quota;
mod sign_in;
mod snapshot_families;
mod stream;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("devin"),
    "Devin",
    SignInFlowKind::Pkce,
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
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Devin {
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::Pending>>,
}

impl Devin {
    fn take_pending(&self, session: &str) -> Option<sign_in::Pending> {
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
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        drop(self.take_pending(ctx.session()));
        host.loopback().release(ctx);
        let (pkce, state) = sign_in::generate()?;
        let spec = LoopbackSpec::ephemeral()
            .path("/callback")
            .redirect_host(RedirectHost::Ipv4)
            .expect_state(state.clone());
        let binding = host.loopback().bind(ctx, &spec)?;
        let redirect_uri = binding.redirect_uri().to_owned();
        let url = match sign_in::authorize(&redirect_uri, &pkce, &state) {
            Ok(url) => url,
            Err(error) => {
                host.loopback().release(ctx);
                return Err(error);
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                ctx.session().to_owned(),
                sign_in::Pending {
                    verifier: pkce.verifier,
                    state,
                    redirect_uri,
                },
            );
        SignInChallenge::open_url(url)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let pending = self.take_pending(ctx.session());
        host.loopback().release(ctx);
        let Some(pending) = pending else {
            return Err(PluginError::config(
                "no Devin sign-in is in progress for this session",
            ));
        };
        let SignInProof::Callback { code, state } = proof else {
            return Err(PluginError::config(
                "Devin sign-in completes from the browser callback",
            ));
        };
        if secret_text(&code).is_empty() {
            return Err(PluginError::config("the sign-in callback has no code"));
        }
        if !constant_time_eq(state.as_bytes(), pending.state.as_bytes()) {
            return Err(PluginError::auth(
                "the sign-in callback does not belong to this sign-in",
            ));
        }
        let key = sign_in::exchange(host, &code, &pending).await?;
        let credential = creds::Credential {
            key,
            api_server: sign_in::SERVER.to_owned(),
            devin_api: "https://api.devin.ai".to_owned(),
            label: "Devin".to_owned(),
            plan: String::new(),
            org_id: String::new(),
            team_id: String::new(),
        };
        let credential = sign_in::success(host, &credential).await?;
        discover::store(&credential, host, creds::account_id(&credential)).await
    }

    async fn cancel_sign_in(&self, ctx: &SignInCtx, host: &HostContext) -> Result<(), PluginError> {
        let _ = self.take_pending(ctx.session());
        host.loopback().release(ctx);
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        match creds::Stored::load(host, account).await {
            Ok(stored) if stored.needs_sign_in => Ok(AccountStatus::NeedsSignIn {
                reason: "Devin refused the stored sign-in; sign in again".to_owned(),
            }),
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
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        quota::read(host, account).await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let stored = creds::Stored::load(host, account).await?;
        catalog::list_models(host, account, &stored).await
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
mod identity_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod pkce_tests;
#[cfg(test)]
mod tests;
