//! Cursor subscription: browser sign-in, cursor-agent import, models, and usage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BuildError, ConfigError, HostContext, ModelList,
    PluginError, PluginId, QuotaWindow, RawTable, RawValue, Schema, SignInChallenge, SignInCtx,
    SignInFlow, SignInFlowKind, SignInProof, Subscription, SubscriptionFactory, SubscriptionSpec,
    UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject, secret_text};

/// Agent discovery. Wired into a Run session in the next R-8f1 checkpoint.
#[allow(dead_code)]
mod agent_host;
mod agent_sign_in;
mod auth;
mod cli;
mod connect;
mod discover;
mod endpoints;
/// Connect framing. Wired into a Run session in a later R-8 slice.
#[allow(dead_code)]
mod frame;
/// HTTP/2 availability. Wired into a Run session in the next R-8f1 checkpoint.
#[allow(dead_code)]
mod http2_memory;
mod import;
mod models;
/// Hand-written protobuf codec. Wired into a Run session in a later R-8 slice.
#[allow(dead_code)]
mod proto;
mod quota;
mod refresh;
mod session;
mod sign_in;
mod tokens;

pub use endpoints::Endpoints;

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("cursor"),
    "Cursor",
    SignInFlowKind::Pkce,
)
.with_agent_sign_in(AgentSignIn::new(
    &["cursor-agent", "agent"],
    "CURSOR_CONFIG_DIR",
    ".cursor",
));

/// Registers the Cursor subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Builds the factory against `endpoints`.
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> CursorFactory {
    CursorFactory {
        endpoints: Arc::new(endpoints),
        test_agent_home: None,
        skip_keychain: false,
    }
}

/// Builds Cursor subscription instances.
#[derive(Clone, Debug)]
pub struct CursorFactory {
    endpoints: Arc<Endpoints>,
    /// When set, agent refresh and token reads use this directory instead of the OS default.
    #[doc(hidden)]
    pub test_agent_home: Option<PathBuf>,
    /// When true, macOS keychain reads are skipped (tests).
    #[doc(hidden)]
    pub skip_keychain: bool,
}

/// Validated plugin config.
#[derive(Clone, Debug, Default)]
pub struct CursorConfig {
    command: Option<PathBuf>,
}

impl ValidConfig for CursorConfig {}

impl SubscriptionFactory for CursorFactory {
    type Config = CursorConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(vec![("command".to_owned(), Schema::string())], &[])
            .expect("command schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "command" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        let command = match raw.get("command") {
            None => None,
            Some(RawValue::String(value)) => {
                let path = PathBuf::from(value.trim());
                if !path.is_absolute() {
                    return Err(
                        ConfigError::new("command must be an absolute path").with_path("command")
                    );
                }
                Some(path)
            }
            Some(_) => {
                return Err(ConfigError::new("command must be a string").with_path("command"));
            }
        };
        Ok(CursorConfig { command })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Cursor {
            endpoints: Arc::clone(&self.endpoints),
            command: cfg.command.clone(),
            flow: SignInFlow::Pkce,
            pending: Mutex::new(HashMap::new()),
            keychain: Mutex::new(cli::KeychainCache::default()),
            agent_home: self
                .test_agent_home
                .clone()
                .unwrap_or_else(auth::default_agent_home),
            skip_keychain: self.skip_keychain,
        }))
    }
}

struct Cursor {
    endpoints: Arc<Endpoints>,
    command: Option<PathBuf>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::BrowserPending>>,
    keychain: Mutex<cli::KeychainCache>,
    agent_home: PathBuf,
    skip_keychain: bool,
}

impl Cursor {
    fn program(&self) -> Result<PathBuf, PluginError> {
        cli::program(self.command.as_ref())
    }

