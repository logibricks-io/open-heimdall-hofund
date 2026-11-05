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
mod models;
mod quota;
mod refresh;
mod site;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("workbuddy"),
    "WorkBuddy",
    SignInFlowKind::Bridge,
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

/// Builds the factory with an optional CodeBuddyExtension data root (tests).
#[must_use]
pub fn factory_with_extension_data(
    endpoints: Endpoints,
    extension_data: Option<PathBuf>,
) -> WorkBuddyFactory {
    WorkBuddyFactory {
        endpoints: Arc::new(endpoints),
        extension_data,
    }
}

/// Builds WorkBuddy subscription instances.
#[derive(Clone, Debug)]
pub struct WorkBuddyFactory {
    endpoints: Arc<Endpoints>,
    extension_data: Option<PathBuf>,
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
            extension_data: self.extension_data.clone(),
            flow: SignInFlow::Bridge {
                command: hofund_plugin_api::BinaryName::new("workbuddy"),
            },
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct WorkBuddy {
    endpoints: Arc<Endpoints>,
    extension_data: Option<PathBuf>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, site::Site>>,
}

impl WorkBuddy {
    fn data_root(&self, home: Option<&Path>) -> Result<PathBuf, PluginError> {
        if let Some(dir) = &self.extension_data {
            return Ok(dir.clone());
        }
        if let Some(home) = home {
            return Ok(home.to_path_buf());
        }
        auth_store::extension_data_dir()
    }
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
        _host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        let site = site::Site::China;
        let root = self.data_root(None)?;
        let path = auth_store::auth_path_in(&root, site);
        if !path.is_file() {
            return Err(PluginError::auth(format!(
                "WorkBuddy desktop auth file is missing at {}",
                path.display()
            )));
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), site);
        SignInChallenge::instructions(format!(
            "Import the WorkBuddy desktop auth file at {} (China build).",
            path.display()
        ))
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "WorkBuddy sign-in completes once the desktop auth file is imported",
            ));
        };
        let Some(site) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session())
        else {
            return Err(PluginError::config(
                "no WorkBuddy sign-in is in progress for this session",
            ));
        };
        let root = self.data_root(None)?;
        let path = auth_store::auth_path_in(&root, site);
        let text = std::fs::read_to_string(&path).map_err(|error| {
            PluginError::auth(format!("{} can't be read ({error})", path.display()))
        })?;
        let credential = creds::parse_desktop_info(&text, site, &path)?;
        discover::store(&credential, host).await
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
        let base = self.endpoints.base_for(stored.credential.site);
        let url = format!("{base}/billing/meter/get-user-resource-summary");
        let mut call = hofund_plugin_sdk::Call::post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("User-Agent", stored.credential.site.user_agent());
        for (name, value) in creds::bearer_headers(&stored.credential) {
            if name == "Authorization" {
                call = call.sensitive_header("authorization", value);
            } else if name != "User-Agent" {
                call = call.header(name, value);
            }
        }
        let reply = call.bytes(b"{}").fetch(host.http()).await?;
        match reply.status() {
            200..=299 => quota::windows(reply.body()),
            401 => Err(PluginError::auth(
                "WorkBuddy refused the sign-in while reading billing",
            )),
            status => Err(PluginError::status_error(
                status,
                "WorkBuddy did not return billing",
            )),
        }
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
        checkin::run(account, host).await
    }

    async fn discover_agent_sign_in(
        &self,
        home: &Path,
        _host: &HostContext,
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
mod tests;
