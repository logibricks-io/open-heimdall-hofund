//! ChatGPT (Codex) subscription.
//!
//! Signs in through the browser with PKCE on the loopback port Codex CLI
//! registers, keeps the token set as one account secret, refreshes the
//! access token under the account lease, and serves the ChatGPT Codex
//! backend as an OpenAI Responses upstream. The usage windows each reply
//! reports become quota observations, and `quota` reads `/wham/usage`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HostContext, LoopbackSpec,
    ModelList, PluginError, PluginId, QuotaWindow, RawTable, RedirectHost, Schema, SecretString,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, constant_time_eq, new_state};

mod agent_sign_in;
mod limits;
mod models;
mod shape;
mod sign_in;
mod tokens;
mod upstream;
mod usage;

/// The OAuth client Codex CLI signs in as.
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The scopes asked for at sign-in.
pub(crate) const SIGN_IN_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// The scopes asked for at refresh, as Codex CLI sends them.
pub(crate) const REFRESH_SCOPE: &str = "openid profile email";

/// The Codex CLI version the backend is told it talks to.
pub(crate) const CODEX_VERSION: &str = "0.159.0";

/// The fixed loopback port Codex CLI's client is registered with.
const LOOPBACK_PORT: u16 = 1455;

/// The loopback callback path Codex CLI's client is registered with.
const CALLBACK_PATH: &str = "/auth/callback";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("codex"),
    "ChatGPT (Codex)",
    SignInFlowKind::Pkce,
)
.with_agent_sign_in(AgentSignIn::new(&["codex"], "CODEX_HOME", ".codex"));

/// Registers the ChatGPT subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Where the plugin signs in and sends requests. Tests point it at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The OAuth authorize page.
    pub authorize: String,
    /// The OAuth token endpoint.
    pub token: String,
    /// The Codex backend base; requests go to `{base}/responses`.
    pub base: String,
    /// The loopback port, or `None` for one the system picks.
    pub port: Option<u16>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: "https://auth.openai.com/oauth/authorize".to_owned(),
            token: "https://auth.openai.com/oauth/token".to_owned(),
            base: "https://chatgpt.com/backend-api/codex".to_owned(),
            port: Some(LOOPBACK_PORT),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> CodexFactory {
    CodexFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds ChatGPT subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct CodexFactory {
    endpoints: Arc<Endpoints>,
}

/// The validated config: the subscription takes no options yet.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct CodexConfig;

impl ValidConfig for CodexConfig {}

impl SubscriptionFactory for CodexFactory {
    type Config = CodexConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(Vec::new(), &[]).expect("an empty object schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        if let Some(key) = raw.keys().next() {
            return Err(ConfigError::new("unknown key").with_path(key));
        }
        Ok(CodexConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Codex {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
            prompts: Arc::default(),
        }))
    }
}

/// A browser sign-in waiting for its callback.
struct Pending {
    verifier: SecretString,
    state: String,
    redirect_uri: String,
}

struct Codex {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    /// Sign-ins in progress, by session id.
    pending: Mutex<HashMap<String, Pending>>,
    /// The instructions each model is sent with.
    prompts: Arc<models::Prompts>,
}

impl Codex {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Codex {
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
        let pkce = Pkce::generate()?;
        let state = new_state()?;
        let spec = match self.endpoints.port {
            Some(port) => LoopbackSpec::fixed(port),
            None => LoopbackSpec::ephemeral(),
        }
        .path(CALLBACK_PATH)
        .redirect_host(RedirectHost::Localhost)
        .expect_state(state.clone());
        // A blocked port still yields a redirect address: the sign-in then
        // completes from the address the user pastes back.
        let binding = host.loopback().bind(ctx, &spec)?;
        let redirect_uri = binding.redirect_uri().to_owned();
        let url = sign_in::authorize(&self.endpoints.authorize, &redirect_uri, &pkce, &state);
        let url = match url {
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
                Pending {
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
                "no ChatGPT sign-in is in progress for this session",
            ));
        };
        let SignInProof::Callback { code, state } = proof else {
            return Err(PluginError::config(
                "ChatGPT sign-in completes from the browser callback",
            ));
        };
        if !constant_time_eq(state.as_bytes(), pending.state.as_bytes()) {
            return Err(PluginError::auth(
                "the sign-in callback does not belong to this sign-in",
            ));
        }
        let signed_in = sign_in::exchange(
            host,
            &self.endpoints.token,
            &code,
            &pending.verifier,
            &pending.redirect_uri,
        )
        .await?;
        let account = host
            .accounts()
            .account(signed_in.id.clone(), signed_in.label)?;
        host.accounts().upsert(account, signed_in.secret).await?;
        Ok(signed_in.id)
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
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(needs_sign_in(
                "no ChatGPT sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in("the stored ChatGPT sign-in is unreadable"));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        if blob.secret("access_token").is_none() || blob.secret("refresh_token").is_none() {
            return Ok(needs_sign_in("the stored ChatGPT sign-in has no tokens"));
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        tokens::access(host, &self.endpoints, account, tokens::Need::Always)
            .await
            .map(|_| ())
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let access =
            tokens::access(host, &self.endpoints, account, tokens::Need::IfExpiring).await?;
        usage::read(host, &self.endpoints, &access).await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let access =
            tokens::access(host, &self.endpoints, account, tokens::Need::IfExpiring).await?;
        models::list(host, &self.endpoints, &access, &self.prompts).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::CodexUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
            Arc::clone(&self.prompts),
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
