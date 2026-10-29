//! GitHub Copilot subscription.
//!
//! Signs in with the GitHub device flow on github.com or a `<name>.ghe.com`
//! enterprise, stores the OAuth token as one account secret, trades it for
//! short-lived Copilot session tokens, reads the Copilot quota, and serves
//! the Copilot API as an OpenAI chat completions upstream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, DeviceFlow, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, RawValue, Schema, SignInChallenge, SignInCtx,
    SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::TokenCache;
use hofund_plugin_sdk::oauth::device::DeviceStart;

mod quota;
mod session;
mod sign_in;
mod site;
mod upstream;

use session::Session;
use site::Site;

/// The sign-in option and config key naming the GitHub host.
const HOST_KEY: &str = "host";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("copilot"),
    "GitHub Copilot",
    SignInFlowKind::Device,
);

/// Registers the GitHub Copilot subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(CopilotFactory)
}

/// Builds GitHub Copilot subscription instances.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct CopilotFactory;

/// The validated config: the GitHub host new sign-ins use.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct CopilotConfig {
    site: Site,
}

impl ValidConfig for CopilotConfig {}

impl SubscriptionFactory for CopilotFactory {
    type Config = CopilotConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![(HOST_KEY.to_owned(), Schema::string())], &[])
            .expect("a one-field object schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        if let Some(key) = raw.keys().find(|key| *key != HOST_KEY) {
            return Err(ConfigError::new("unknown key").with_path(key));
        }
        let site = match raw.get(HOST_KEY) {
            None => Site::github(),
            Some(RawValue::String(host)) => Site::parse(host)
                .map_err(|error| ConfigError::new(error.message()).with_path(HOST_KEY))?,
            Some(_) => return Err(ConfigError::new("expected a string").with_path(HOST_KEY)),
        };
        Ok(CopilotConfig { site })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let flow = DeviceFlow::new(cfg.site.name())
            .and_then(|flow| flow.with_path("/login/device"))
            .map_err(|error| BuildError::new(error.message()))?;
        Ok(Arc::new(Copilot {
            site: cfg.site.clone(),
            flow: SignInFlow::Device(flow),
            pending: Mutex::new(HashMap::new()),
            sessions: Arc::new(TokenCache::new(session::MARGIN)),
        }))
    }
}

/// A device sign-in waiting for the user.
struct Pending {
    site: Site,
    start: DeviceStart,
}

struct Copilot {
    /// The host new sign-ins use unless the sign-in names another.
    site: Site,
    flow: SignInFlow,
    /// Sign-ins in progress, by session id.
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Arc<TokenCache<Session>>,
}

impl Copilot {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Copilot {
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
        let site = match ctx.option(HOST_KEY) {
            Some(text) => Site::parse(text)?,
            None => self.site.clone(),
        };
        let start = sign_in::start(host, &site).await?;
        // The page is derived from the host the user chose, not taken from
        // the reply, so a reply cannot send the user elsewhere.
        let challenge = SignInChallenge::device_code(start.user_code(), site.device_page())?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), Pending { site, start });
        Ok(challenge)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let Some(pending) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Copilot sign-in is in progress for this session",
            ));
        };
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Copilot sign-in completes once the code is entered on GitHub",
            ));
        };
        let signed_in = sign_in::finish(host, &pending.site, &pending.start).await?;
        let account = host
            .accounts()
            .account(signed_in.id.clone(), signed_in.label)?;
        host.accounts().upsert(account, signed_in.secret).await?;
        self.sessions.remove(&signed_in.id);
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
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(needs_sign_in(
                "no Copilot sign-in is stored for this account",
            ));
        };
        match session::read_blob(&secret) {
            Ok(_) => Ok(AccountStatus::Ready),
            Err(error) => Ok(needs_sign_in(error.message())),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        self.sessions.remove(account);
        session::session(&self.sessions, host, account)
            .await
            .map(|_| ())
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = session::stored(host, account).await?;
        let reply = session::github_get(stored.site.copilot_user_url(), &stored.oauth)
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => quota::windows(reply.body()),
            401 | 403 => Err(PluginError::auth(
                "GitHub refused to show the Copilot quota",
            )),
            status => Err(PluginError::status_error(
                status,
                "GitHub did not return the Copilot quota",
            )),
        }
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        Err(PluginError::unsupported(
            "the Copilot model list is not read yet",
        ))
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::CopilotUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.sessions),
        )))
    }
}

fn needs_sign_in(reason: &str) -> AccountStatus {
    AccountStatus::NeedsSignIn {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
