//! Kiro subscription: browser sign-in, IDE or CLI import, source-aware refresh,
//! usage limits, and Anthropic Messages chat with streams, tools, and images.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, BinaryName, BuildError, ConfigError, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, Schema, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::secret_text;

mod agent_sign_in;
mod cli_db;
mod creds;
mod discover;
mod kiro_body;
mod kiro_stream;
mod models;
mod portal;
mod quota;
mod refresh;
mod upstream;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("kiro"),
    "Kiro",
    SignInFlowKind::Bridge,
)
.with_agent_sign_in(creds::AGENT_SIGN_IN);

/// Registers the Kiro subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// HTTP endpoints for refresh and usage. Tests aim these at mocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Kiro runtime API base; `{region}` is substituted per account.
    pub runtime: String,
    /// Management API base URL; `{region}` is substituted per account.
    pub management: String,
    /// Refresh token URL; `{region}` is substituted per account.
    pub refresh: String,
    /// Portal page for browser sign-in.
    pub portal: String,
    /// Social code-exchange service.
    pub auth: String,
    /// AWS OIDC base; `{region}` is substituted per sign-in.
    pub oidc: String,
    /// Callback ports in reference order. Tests inject port zero.
    pub callback_ports: Vec<u16>,
}

impl Endpoints {
    pub(crate) fn chat_base(&self, profile_arn: &str, signed_in_region: &str) -> String {
        self.runtime.replace(
            "{region}",
            &creds::kiro_region(profile_arn, signed_in_region),
        )
    }

    pub(crate) fn refresh_url(&self, region: &str) -> String {
        self.refresh.replace("{region}", region)
    }

    pub(crate) fn usage_url(&self, profile_arn: &str, signed_in_region: &str) -> String {
        let region = creds::kiro_region(profile_arn, signed_in_region);
        let base = self.management.replace("{region}", &region);
        format!(
            "{base}/Get-Usage-Limits?origin=KIRO_CLI&profileArn={}&resourceType=CREDIT&isEmailRequired=true",
            creds::percent_encode_query(profile_arn)
        )
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            runtime: "https://runtime.{region}.kiro.dev".to_owned(),
            management: "https://management.{region}.kiro.dev".to_owned(),
            refresh: "https://prod.{region}.auth.desktop.kiro.dev/refreshToken".to_owned(),
            portal: "https://app.kiro.dev".to_owned(),
            auth: "https://prod.us-east-1.auth.desktop.kiro.dev".to_owned(),
            oidc: "https://oidc.{region}.amazonaws.com".to_owned(),
            callback_ports: vec![
                3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153,
            ],
        }
    }
}

/// Builds the factory against `endpoints`.
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> KiroFactory {
    KiroFactory {
        endpoints: Arc::new(endpoints),
        cli_command: None,
    }
}

/// Builds a factory with an injected kiro-cli program for offline tests.
#[doc(hidden)]
#[must_use]
pub fn factory_with_cli_command(endpoints: Endpoints, command: PathBuf) -> KiroFactory {
    KiroFactory {
        endpoints: Arc::new(endpoints),
        cli_command: Some(command),
    }
}

/// Builds Kiro subscription instances.
#[derive(Clone, Debug)]
pub struct KiroFactory {
    endpoints: Arc<Endpoints>,
    cli_command: Option<PathBuf>,
}

/// Validated config: no options yet.
#[derive(Clone, Copy, Debug)]
pub struct KiroConfig;

impl ValidConfig for KiroConfig {}

impl SubscriptionFactory for KiroFactory {
    type Config = KiroConfig;

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
        Ok(KiroConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Kiro {
            endpoints: Arc::clone(&self.endpoints),
            cli_command: self.cli_command.clone(),
            flow: SignInFlow::Bridge {
                command: BinaryName::new("kiro-cli"),
            },
            pending: Mutex::new(HashMap::new()),
        }))
    }
}

enum Pending {
    Ide(PathBuf),
    Cli(PathBuf),
    Browser(portal::Pending),
}

