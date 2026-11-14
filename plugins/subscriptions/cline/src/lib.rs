//! Cline (cline.bot) subscription.
//!
//! Signs in with WorkOS device codes or a Cline API key, keeps OAuth tokens
//! refreshed, reads ClinePass limits and credit balance, lists models from
//! Cline's feeds, and serves chat completions on Cline's OpenAI-compatible
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

mod api;
mod client;
mod models;
mod quota;
mod sign_in;
mod tokens;
mod upstream;

/// WorkOS client id Cline's apps use in production.
pub(crate) const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("cline"),
    "Cline",
    SignInFlowKind::Device,
);

/// Registers the Cline subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for Cline and WorkOS. Tests aim these at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Cline API base, including `/api/v1`.
    pub api: String,
    /// WorkOS device authorization URL.
    pub workos_device: String,
    /// WorkOS token URL for device grant polling.
    pub workos_token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: "https://api.cline.bot/api/v1".to_owned(),
            workos_device: "https://api.workos.com/user_management/authorize/device".to_owned(),
            workos_token: "https://api.workos.com/user_management/authenticate".to_owned(),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> ClineFactory {
    ClineFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Cline subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ClineFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ClineConfig;

impl ValidConfig for ClineConfig {}

impl SubscriptionFactory for ClineFactory {
    type Config = ClineConfig;

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
        Ok(ClineConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        let flow =
            DeviceFlow::new("app.cline.bot").map_err(|error| BuildError::new(error.message()))?;
        Ok(Arc::new(Cline {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Device(flow),
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Pending {
    device: sign_in::DevicePending,
}

struct Cline {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Cline {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Cline {
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
                let session = ctx.session().to_string();
                let device = {
                    let guard = self
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(pending) = guard.get(&session) else {
                        return Err(PluginError::config(
                            "no Cline sign-in is in progress for this session",
                        ));
                    };
                    pending.device.clone()
                };
                let signed_in = match sign_in::device_finish(host, &self.endpoints, &device, || {
                    self.pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&session)
                })
                .await
                {
                    Ok(signed_in) => signed_in,
                    Err(error) => {
                        let _ = self.take_pending(&session);
                        return Err(error);
                    }
                };
                let _ = self.take_pending(&session);
                let account = host
                    .accounts()
                    .account(signed_in.id.clone(), signed_in.label)?;
                host.accounts().upsert(account, signed_in.secret).await?;
                Ok(signed_in.id)
            }
            _ => Err(PluginError::config(
                "Cline sign-in completes after the device code is approved or with an API key",
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
            return Ok(needs_sign_in("no Cline sign-in is stored for this account"));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in("the stored Cline sign-in is unreadable"));
        };
        if blob.flag(tokens::LAPSED) {
            return Ok(needs_sign_in(tokens::LAPSED_REASON));
        }
        match tokens::kind(&blob) {
            tokens::AuthKind::Api => {
                if blob.secret("api_key").is_none() {
                    return Ok(needs_sign_in("the stored Cline sign-in has no API key"));
                }
            }
            tokens::AuthKind::Oauth => {
                if blob.secret("refresh_token").is_none() {
                    return Ok(needs_sign_in(
                        "the stored Cline sign-in has no refresh token",
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
            &self.endpoints,
            "/users/me/plan/usage-limits",
            Some(&bearer.token),
            false,
        )
        .await;
        let me = api::get(
            host,
            &self.endpoints,
            "/users/me",
            Some(&bearer.token),
            true,
        )
        .await
        .ok();
        let balance = read_balance(host, &self.endpoints, &bearer, me.as_ref()).await;
        let limits_value = match limits {
            Ok(value) => Some(value),
            Err(error) if error.status() == Some(404) => None,
            Err(error) => {
                let usage = quota::assemble(me.as_ref(), balance.as_ref(), None);
                if usage.windows.is_empty() {
                    return Err(error);
                }
                return Ok(usage.windows);
            }
        };
        let usage = quota::assemble(me.as_ref(), balance.as_ref(), limits_value.as_ref());
        let mut windows = usage.windows;
        if let Some(balance) = usage.balance
            && !windows
                .iter()
                .any(|window| window.name().starts_with("Balance:"))
        {
            windows.push(quota::balance_window(&balance)?);
        }
        if windows.is_empty() {
            return Err(PluginError::config(
                "Cline reported neither plan limits nor a balance",
            ));
        }
        Ok(windows)
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let bearer = tokens::bearer(host, &self.endpoints, account).await?;
        models::list(host, &self.endpoints, &bearer).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::ClineUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
        )))
    }
}

async fn read_balance(
    host: &HostContext,
    endpoints: &Endpoints,
    bearer: &tokens::Bearer,
    me: Option<&hofund_plugin_sdk::json::Value>,
) -> Option<hofund_plugin_sdk::json::Value> {
    let ids = me
        .map(|value| {
            ["clineUserId", "subject", "id"]
                .iter()
                .filter_map(|key| value.get(*key).and_then(|v| v.as_str()))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for id in ids {
        let path = format!("/users/{}/balance", urlencoding(&id));
        if let Ok(value) = api::get(host, endpoints, &path, Some(&bearer.token), false).await {
            return Some(value);
        }
    }
    None
}

fn urlencoding(id: &str) -> String {
    id.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
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
