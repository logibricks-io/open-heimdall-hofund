//! Cursor subscription: browser sign-in, cursor-agent import, models, usage,
//! and Agent Run request serving.

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

mod agent_host;
mod agent_sign_in;
mod auth;
mod cli;
mod connect;
mod discover;
mod endpoints;
mod exchange;
mod failure;
mod frame;
mod http2_memory;
mod import;
mod max_only;
mod models;
mod picker;
mod proto;
mod quota;
mod refresh;
mod request;
mod run;
mod session;
mod sign_in;
mod team;
mod tokens;
mod upstream;
mod usage;

pub(crate) use endpoints::Endpoints;

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
#[doc(hidden)]
#[must_use]
pub(crate) fn factory_with_endpoints(endpoints: Endpoints) -> CursorFactory {
    CursorFactory {
        endpoints: Arc::new(endpoints),
        test_agent_home: None,
        skip_keychain: false,
        force_keychain: false,
    }
}

/// Builds Cursor subscription instances.
#[derive(Clone, Debug)]
pub struct CursorFactory {
    endpoints: Arc<Endpoints>,
    /// When set, agent refresh and token reads use this directory instead of the OS default.
    #[doc(hidden)]
    pub(crate) test_agent_home: Option<PathBuf>,
    /// When true, keychain reads are skipped (tests).
    #[doc(hidden)]
    pub(crate) skip_keychain: bool,
    /// When true, keychain reads run on every OS so tests can script the child.
    #[doc(hidden)]
    pub(crate) force_keychain: bool,
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
            keychain: Arc::new(Mutex::new(cli::KeychainCache::default())),
            agent_home: self
                .test_agent_home
                .clone()
                .unwrap_or_else(auth::default_agent_home),
            keychain_enabled: self.keychain_enabled(),
        }))
    }
}

impl CursorFactory {
    fn keychain_enabled(&self) -> bool {
        if self.skip_keychain {
            return false;
        }
        self.force_keychain || cfg!(target_os = "macos")
    }
}

struct Cursor {
    endpoints: Arc<Endpoints>,
    command: Option<PathBuf>,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, sign_in::PendingSignIn>>,
    keychain: Arc<Mutex<cli::KeychainCache>>,
    agent_home: PathBuf,
    keychain_enabled: bool,
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
        if let Some(key) = crate::exchange::read_env_api_key() {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    ctx.session().to_owned(),
                    sign_in::PendingSignIn::ApiKey(key),
                );
            return SignInChallenge::instructions(format!(
                "Using the Cursor API key from the {} environment variable.",
                crate::exchange::CURSOR_API_KEY_ENV
            ));
        }
        let pkce = Pkce::generate()?;
        let uuid = hofund_plugin_sdk::oauth::new_state()?;
        let (url, pending) = sign_in::start(&pkce, &uuid, &self.endpoints);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                ctx.session().to_owned(),
                sign_in::PendingSignIn::Browser(pending),
            );
        SignInChallenge::open_url(url)
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        match proof {
            SignInProof::Token(key) => {
                let key = if secret_text(&key).trim().is_empty() {
                    crate::exchange::read_env_api_key()
                        .ok_or_else(|| PluginError::auth("Cursor API key is empty"))?
                } else {
                    key
                };
                import::store_api_key(host, &self.endpoints, key).await
            }
            SignInProof::Await => {
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
                match pending {
                    sign_in::PendingSignIn::ApiKey(key) => {
                        let result = import::store_api_key(host, &self.endpoints, key).await;
                        self.pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&session);
                        result
                    }
                    sign_in::PendingSignIn::Browser(pending) => {
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
                            .ok_or_else(|| {
                                PluginError::auth("Cursor signed in but did not name the account")
                            })?;
                        let account = host.accounts().account(id.clone(), id_label)?;
                        host.accounts().upsert(account, blob.to_secret()).await?;
                        Ok(id)
                    }
                }
            }
            _ => Err(PluginError::config(
                "Cursor sign-in finishes once the browser has signed in or with an API key",
            )),
        }
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
        if tokens::is_api_key(&blob) {
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
            refresh::renew_agent(
                host,
                account,
                self.program().ok().as_deref(),
                self.agent_home(),
                &self.keychain,
                self.keychain_enabled,
            )
            .await
            .map(|_| ())
        } else if tokens::is_api_key(&blob) {
            crate::exchange::access_token(host, &self.endpoints, account)
                .await
                .map(|_| ())
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
            self.keychain_enabled,
        )
        .await?;
        let team_id = team::header_value(&token);
        let body = connect::unary_opts(
            host,
            &self.endpoints,
            "GetCurrentPeriodUsage",
            &token,
            "{}",
            team_id.as_deref(),
            None,
        )
        .await?;
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
            self.keychain_enabled,
        )
        .await?;
        let program = self.program()?;
        let models_dir = models_temp_dir(account)?;
        auth::write_auth_file(
            &models_dir.path,
            hofund_plugin_sdk::oauth::secret_text(&token),
        )?;
        let request = cli::request(&program, &["models"], &models_dir.path)?;
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
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::CursorUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
            self.agent_home.clone(),
            self.program().ok(),
            Arc::clone(&self.keychain),
            self.keychain_enabled,
        )))
    }

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        import::import_accounts(host, &self.endpoints, text).await
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

/// Isolated `cursor-agent models` config dir. Removed on drop, including errors.
struct ModelsTempDir {
    path: PathBuf,
}

impl Drop for ModelsTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn models_temp_dir(account: &AccountId) -> Result<ModelsTempDir, PluginError> {
    let digest = hofund_plugin_sdk::digest::sha256(account.to_string().as_bytes());
    let mut name = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("write to a string");
    }
    let path = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&path)
        .map_err(|_| PluginError::io("cannot create the Cursor models directory"))?;
    Ok(ModelsTempDir { path })
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
