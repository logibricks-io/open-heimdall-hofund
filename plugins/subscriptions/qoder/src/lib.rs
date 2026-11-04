//! Qoder subscription plugin.

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

mod checkin;
mod creds;
mod discover;
mod models;
mod quota;
mod refresh;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("qoder"),
    "Qoder",
    SignInFlowKind::Bridge,
)
.with_daily_checkin(true)
.with_agent_sign_in(AgentSignIn::new(
    &["qoder", creds::CN_SITE],
    "QODER_CONFIG_DIR",
    ".qoder",
));

/// Registers the Qoder subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for openapi, refresh, chat, and usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Global openapi host (`https://openapi.qoder.sh`).
    pub openapi_global: String,
    /// China openapi host (`https://openapi.qoder.com.cn`).
    pub openapi_cn: String,
    /// Global chat API base (`https://api3.qoder.sh/v1`).
    pub chat_global: String,
    /// China chat API base (`https://gateway.qoder.com.cn/v1`).
    pub chat_cn: String,
}

impl Endpoints {
    pub(crate) fn openapi_for(&self, site: creds::Site) -> &str {
        match site {
            creds::Site::Global => self.openapi_global.as_str(),
            creds::Site::China => self.openapi_cn.as_str(),
        }
    }

    pub(crate) fn chat_for(&self, site: creds::Site) -> &str {
        match site {
            creds::Site::Global => self.chat_global.as_str(),
            creds::Site::China => self.chat_cn.as_str(),
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            openapi_global: "https://openapi.qoder.sh".to_owned(),
            openapi_cn: "https://openapi.qoder.com.cn".to_owned(),
            chat_global: "https://api3.qoder.sh/v1".to_owned(),
            chat_cn: "https://gateway.qoder.com.cn/v1".to_owned(),
        }
    }
}

/// Builds the factory against `endpoints`.
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> QoderFactory {
    QoderFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Qoder subscription instances.
#[derive(Clone, Debug)]
pub struct QoderFactory {
    endpoints: Arc<Endpoints>,
}

/// Validated config: no options yet.
#[derive(Clone, Copy, Debug)]
pub struct QoderConfig;

impl ValidConfig for QoderConfig {}

impl SubscriptionFactory for QoderFactory {
    type Config = QoderConfig;

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
        Ok(QoderConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Qoder {
            endpoints: Arc::clone(&self.endpoints),
            flow: SignInFlow::Bridge {
                command: hofund_plugin_api::BinaryName::new("qodercli"),
            },
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Qoder {
    endpoints: Arc<Endpoints>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, PathBuf>>,
}

impl Qoder {
    fn take_pending(&self, session: &str) -> Option<PathBuf> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Qoder {
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
        drop(self.take_pending(ctx.session()));
        let path = default_credential_path()?;
        creds::read_file(&path).map_err(|error| {
            PluginError::auth(format!(
                "Qoder is not signed in at {}; {}",
                path.display(),
                error.message()
            ))
        })?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), path);
        SignInChallenge::instructions(
            "Qoder credentials are on this machine; confirm to import them.",
        )
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Qoder sign-in completes once the credential file is imported",
            ));
        };
        let Some(path) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Qoder sign-in is in progress for this session",
            ));
        };
        let credential = creds::read_file(&path)?;
        discover::store(&credential, host).await
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
        match creds::Stored::load(host, account).await {
            Ok(stored) if stored.ready_at(host.clock().now()) => Ok(AccountStatus::Ready),
            Ok(_) => Ok(AccountStatus::NeedsSignIn {
                reason: "the Qoder sign-in has expired".to_owned(),
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
        let url = format!(
            "{}/api/v1/adapter/user/account-context?include=user,plan,quota",
            self.endpoints.openapi_for(stored.credential.site)
        );
        let reply = hofund_plugin_sdk::Call::get(url)
            .sensitive_header(
                "authorization",
                format!(
                    "Bearer {}",
                    hofund_plugin_sdk::oauth::secret_text(&stored.credential.token)
                ),
            )
            .header("Accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => quota::windows(reply.body()),
            401 => Err(PluginError::auth(
                "Qoder refused the sign-in while reading usage",
            )),
            status => Err(PluginError::status_error(
                status,
                "Qoder did not return usage",
            )),
        }
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        models::list_account(host, Arc::clone(&self.endpoints), account).await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::QoderUpstream::new(
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

fn default_credential_path() -> Result<PathBuf, PluginError> {
    if let Ok(dir) = std::env::var("QODER_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join(creds::CREDENTIAL_FILE));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| PluginError::io("cannot resolve the user home for sign-in"))?;
    Ok(PathBuf::from(home)
        .join(".qoder")
        .join(creds::CREDENTIAL_FILE))
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
