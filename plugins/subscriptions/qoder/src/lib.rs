//! Qoder subscription plugin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BuildError, ConfigError, HostContext, ModelList, PluginError,
    PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod checkin;
mod cosy;
mod creds;
mod identity;
mod import;
mod legacy_migrate;
mod models;
mod quota;
mod refresh;
mod shape;
mod sign_in;
mod site;
mod stream;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("qoder"),
    "Qoder",
    SignInFlowKind::Device,
)
.with_daily_checkin(true);

/// Registers the Qoder subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for openapi, refresh, chat, and usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Global sign-in page host (`https://qoder.com`).
    pub web_global: String,
    /// China sign-in page host (`https://qoder.cn`).
    pub web_cn: String,
    /// Global openapi host (`https://openapi.qoder.sh`).
    pub openapi_global: String,
    /// China openapi host (`https://openapi.qoder.com.cn`).
    pub openapi_cn: String,
    /// Global native API host (`https://api3.qoder.sh`).
    pub chat_global: String,
    /// China native API host (`https://gateway.qoder.com.cn`).
    pub chat_cn: String,
}

impl Endpoints {
    pub(crate) fn web_for(&self, site: creds::Site) -> &str {
        match site {
            creds::Site::Global => self.web_global.as_str(),
            creds::Site::China => self.web_cn.as_str(),
        }
    }

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
            web_global: "https://qoder.com".to_owned(),
            web_cn: "https://qoder.cn".to_owned(),
            openapi_global: "https://openapi.qoder.sh".to_owned(),
            openapi_cn: "https://openapi.qoder.com.cn".to_owned(),
            chat_global: "https://api3.qoder.sh".to_owned(),
            chat_cn: "https://gateway.qoder.com.cn".to_owned(),
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
            devices: Arc::new(identity::Devices::new()),
            catalog: Arc::new(models::Catalog::default()),
            renewals: Arc::new(Mutex::new(HashMap::new())),
            flow: SignInFlow::Device(
                hofund_plugin_api::DeviceFlow::new("qoder.com")
                    .expect("static device host")
                    .with_path("/device/selectAccounts")
                    .expect("static device path"),
            ),
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

struct Qoder {
    endpoints: Arc<Endpoints>,
    devices: Arc<identity::Devices>,
    catalog: Arc<models::Catalog>,
    renewals: Arc<Mutex<HashMap<String, std::time::SystemTime>>>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::Pending>>,
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
        host: &HostContext,
    ) -> Result<SignInChallenge, PluginError> {
        let (pending, challenge) = sign_in::start(ctx, host, &self.endpoints)?;
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
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Qoder sign-in completes after the device page is approved",
            ));
        };
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(ctx.session())
            .cloned()
            .ok_or_else(|| {
                PluginError::config("no Qoder sign-in is in progress for this session")
            })?;
        let result = sign_in::complete(host, pending).await;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ctx.session());
        result
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
        if let Some(secret) = host.accounts().secret(account).await?
            && hofund_plugin_sdk::oauth::SecretObject::parse(&secret)
                .is_ok_and(|blob| blob.flag(creds::LAPSED))
        {
            return Ok(AccountStatus::NeedsSignIn {
                reason: creds::LAPSED_REASON.to_owned(),
            });
        }
        match creds::Stored::load(host, account).await {
            Ok(stored) if stored.ready_at(host.clock().now()) => Ok(AccountStatus::Ready),
            Ok(stored)
                if !hofund_plugin_sdk::oauth::secret_text(&stored.credential.refresh_token)
                    .is_empty() =>
            {
                Ok(AccountStatus::Ready)
            }
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
        quota::read(host, Arc::clone(&self.endpoints), account).await
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
            Arc::clone(&self.devices),
            Arc::clone(&self.catalog),
            Arc::clone(&self.renewals),
        )))
    }

    async fn daily_checkin(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<hofund_plugin_api::CheckinResult, PluginError> {
        checkin::run(
            account,
            host,
            Arc::clone(&self.endpoints),
            Arc::clone(&self.devices),
        )
        .await
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        import::import_accounts(host, text).await
    }

    async fn prepare_accounts(
        &self,
        config_dir: &std::path::Path,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        legacy_migrate::run(config_dir, host).await
    }
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod r54b_tests;
#[cfg(test)]
mod r54d_tests;
#[cfg(test)]
mod tests;