struct Kiro {
    endpoints: Arc<Endpoints>,
    cli_command: Option<PathBuf>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Kiro {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }

    fn put_pending(&self, session: &str, pending: Pending) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.to_owned(), pending);
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Kiro {
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
        drop(self.take_pending(ctx.session()));
        if matches!(ctx.option("site"), Some("current" | "import")) {
            if let Ok(path) = cli_db::default_path()
                && let Some(credential) = cli_db::read_at(&path)?
            {
                let _ = credential;
                self.put_pending(ctx.session(), Pending::Cli(path));
                return SignInChallenge::instructions(
                    "kiro-cli is signed in on this machine; confirm to import its database sign-in.",
                );
            }
            let path = creds::discover_token_path();
            creds::read_token_file(&path).map_err(|error| {
                PluginError::auth(format!(
                    "Kiro is not signed in at {}; {}",
                    path.display(),
                    error.message()
                ))
            })?;
            self.put_pending(ctx.session(), Pending::Ide(path));
            return SignInChallenge::instructions(
                "Kiro is signed in on this machine; confirm to import its token file.",
            );
        }
        let (pending, challenge) = portal::start(ctx, host, Arc::clone(&self.endpoints)).await?;
        self.put_pending(ctx.session(), Pending::Browser(pending));
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
                "Kiro sign-in completes from its browser callback or an existing sign-in",
            ));
        };
        let Some(pending) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Kiro sign-in is in progress for this session",
            ));
        };
        match pending {
            Pending::Ide(path) => {
                let home = path.parent().ok_or_else(|| {
                    PluginError::config("Kiro token file has no parent directory")
                })?;
                let credential = creds::read_token_file(&path)?;
                discover::store(home, &credential, host).await
            }
            Pending::Cli(path) => {
                let credential = cli_db::read_at(&path)?.ok_or_else(|| {
                    PluginError::auth("kiro-cli no longer has a readable sign-in")
                })?;
                discover::store(Path::new(""), &credential, host).await
            }
            Pending::Browser(pending) => {
                let credential = portal::complete(pending).await?;
                discover::store_browser(&credential, host).await
            }
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
            return Ok(AccountStatus::NeedsSignIn {
                reason: "no Kiro sign-in is stored for this account".to_owned(),
            });
        };
        let stored = creds::parse_secret(&secret)?;
        if stored.credential.method != "apikey"
            && secret_text(&stored.credential.refresh).is_empty()
            && !stored.fresh_at(host.clock().now())
        {
            return Ok(AccountStatus::NeedsSignIn {
                reason: "Kiro sign-in has expired and cannot be refreshed".to_owned(),
            });
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        let stored = creds::Stored::load(host, account).await?;
        if stored.fresh_at(host.clock().now()) {
            return Ok(());
        }
        refresh::refresh_stale(
            host,
            account,
            &stored,
            &self.endpoints,
            self.cli_command.as_deref(),
        )
        .await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored = refresh::ensure_fresh(
            host,
            account,
            Arc::clone(&self.endpoints),
            self.cli_command.as_deref(),
        )
        .await?;
        let url = self.endpoints.usage_url(
            creds::effective_profile_arn(&stored.credential),
            &stored.credential.region,
        );
        let reply = hofund_plugin_sdk::Call::get(url)
            .sensitive_header(
                "authorization",
                format!(
                    "Bearer {}",
                    hofund_plugin_sdk::oauth::secret_text(&stored.credential.access)
                ),
            )
            .header("accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => quota::windows(reply.body()),
            401 | 403 => Err(PluginError::auth(
                "Kiro refused to show usage limits for this sign-in",
            )),
            status => Err(PluginError::status_error(
                status,
                "Kiro did not return usage limits",
            )),
        }
    }

    async fn list_models(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        models::catalog()
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::KiroUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
            self.cli_command.clone(),
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

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod portal_tests;
#[cfg(test)]
mod r51b_tests;
#[cfg(test)]
mod tests;