    fn agent_home(&self) -> &Path {
        &self.agent_home
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Cursor {
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
        let pkce = Pkce::generate()?;
        let uuid = hofund_plugin_sdk::oauth::new_state()?;
        let (url, pending) = sign_in::start(&pkce, &uuid, &self.endpoints);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), pending);
        SignInChallenge::open_url(url)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Cursor sign-in finishes once the browser has signed in",
            ));
        };
        let session = ctx.session().to_string();
        let pending = {
            let guard = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(pending) = guard.get(&session) else {
                return Err(PluginError::config(
                    "no Cursor sign-in is in progress for this session",
                ));
            };
            pending.clone()
        };
        let blob = match sign_in::finish(host, &self.endpoints, &pending, || {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&session)
        })
        .await
        {
            Ok(blob) => blob,
            Err(error) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&session);
                return Err(error);
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session);
        let access = blob
            .secret(tokens::ACCESS)
            .ok_or_else(|| PluginError::auth("Cursor sent back no access token"))?;
        let (id_label, id) = tokens::jwt_identity(secret_text(&access))
            .ok_or_else(|| PluginError::auth("Cursor signed in but did not name the account"))?;
        let account = host.accounts().account(id.clone(), id_label)?;
        host.accounts().upsert(account, blob.to_secret()).await?;
        Ok(id)
    }

    async fn cancel_sign_in(
        &self,
        ctx: &SignInCtx,
        _host: &HostContext,
    ) -> Result<(), PluginError> {
        self.pending
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
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(AccountStatus::NeedsSignIn {
                reason: "no Cursor sign-in is stored for this account".to_owned(),
            });
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(AccountStatus::NeedsSignIn {
                reason: "the stored Cursor sign-in is unreadable".to_owned(),
            });
        };
        if tokens::is_agent(&blob) {
            return Ok(AccountStatus::Ready);
        }
        if blob.secret(tokens::ACCESS).is_none() {
            return Ok(AccountStatus::NeedsSignIn {
                reason: "the stored Cursor sign-in has no access token".to_owned(),
            });
        }
        let now = host.clock().now();
        if let Some(ms) = tokens::effective_expires_ms(&blob) {
            let expires =
                tokens::ms_from_now(std::time::UNIX_EPOCH, i64::try_from(ms).unwrap_or(i64::MAX));
            if now >= expires {
                return Ok(AccountStatus::NeedsSignIn {
                    reason: "the Cursor sign-in has expired".to_owned(),
                });
            }
        }
        Ok(AccountStatus::Ready)
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Cursor sign-in is stored for this account",
            ));
        };
        let blob = SecretObject::parse(&secret)?;
        if tokens::is_agent(&blob) {
            let home = self.agent_home().to_path_buf();
            let token = cli::live_access(host, &home, false, &self.keychain, self.skip_keychain)
                .await?
                .unwrap_or_default();
            let now = host.clock().now();
            if !token.is_empty() && !tokens::near_expiry(now, tokens::jwt_expiry_ms(&token)) {
                return Ok(());
            }
            let program = self.program()?;
            refresh::agent_status(host, &program, &home).await
        } else {
            refresh::oauth_check(host, &self.endpoints, account).await
        }
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let token = session::access_token(
            host,
            &self.endpoints,
            account,
            self.agent_home(),
            self.program().ok(),
            &self.keychain,
            self.skip_keychain,
        )
        .await?;
        let body =
            connect::unary(host, &self.endpoints, "GetCurrentPeriodUsage", &token, "{}").await?;
        quota::period_windows(&body)
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let token = session::access_token(
            host,
            &self.endpoints,
            account,
            self.agent_home(),
            self.program().ok(),
            &self.keychain,
            self.skip_keychain,
        )
        .await?;
        let program = self.program()?;
        let models_dir = host
            .dirs()
            .state_dir()?
            .join("cursor-models")
            .join(account_label(account));
        auth::write_auth_file(&models_dir, hofund_plugin_sdk::oauth::secret_text(&token))?;
        let request = cli::request(&program, &["models"], &models_dir)?;
        let child = host.spawn_process().spawn_piped(&request).await?;
        let mut stdout = child.stdout()?;
        let text =
            match hofund_plugin_sdk::timeout(cli::MODELS_WAIT, cli::collect(&mut stdout, 8 << 20))
                .await
            {
                Some(Some(text)) if !text.trim().is_empty() => text,
                _ => {
                    return Err(PluginError::config(
                        "cursor-agent did not return a model list",
                    ));
                }
            };
        models::model_list(&text)
    }

    fn upstream(
        &self,
        _account: &AccountId,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Err(PluginError::unsupported(
            "requests through Cursor's agent API are not served yet",
        ))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        import::import_accounts(host, text).await
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

fn account_label(account: &AccountId) -> String {
    account
        .to_string()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
