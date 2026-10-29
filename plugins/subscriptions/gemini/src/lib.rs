//! Gemini CLI (Google Code Assist) subscription.
//!
//! Signs in through the browser with PKCE as Gemini CLI does, on a loopback
//! port the system picks, sets the account up for Code Assist and keeps its
//! project, and holds the access token in memory only: Google does not
//! rotate the refresh token, so the stored secret never changes on refresh.
//! Code Assist requests are not served yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, HostContext, LoopbackSpec, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, RawValue, RedirectHost, Schema, SecretString,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, TokenCache, constant_time_eq, new_state};

mod project;
mod sign_in;
mod tokens;

/// The OAuth client Gemini CLI signs in as.
pub(crate) const CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// Gemini CLI's installed-app client secret. Google treats an installed
/// app's secret as public; this one ships inside Gemini CLI.
pub(crate) const CLIENT_SECRET: &str = "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";

/// The scopes Gemini CLI asks for, space-separated.
pub(crate) const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform \
     https://www.googleapis.com/auth/userinfo.email \
     https://www.googleapis.com/auth/userinfo.profile";

/// The Gemini CLI version Code Assist is told it talks to.
pub(crate) const GEMINI_CLI_VERSION: &str = "0.61.0";

/// The loopback callback path Gemini CLI listens on.
const CALLBACK_PATH: &str = "/oauth2callback";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("gemini"),
    "Gemini CLI (Code Assist)",
    SignInFlowKind::Pkce,
);

/// Registers the Gemini CLI subscription.
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
    /// The address that says which Google account signed in.
    pub userinfo: String,
    /// The Code Assist base; methods go to `{code_assist}/v1internal:{method}`.
    pub code_assist: String,
    /// How long onboarding waits before it asks again.
    pub onboard_poll: Duration,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            userinfo: "https://www.googleapis.com/oauth2/v2/userinfo?alt=json".to_owned(),
            code_assist: "https://cloudcode-pa.googleapis.com".to_owned(),
            onboard_poll: Duration::from_secs(2),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> GeminiFactory {
    GeminiFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Gemini CLI subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct GeminiFactory {
    endpoints: Arc<Endpoints>,
}

/// The validated config.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct GeminiConfig {
    /// The Google Cloud project to bill, for a Code Assist Standard or
    /// Enterprise account. `None` lets Code Assist pick.
    project: Option<String>,
}

impl ValidConfig for GeminiConfig {}

impl SubscriptionFactory for GeminiFactory {
    type Config = GeminiConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![("project".to_owned(), Schema::string())], &[])
            .expect("the project schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "project" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let project = match raw.get("project") {
            None => None,
            Some(RawValue::String(value)) => {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_owned())
            }
            Some(_) => {
                return Err(ConfigError::new("project must be a string").with_path("project"));
            }
        };
        Ok(GeminiConfig { project })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Gemini {
            endpoints: Arc::clone(&self.endpoints),
            project: cfg.project.clone(),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
            tokens: TokenCache::new(tokens::REFRESH_MARGIN),
        }))
    }
}

/// A browser sign-in waiting for its callback.
struct Pending {
    verifier: SecretString,
    state: String,
    redirect_uri: String,
}

struct Gemini {
    endpoints: Arc<Endpoints>,
    /// The project the config names, if any.
    project: Option<String>,
    flow: SignInFlow,
    /// Sign-ins in progress, by session id.
    pending: Mutex<HashMap<String, Pending>>,
    /// Access tokens, in memory only.
    tokens: TokenCache<SecretString>,
}

impl Gemini {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }

    fn access<'a>(
        &'a self,
        account: &'a AccountId,
        host: &'a HostContext,
    ) -> impl Future<Output = Result<SecretString, PluginError>> + 'a {
        tokens::access(host, &self.endpoints, &self.tokens, account)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Gemini {
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
        // Gemini CLI listens on 127.0.0.1 at a port the system picks.
        let spec = LoopbackSpec::ephemeral()
            .path(CALLBACK_PATH)
            .redirect_host(RedirectHost::Ipv4)
            .expect_state(state.clone());
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
                "no Gemini CLI sign-in is in progress for this session",
            ));
        };
        let SignInProof::Callback { code, state } = proof else {
            return Err(PluginError::config(
                "Gemini CLI sign-in completes from the browser callback",
            ));
        };
        if !constant_time_eq(state.as_bytes(), pending.state.as_bytes()) {
            return Err(PluginError::auth(
                "the sign-in callback does not belong to this sign-in",
            ));
        }
        let signed_in = sign_in::exchange(
            host,
            &self.endpoints,
            &code,
            &pending.verifier,
            &pending.redirect_uri,
            self.project.as_deref(),
        )
        .await?;
        let account = host
            .accounts()
            .account(signed_in.id.clone(), signed_in.label)?;
        host.accounts().upsert(account, signed_in.secret).await?;
        // A token cached for an earlier sign-in of this account is dropped.
        self.tokens.remove(&signed_in.id);
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
                "no Gemini CLI sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in("the stored Gemini CLI sign-in is unreadable"));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        if blob.secret("refresh_token").is_none() {
            return Ok(needs_sign_in(
                "the stored Gemini CLI sign-in has no refresh token",
            ));
        }
        if !project::known(&blob, self.project.as_deref()) {
            return Ok(AccountStatus::NeedsSignIn {
                reason: project::missing_reason(&blob),
            });
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        self.tokens.invalidate_if(account, |_| true).await;
        let token = self.access(account, host).await?;
        project::ensure(
            host,
            &self.endpoints,
            account,
            &token,
            self.project.as_deref(),
        )
        .await
    }

    async fn quota(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        Err(PluginError::unsupported(
            "Gemini Code Assist usage limits are not read yet",
        ))
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        Err(PluginError::unsupported(
            "the Gemini Code Assist model list is not read yet",
        ))
    }

    fn upstream(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Err(PluginError::unsupported(
            "Gemini Code Assist requests are not served yet",
        ))
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
