//! WorkBuddy subscription plugin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod auth_store;
mod checkin;
mod creds;
mod discover;
mod errors;
mod models;
mod quota;
mod refresh;
mod sign_in;
mod site;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("workbuddy"),
    "WorkBuddy",
    SignInFlowKind::TokenFile,
)
.with_daily_checkin(true)
.with_agent_sign_in(AgentSignIn::new(
    &["workbuddy", "workbuddy-ai"],
    "HOFUND_CODEBUDDY_EXTENSION_DATA",
    auth_store::AGENT_HOME_DIR,
));

/// Registers the WorkBuddy subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_extension_data(Endpoints::default(), None))
}

/// HTTP endpoints for China and international WorkBuddy builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// China API origin (`https://copilot.tencent.com`).
    pub cn: String,
    /// International API origin (`https://www.workbuddy.ai`).
    pub ai: String,
}

impl Endpoints {
    pub(crate) fn base_for(&self, site: site::Site) -> &str {
        match site {
            site::Site::China => self.cn.as_str(),
            site::Site::International => self.ai.as_str(),
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            cn: site::Site::China.endpoint().to_owned(),
            ai: site::Site::International.endpoint().to_owned(),
        }
    }
}

/// Builds the factory against `endpoints`.
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> WorkBuddyFactory {
    factory_with_extension_data(endpoints, None)
}

/// Builds the factory with the legacy optional desktop data root ignored.
#[must_use]
pub fn factory_with_extension_data(
    endpoints: Endpoints,
    _extension_data: Option<PathBuf>,
) -> WorkBuddyFactory {
    WorkBuddyFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds WorkBuddy subscription instances.
#[derive(Clone, Debug)]
pub struct WorkBuddyFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[derive(Clone, Copy, Debug)]
pub struct WorkBuddyConfig;

impl ValidConfig for WorkBuddyConfig {}

impl SubscriptionFactory for WorkBuddyFactory {
    type Config = WorkBuddyConfig;

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
        Ok(WorkBuddyConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(WorkBuddy {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::TokenFile,
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct WorkBuddy {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::Pending>>,
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for WorkBuddy {
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
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        let (pending, challenge) = sign_in::start(ctx, host, &self.endpoints).await?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), pending);
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
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(ctx.session());
                sign_in::import_api_key(ctx, host, key).await
            }
            SignInProof::Await => {
                let session = ctx.session();
                let pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(session)
                    .cloned()
                    .ok_or_else(|| {
                        PluginError::config("no sign-in is in progress for this session")
                    })?;
                let result = sign_in::finish(host, &self.endpoints, pending, || {
                    self.pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(session)
                })
                .await;
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(session);
                result
            }
            _ => Err(PluginError::config(
                "sign-in completes after browser approval or with an API key",
            )),
        }
    }

    async fn cancel_sign_in(
        &self,
        ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        let _ = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        match creds::Stored::load(host, account).await {
            Ok(stored) if stored.fresh_at(host.clock().now()) => Ok(AccountStatus::Ready),
            Ok(_) => Ok(AccountStatus::NeedsSignIn {
                reason: "the WorkBuddy sign-in has expired".to_owned(),
            }),
            Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
                Ok(AccountStatus::NeedsSignIn {
                    reason: error.to_string(),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        refresh::run(host, Arc::clone(&self.endpoints), account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = refresh::ensure_fresh(host, Arc::clone(&self.endpoints), account).await?;
        if stored.credential.api_key {
            return Ok(Vec::new());
        }
        let base = self.endpoints.base_for(stored.credential.site);
        let reply = meter(
            host,
            &stored.credential,
            base,
            "/billing/meter/get-user-resource-summary",
            b"{}".to_vec(),
        )
        .await?;
        match reply.status() {
            200..=299 => {}
            401 => {
                return Err(PluginError::auth(
                    "WorkBuddy refused the sign-in while reading billing",
                ));
            }
            status => {
                return Err(PluginError::status_error(
                    status,
                    "WorkBuddy did not return billing",
                ));
            }
        }
        let mut summary = quota::summary(reply.body(), stored.credential.site)?;
        if summary.windows.is_empty() {
            if let Ok(reply) = meter(
                host,
                &stored.credential,
                base,
                "/billing/meter/get-enterprise-user-usage",
                b"{}".to_vec(),
            )
            .await
                && (200..300).contains(&reply.status())
                && let Ok(Some(window)) = quota::enterprise(reply.body())
            {
                return Ok(vec![window]);
            }
            return Ok(summary.windows);
        }
        if summary.has_credits() {
            let now = host.clock().now();
            if let Ok(packages) = package_list(host, &stored.credential, base, now).await {
                let _ = summary.apply_packages(&packages, now, stored.credential.site);
            }
        }
        Ok(summary.windows)
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let stored = refresh::ensure_fresh(host, Arc::clone(&self.endpoints), account).await?;
        models::list_account(host, Arc::clone(&self.endpoints), &stored).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::WorkBuddyUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
        )))
    }

    async fn daily_checkin(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<hofund_plugin_api::CheckinResult, PluginError> {
        checkin::run(account, host, Arc::clone(&self.endpoints)).await
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

async fn meter(
    host: &HostContext,
    credential: &creds::Credential,
    base: &str,
    path: &str,
    body: Vec<u8>,
) -> Result<hofund_plugin_sdk::Reply, PluginError> {
    let call = creds::apply_signed_headers(
        hofund_plugin_sdk::Call::post(format!("{base}{path}"))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json"),
        credential,
        base,
    );
    call.bytes(body).fetch(host.http()).await
}

async fn package_list(
    host: &HostContext,
    credential: &creds::Credential,
    base: &str,
    now: std::time::SystemTime,
) -> Result<Vec<hofund_plugin_sdk::json::Value>, PluginError> {
    let mut out = Vec::new();
    for (path, free) in [
        ("/billing/meter/get-user-resource-paid-packages", false),
        ("/billing/meter/get-user-resource-free-packages", true),
    ] {
        for page in 1..=quota::package_page_limit() {
            let body = if free {
                quota::free_package_body(page, now)
            } else {
                quota::paid_package_body(page)
            };
            let reply = meter(host, credential, base, path, body).await?;
            if !(200..300).contains(&reply.status()) {
                return Err(PluginError::status_error(
                    reply.status(),
                    "WorkBuddy did not return billing packages",
                ));
            }
            let page = quota::package_page(reply.body())?;
            let count = page.accounts.len();
            out.extend(page.accounts);
            if count < 200 || out.len() >= page.total_count {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
