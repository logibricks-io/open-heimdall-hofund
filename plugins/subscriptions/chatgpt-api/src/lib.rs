//! ChatGPT API subscription (Sign in with ChatGPT).
//!
//! Signs in through OpenAI's dynamic client flow, keeps the per-account client
//! id and tokens, refreshes before expiry, and serves `api.openai.com` as an
//! OpenAI Responses upstream with the agent's own instructions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, HostContext, LoopbackSpec, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, RedirectHost, Schema, SecretString,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, constant_time_eq, new_state};

mod errors;
mod host_id;
mod models;
mod quota;
mod revoke;
mod shape;
mod sign_in;
mod tokens;
mod upstream;

const LOOPBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("chatgpt-api"),
    "ChatGPT API",
    SignInFlowKind::Pkce,
);

/// Registers the ChatGPT API subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Where the plugin signs in and sends requests. Tests may point these at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub api: String,
    pub port: Option<u16>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            issuer: "https://auth.openai.com".to_owned(),
            authorize: "https://auth.openai.com/api/accounts/authorize".to_owned(),
            token: "https://auth.openai.com/api/accounts/oauth/token".to_owned(),
            api: "https://api.openai.com/v1".to_owned(),
            port: Some(LOOPBACK_PORT),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> ChatgptApiFactory {
    ChatgptApiFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds ChatGPT API subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ChatgptApiFactory {
    endpoints: Arc<Endpoints>,
}

/// The validated config: the subscription takes no options yet.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ChatgptApiConfig;

impl ValidConfig for ChatgptApiConfig {}

impl SubscriptionFactory for ChatgptApiFactory {
    type Config = ChatgptApiConfig;

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
        Ok(ChatgptApiConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(ChatgptApi {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
            models: Arc::new(models::Cache::default()),
        }))
    }
}

struct Pending {
    verifier: SecretString,
    state: String,
    redirect_uri: String,
    nonce: String,
    host_id: String,
}

struct ChatgptApi {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
    models: Arc<models::Cache>,
}

impl ChatgptApi {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for ChatgptApi {
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
        let nonce = sign_in::new_nonce()?;
        let host_id = host_id::load(host)?;
        let spec = match self.endpoints.port {
            Some(port) => LoopbackSpec::fixed(port),
            None => LoopbackSpec::ephemeral(),
        }
        .path(CALLBACK_PATH)
        .redirect_host(RedirectHost::Ipv4)
        .expect_state(state.clone());
        let binding = host.loopback().bind(ctx, &spec)?;
        let redirect_uri = binding.redirect_uri().to_owned();
        let url = sign_in::authorize(
            &self.endpoints.authorize,
            &redirect_uri,
            &pkce,
            &state,
            &nonce,
            &host_id,
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
                    nonce,
                    host_id,
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
        let Some(pending) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no ChatGPT API sign-in is in progress for this session",
            ));
        };
        host.loopback().release(ctx);
        let SignInProof::Callback {
            code,
            state,
            issued_client_id,
        } = proof
        else {
            return Err(PluginError::config(
                "ChatGPT API sign-in completes from the browser callback",
            ));
        };
        if !constant_time_eq(state.as_bytes(), pending.state.as_bytes()) {
            return Err(PluginError::auth(
                "the sign-in callback does not belong to this sign-in",
            ));
        }
        let client_id = issued_client_id.unwrap_or_default();
        let signed_in = sign_in::exchange(
            host,
            &sign_in::Exchange {
                token_url: &self.endpoints.token,
                client_id: &client_id,
                code: &code,
                verifier: &pending.verifier,
                redirect_uri: &pending.redirect_uri,
                nonce: &pending.nonce,
                host_id: &pending.host_id,
            },
        )
        .await?;
        let stored_secret = signed_in.secret.clone();
        let prefetched = SecretObject::parse(&stored_secret).ok().and_then(|blob| {
            blob.secret("access_token")
                .map(|token| tokens::Access { token })
        });
        let account = host
            .accounts()
            .account(signed_in.id.clone(), signed_in.label)?;
        host.accounts().upsert(account, stored_secret).await?;
        if let Some(access) = prefetched {
            let _ = models::list(host, &self.endpoints, &access, &self.models, &signed_in.id).await;
        }
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
                "no ChatGPT API sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in(
                "the stored ChatGPT API sign-in is unreadable",
            ));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        if blob.secret("refresh_token").is_none() || blob.text("client_id").is_none() {
            return Ok(needs_sign_in(
                "the stored ChatGPT API sign-in is incomplete; sign in again",
            ));
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
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        quota::windows()
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let access =
            tokens::access(host, &self.endpoints, account, tokens::Need::IfExpiring).await?;
        models::list(host, &self.endpoints, &access, &self.models, account).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::ChatgptApiUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
        )))
    }

    async fn remove_account(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(());
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(());
        };
        revoke::revoke(host, &self.endpoints, &blob).await
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
