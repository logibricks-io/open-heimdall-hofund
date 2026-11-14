//! Factory (Droid) subscription.
//!
//! Signs in with WorkOS device codes or a Factory API key, keeps OAuth tokens
//! refreshed, reads Factory plan limits and credit balance, lists models from
//! Factory's feeds, and serves chat completions on Factory's OpenAI-compatible
//! gateway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, DeviceFlow, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::SecretObject;

mod anthropic_tools;
mod api;
mod client;
mod discover;
mod models;
mod quota;
mod sign_in;
mod tokens;
mod upstream;

/// WorkOS client id Factory's apps use in production.
pub(crate) const WORKOS_CLIENT_ID: &str = "client_01HNM792M5G5G1A2THWPXKFMXB";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("factory"),
    "Factory",
    SignInFlowKind::Device,
);

/// Registers the Factory subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for Factory and WorkOS. Tests aim these at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Factory API origin (`https://api.factory.ai`).
    pub api: String,
    /// Factory EU API origin.
    pub api_eu: String,
    /// WorkOS device authorization URL.
    pub workos_device: String,
    /// WorkOS token URL for device grant polling.
    pub workos_token: String,
}

impl Endpoints {
    pub(crate) fn api_base(&self) -> &str {
        self.api.as_str()
    }

    pub(crate) fn api_base_for_region(&self, region: Option<&str>) -> &str {
        if region.is_some_and(|value| value.eq_ignore_ascii_case("eu")) {
            self.api_eu.as_str()
        } else {
            self.api.as_str()
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.factory.ai".to_owned(),
            api_eu: "https://api.eu.factory.ai".to_owned(),
            workos_device: "https://api.workos.com/user_management/authorize/device".to_owned(),
            workos_token: "https://api.workos.com/user_management/authenticate".to_owned(),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> FactoryFactory {
    FactoryFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Factory subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct FactoryFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct FactoryConfig;

impl ValidConfig for FactoryConfig {}

impl SubscriptionFactory for FactoryFactory {
    type Config = FactoryConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(Vec::new(), &[]).expect("empty object schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        if !raw.is_empty() {
            let key = raw.keys().next().expect("non-empty");
            return Err(ConfigError::new("unknown key").with_path(key));
        }
        Ok(FactoryConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let flow =
            DeviceFlow::new("app.factory.ai").map_err(|error| BuildError::new(error.message()))?;
        Ok(Arc::new(Factory {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Device(flow),
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Pending {
    device: sign_in::DevicePending,
}

struct Factory {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Factory {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Factory {
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
        let pending = sign_in::device_start(host, &self.endpoints).await?;
        let page = sign_in::verification_page(&pending);
        let challenge = SignInChallenge::device_code(pending.start.user_code().to_owned(), page)?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), Pending { device: pending });
        Ok(challenge)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        match proof {
            SignInProof::Token(key) => {
                let signed_in = sign_in::import_api_key(host, &self.endpoints, key).await?;
                let account = host
                    .accounts()
                    .account(signed_in.id.clone(), signed_in.label)?;
                host.accounts().upsert(account, signed_in.secret).await?;
                Ok(signed_in.id)
            }
            SignInProof::Await => {
                let Some(pending) = self.take_pending(ctx.session()) else {
                    return Err(PluginError::config(
                        "no Factory sign-in is in progress for this session",
                    ));
                };
                let signed_in =
                    sign_in::device_finish(host, &self.endpoints, &pending.device).await?;
                let account = host
                    .accounts()
                    .account(signed_in.id.clone(), signed_in.label)?;
                host.accounts().upsert(account, signed_in.secret).await?;
                Ok(signed_in.id)
            }
            _ => Err(PluginError::config(
                "Factory sign-in completes after the device code is approved or with an API key",
            )),
        }
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
                "no Factory sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in("the stored Factory sign-in is unreadable"));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        match tokens::kind(&blob) {
            tokens::AuthKind::Api => {
                if blob.secret("api_key").is_none() {
                    return Ok(needs_sign_in("the stored Factory sign-in has no API key"));
                }
            }
            tokens::AuthKind::Oauth => {
                if blob.secret("refresh_token").is_none() {
                    return Ok(needs_sign_in(
                        "the stored Factory sign-in has no refresh token",
                    ));
                }
            }
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        tokens::refresh_account(host, &self.endpoints, account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let bearer = tokens::bearer(host, &self.endpoints, account).await?;
        let limits = api::get(
            host,
            self.endpoints.api_base_for_region(bearer.region.as_deref()),
            "/api/billing/limits",
            Some(&bearer.token),
            bearer.active_org.as_deref(),
            true,
        )
        .await;
        match limits {
            Ok(value) => quota::windows_from_limits(&value),
            Err(error) if error.status() == Some(404) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let _ = tokens::bearer(host, &self.endpoints, account).await?;
        models::catalog_at(host.clock().now())
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::FactoryUpstream::new(
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
        discover::import_accounts(host, text).await
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
