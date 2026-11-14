//! Google Antigravity subscription sign-in, account setup, and token refresh.
//!
//! Antigravity uses its own OAuth client, a localhost `/oauth-callback`, the
//! production Code Assist base for account discovery, and the daily base for
//! onboarding. Refresh tokens stay in the account store; access tokens do not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, HostContext, LoopbackSpec, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, RawValue, Schema, SecretString, SignInChallenge,
    SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory,
    SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, TokenCache, constant_time_eq, new_state};

mod client;
mod import;
mod models;
mod project;
mod quota;
mod sign_in;
mod tokens;
mod upstream;
mod version;

pub(crate) const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform \
     https://www.googleapis.com/auth/userinfo.email \
     https://www.googleapis.com/auth/userinfo.profile \
     https://www.googleapis.com/auth/cclog \
     https://www.googleapis.com/auth/experimentsandconfigs";

/// The account-suspension risk stated before sign-in.
pub const SIGN_IN_WARNING: &str = "Google may suspend an Antigravity account it sees used outside Antigravity. Use one you can afford to lose.";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("antigravity"),
    "Google Antigravity",
    SignInFlowKind::Pkce,
);

/// Registers the Antigravity subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Remote addresses used by the plugin. Tests replace them with recordings.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub userinfo: String,
    pub load_code_assist: String,
    pub code_assist: String,
    pub version_manifest: String,
    pub onboard_poll: Duration,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            userinfo: "https://www.googleapis.com/oauth2/v2/userinfo?alt=json".to_owned(),
            load_code_assist: "https://cloudcode-pa.googleapis.com".to_owned(),
            code_assist: "https://daily-cloudcode-pa.googleapis.com".to_owned(),
            version_manifest: "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml".to_owned(),
            onboard_poll: Duration::from_secs(2),
        }
    }
}

#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> AntigravityFactory {
    AntigravityFactory {
        endpoints: Arc::new(endpoints),
    }
}

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct AntigravityFactory {
    endpoints: Arc<Endpoints>,
}

#[doc(hidden)]
#[derive(Clone, Default)]
pub struct AntigravityConfig {
    project: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl std::fmt::Debug for AntigravityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntigravityConfig")
            .field("project", &self.project)
            .field("client_id", &self.client_id.as_ref().map(|_| "<set>"))
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<set>"),
            )
            .finish()
    }
}

impl ValidConfig for AntigravityConfig {}

impl SubscriptionFactory for AntigravityFactory {
    type Config = AntigravityConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![
                ("project".to_owned(), Schema::string()),
                (client::ID_KEY.to_owned(), Schema::string()),
                (client::SECRET_KEY.to_owned(), Schema::string()),
            ],
            &[],
        )
        .expect("the antigravity config schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        const KEYS: &[&str] = &["project", client::ID_KEY, client::SECRET_KEY];
        for key in raw.keys() {
            if !KEYS.contains(&key) {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        Ok(AntigravityConfig {
            project: optional_string(raw, "project")?,
            client_id: optional_string(raw, client::ID_KEY)?,
            client_secret: optional_string(raw, client::SECRET_KEY)?,
        })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let oauth = Arc::new(client::resolve(
            cfg.client_id.as_deref(),
            cfg.client_secret.as_deref(),
            client::env_lookup,
        )?);
        Ok(Arc::new(Antigravity {
            endpoints: Arc::clone(&self.endpoints),
            oauth,
            project: cfg.project.clone(),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
            tokens: Arc::new(TokenCache::new(tokens::REFRESH_MARGIN)),
            catalog: Arc::new(models::Catalog::default()),
        }))
    }
}

struct Pending {
    verifier: SecretString,
    state: String,
    redirect_uri: String,
}

struct Antigravity {
    endpoints: Arc<Endpoints>,
    oauth: Arc<client::OauthClient>,
    project: Option<String>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
    tokens: Arc<TokenCache<SecretString>>,
    catalog: Arc<models::Catalog>,
}

impl Antigravity {
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
        tokens::access(host, &self.endpoints, &self.oauth, &self.tokens, account)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Antigravity {
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
        let spec = LoopbackSpec::ephemeral()
            .path("/oauth-callback")
            .expect_state(state.clone());
        let binding = host.loopback().bind(ctx, &spec)?;
        let redirect_uri = binding.redirect_uri().to_owned();
        let url = sign_in::authorize(
            &self.endpoints.authorize,
            &redirect_uri,
            &pkce,
            &state,
            &self.oauth,
        );
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
                "no Antigravity sign-in is in progress for this session",
            ));
        };
        let SignInProof::Callback { code, state, .. } = proof else {
            return Err(PluginError::config(
                "Antigravity sign-in completes from the browser callback",
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
            &self.oauth,
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
        self.tokens.remove(&signed_in.id);
        self.catalog.forget(&signed_in.id);
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
                "no Antigravity sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in(
                "the stored Antigravity sign-in is unreadable",
            ));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        if blob.secret("refresh_token").is_none() {
            return Ok(needs_sign_in(
                "the stored Antigravity sign-in has no refresh token",
            ));
        }
        if !project::known(&blob, self.project.as_deref()) {
            return Ok(needs_sign_in(project::PENDING_REASON));
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        self.tokens.invalidate_if(account, |_| true).await;
        let token = self.access(account, host).await?;
        let secret = host.accounts().secret(account).await?;
        if secret.as_ref().is_some_and(|secret| {
            SecretObject::parse(secret)
                .is_ok_and(|blob| project::known(&blob, self.project.as_deref()))
        }) {
            return Ok(());
        }
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
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let models = self
            .catalog
            .read(
                host,
                &self.endpoints,
                &self.oauth,
                &self.tokens,
                account,
                self.project.as_deref(),
            )
            .await?;
        let mut windows = quota::model_windows(&models)?;
        let access = self.access(account, host).await?;
        let project = project::resolve(
            host,
            &self.endpoints,
            account,
            &access,
            self.project.as_deref(),
        )
        .await?;
        windows.extend(quota::summary_windows(host, &self.endpoints, &access, &project).await);
        Ok(windows)
    }

    async fn set_project(
        &self,
        email: &str,
        project_id: &str,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        let account = project::set(host, email, project_id).await?;
        self.catalog.forget_project(&account);
        Ok(())
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        self.catalog
            .pool(
                host,
                &self.endpoints,
                &self.oauth,
                &self.tokens,
                self.project.as_deref(),
            )
            .await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::AntigravityUpstream {
            account: account.clone(),
            host: host.clone(),
            endpoints: Arc::clone(&self.endpoints),
            oauth: Arc::clone(&self.oauth),
            tokens: Arc::clone(&self.tokens),
            catalog: Arc::clone(&self.catalog),
            project: self.project.clone(),
        }))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        let imported = import::store(
            host,
            &self.endpoints,
            &self.oauth,
            text,
            self.project.as_deref(),
        )
        .await?;
        for id in &imported {
            self.tokens.remove(id);
            self.catalog.forget(id);
        }
        Ok(imported)
    }
}

fn optional_string(raw: &RawTable, key: &str) -> Result<Option<String>, ConfigError> {
    match raw.get(key) {
        None => Ok(None),
        Some(RawValue::String(value)) => {
            let value = value.trim();
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
        Some(_) => Err(ConfigError::new(format!("{key} must be a string")).with_path(key)),
    }
}

fn needs_sign_in(reason: &str) -> AccountStatus {
    AccountStatus::NeedsSignIn {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod r49d_tests;
#[cfg(test)]
mod r49e_tests;
#[cfg(test)]
mod tests;
