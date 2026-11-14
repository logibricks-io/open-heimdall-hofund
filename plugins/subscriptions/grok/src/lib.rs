//! Grok Build subscription.
//!
//! Sign-in runs `grok login --device-auth` in a directory of its own, shows
//! the link it prints, and waits for the CLI to finish. hofund keeps the
//! token and the CLI home path, renews the sign-in with `grok models`, reads
//! models and billing from the CLI backend, and serves it as an OpenAI
//! Responses upstream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BinaryName, BuildError, ConfigError, HostContext,
    ModelList, PipedChild, PluginError, PluginId, QuotaWindow, RawTable, RawValue, Schema,
    SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

mod cli;
mod creds;
mod discover;
mod headers;
mod models;
mod quota;
mod refresh;
mod shape;
mod upstream;

/// The CLI command this subscription drives.
pub(crate) const COMMAND: BinaryName = BinaryName::new("grok");

/// Sign-in option that imports the CLI's existing home instead of device auth.
pub(crate) const CURRENT_SITE: &str = "current";

const LINK_WAIT: Duration = Duration::from_secs(30);
const LOGIN_WAIT: Duration = Duration::from_secs(600);

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("grok"),
    "Grok Build",
    SignInFlowKind::Bridge,
)
.with_agent_sign_in(AgentSignIn::new(&["grok"], "GROK_HOME", ".grok"));

/// Registers the Grok Build subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(factory_with_endpoints(Endpoints::default()))
}

/// Where the plugin sends requests. Tests point it at mocks.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The CLI backend base; requests go to `{base}/responses`.
    pub base: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            base: "https://cli-chat-proxy.grok.com/v1".to_owned(),
        }
    }
}

/// Builds the factory against `endpoints`.
#[doc(hidden)]
#[must_use]
pub fn factory_with_endpoints(endpoints: Endpoints) -> GrokFactory {
    GrokFactory {
        endpoints: Arc::new(endpoints),
    }
}

/// Builds Grok Build subscription instances.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct GrokFactory {
    endpoints: Arc<Endpoints>,
}

/// The validated config.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct GrokConfig {
    command: Option<PathBuf>,
    x_search: bool,
}

impl ValidConfig for GrokConfig {}

impl SubscriptionFactory for GrokFactory {
    type Config = GrokConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![
                ("command".to_owned(), Schema::string()),
                ("x_search".to_owned(), Schema::boolean()),
            ],
            &[],
        )
        .expect("the config schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "command" && key != "x_search" {
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
        let x_search = match raw.get("x_search") {
            None => false,
            Some(RawValue::Boolean(value)) => *value,
            Some(_) => {
                return Err(ConfigError::new("x_search must be a boolean").with_path("x_search"));
            }
        };
        Ok(GrokConfig { command, x_search })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Grok {
            endpoints: Arc::clone(&self.endpoints),
            command: cfg.command.clone(),
            x_search: cfg.x_search,
            flow: SignInFlow::Bridge { command: COMMAND },
            pending: Mutex::new(HashMap::new()),
            credentials: creds::Cache::default(),
        }))
    }
}

enum Pending {
    Login {
        home: PathBuf,
        #[allow(dead_code)]
        child: PipedChild,
        pump: Arc<Mutex<LoginPump>>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    },
    Current {
        home: PathBuf,
    },
}

struct LoginPump {
    stdout: Option<hofund_plugin_api::BodyStream>,
    last: String,
}

struct Grok {
    endpoints: Arc<Endpoints>,
    command: Option<PathBuf>,
    x_search: bool,
    flow: SignInFlow,
    pending: Mutex<HashMap<String, Pending>>,
    credentials: creds::Cache,
}

impl Grok {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }

    fn program(&self) -> Result<PathBuf, PluginError> {
        cli::program(self.command.as_ref())
    }

    fn program_fn(&self) -> Arc<dyn Fn() -> Result<PathBuf, PluginError> + Send + Sync> {
        let command = self.command.clone();
        Arc::new(move || cli::program(command.as_ref()))
    }
}

fn sign_in_dir(host: &HostContext, ctx: &SignInCtx) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join("signin").join(ctx.session()))
}

fn grok_account_home(host: &HostContext, session: &str) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join("grok-homes").join(session))
}

fn remove_dir_if_under(dir: &Path, root: &Path) {
    if dir.starts_with(root) {
        remove_dir(dir);
    }
}

fn remove_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn user_home() -> Result<PathBuf, PluginError> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    let Ok(home) = home else {
        return Err(PluginError::io("cannot resolve the user home for sign-in"));
    };
    let path = PathBuf::from(home);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(PluginError::io("cannot resolve the user home for sign-in"))
    }
}

fn default_cli_home() -> PathBuf {
    if let Ok(home) = std::env::var(cli::HOME_VAR) {
        return PathBuf::from(home);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".grok")
}

async fn start_login(
    host: &HostContext,
    program: PathBuf,
    home: PathBuf,
) -> Result<(Pending, String), PluginError> {
    private_dir(&home)?;
    let cwd = user_home()?;
    let request = cli::login_request(&program, &home, &cwd)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let mut last = String::new();
    let link = hofund_plugin_sdk::timeout(LINK_WAIT, cli::read_link(&mut stdout, &mut last)).await;
    let Some(Some(link)) = link else {
        return Err(PluginError::auth(format!(
            "Grok Build did not print a sign-in link; {}",
            cli::said(&last)
        )));
    };
    Ok((
        Pending::Login {
            home,
            child,
            pump: Arc::new(Mutex::new(LoginPump {
                stdout: Some(stdout),
                last,
            })),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        link,
    ))
}

struct LoginStdoutGuard {
    pump: Arc<Mutex<LoginPump>>,
    stdout: Option<hofund_plugin_api::BodyStream>,
    last: String,
}

impl LoginStdoutGuard {
    fn take(pump: &Arc<Mutex<LoginPump>>) -> Result<Self, PluginError> {
        let mut state = pump
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(stdout) = state.stdout.take() else {
            return Err(PluginError::config(
                "Grok Build sign-in was cancelled before the CLI finished",
            ));
        };
        Ok(Self {
            pump: Arc::clone(pump),
            stdout: Some(stdout),
            last: std::mem::take(&mut state.last),
        })
    }

    fn finish(&mut self, stream_ended: bool) {
        let Some(stdout) = self.stdout.take() else {
            return;
        };
        let mut state = self
            .pump
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last.clone_from(&self.last);
        if !stream_ended {
            state.stdout = Some(stdout);
        }
    }
}

impl Drop for LoginStdoutGuard {
    fn drop(&mut self) {
        if self.stdout.is_some() {
            self.finish(false);
        }
    }
}

fn abort_login(grok: &Grok, session: &str, host: &HostContext) {
    use std::sync::atomic::Ordering;
    if let Some(Pending::Login {
        home, cancelled, ..
    }) = grok.take_pending(session)
    {
        cancelled.store(true, Ordering::Relaxed);
        if let Ok(state) = host.dirs().state_dir() {
            remove_dir_if_under(&home, &state.join("grok-homes"));
        }
    }
}

async fn finish_login(grok: &Grok, session: &str) -> Result<(), PluginError> {
    use std::sync::atomic::Ordering;
    let deadline = LOGIN_WAIT;
    let finished = hofund_plugin_sdk::timeout(deadline, async {
        loop {
            let pump = {
                let guard = grok
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(Pending::Login {
                    pump, cancelled, ..
                }) = guard.get(session)
                else {
                    return Err(PluginError::config(
                        "Grok Build sign-in was cancelled before the CLI finished",
                    ));
                };
                if cancelled.load(Ordering::Relaxed) {
                    return Err(PluginError::config(
                        "Grok Build sign-in was cancelled before the CLI finished",
                    ));
                }
                Arc::clone(pump)
            };
            let done = {
                let mut guard = LoginStdoutGuard::take(&pump)?;
                let stream_ended =
                    cli::drain_once(guard.stdout.as_mut().expect("stdout"), &mut guard.last).await;
                guard.finish(stream_ended);
                stream_ended
            };
            if done {
                let home = {
                    let guard = grok
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(Pending::Login { home, .. }) = guard.get(session) else {
                        return Err(PluginError::config(
                            "Grok Build sign-in was cancelled before the CLI finished",
                        ));
                    };
                    home.clone()
                };
                let credential = creds::read_sync(&home).map_err(|error| match error {
                    creds::ReadError::Torn(reason) => PluginError::auth(format!(
                        "Grok Build finished, but its sign-in couldn't be read ({reason})"
                    )),
                    creds::ReadError::Missing(reason) => {
                        PluginError::auth(format!("Grok Build did not leave a sign-in: {reason}"))
                    }
                })?;
                if credential.who.is_empty() {
                    return Err(PluginError::auth(
                        "Grok Build left a sign-in that names no account",
                    ));
                }
                return Ok(());
            }
        }
    })
    .await;
    match finished {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(error),
        None => Err(PluginError::timeout(
            "Grok Build sign-in did not finish before the time limit",
        )),
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Grok {
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
        if ctx.option("site") == Some(CURRENT_SITE) {
            let home = default_cli_home();
            let credential = creds::read_sync(&home).map_err(|error| {
                PluginError::auth(format!(
                    "Grok Build is not signed in in {}; {}",
                    home.display(),
                    error.message()
                ))
            })?;
            if credential.who.is_empty() {
                return Err(PluginError::auth(
                    "Grok Build is signed in but names no account",
                ));
            }
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(ctx.session().to_owned(), Pending::Current { home });
            return SignInChallenge::instructions(
                "Grok Build is signed in; confirm to import its sign-in.",
            );
        }
        let program = self.program()?;
        let session_dir = sign_in_dir(host, ctx)?;
        remove_dir(&session_dir);
        let home = grok_account_home(host, ctx.session())?;
        remove_dir(&home);
        let (pending, link) = match start_login(host, program, home.clone()).await {
            Ok(started) => started,
            Err(error) => {
                remove_dir(&home);
                remove_dir(&session_dir);
                return Err(error);
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ctx.session().to_owned(), pending);
        if cli::opens_page_natively() {
            SignInChallenge::instructions(format!(
                "Grok Build opened the sign-in page in your browser.\n\
                 Approve the sign-in there; this finishes when the CLI exits.\n\
                 If nothing opened, visit:\n  {link}"
            ))
        } else {
            SignInChallenge::open_url(link)
        }
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        let session = ctx.session();
        let SignInProof::Await = proof else {
            return Err(PluginError::config(
                "Grok Build sign-in completes once the CLI finishes or the current sign-in is imported",
            ));
        };
        if !self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(session)
        {
            return Err(PluginError::config(
                "no Grok Build sign-in is in progress for this session",
            ));
        }
        {
            let needs_wait = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(session)
                .is_some_and(|pending| matches!(pending, Pending::Login { .. }));
            if needs_wait && let Err(error) = finish_login(self, session).await {
                abort_login(self, session, host);
                return Err(error);
            }
        }
        let Some(pending) = self.take_pending(session) else {
            return Err(PluginError::config(
                "Grok Build sign-in was cancelled before it could finish",
            ));
        };
        let (home, credential) = match pending {
            Pending::Current { home } => {
                let credential = creds::read_sync(&home).map_err(|error| {
                    PluginError::auth(format!(
                        "Grok Build has no readable sign-in in {}: {}",
                        home.display(),
                        error.message()
                    ))
                })?;
                if credential.who.is_empty() {
                    return Err(PluginError::auth(
                        "Grok Build is signed in but names no account",
                    ));
                }
                (home, credential)
            }
            Pending::Login { home, .. } => {
                let credential = creds::read_sync(&home).map_err(|error| {
                    PluginError::auth(format!(
                        "Grok Build has no readable sign-in in {}: {}",
                        home.display(),
                        error.message()
                    ))
                })?;
                if credential.who.is_empty() {
                    return Err(PluginError::auth(
                        "Grok Build left a sign-in that names no account",
                    ));
                }
                (home, credential)
            }
        };
        let id = creds::account_id(&credential);
        let label = if !credential.email.is_empty() {
            credential.email.clone()
        } else {
            credential.who.clone()
        };
        let secret = creds::secret_blob(&home, &credential).to_secret();
        let account = host.accounts().account(id.clone(), label)?;
        host.accounts().upsert(account, secret).await?;
        Ok(id)
    }

    async fn cancel_sign_in(&self, ctx: &SignInCtx, host: &HostContext) -> Result<(), PluginError> {
        if let Some(pending) = self.take_pending(ctx.session()) {
            if let Pending::Login {
                home, cancelled, ..
            } = &pending
            {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Ok(state) = host.dirs().state_dir() {
                    remove_dir_if_under(home, &state.join("signin"));
                    remove_dir_if_under(home, &state.join("grok-homes"));
                }
            }
            drop(pending);
        } else if let Ok(dir) = sign_in_dir(host, ctx)
            && let Ok(state) = host.dirs().state_dir()
        {
            remove_dir_if_under(&dir, &state.join("signin"));
        }
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
                reason: "the Grok Build sign-in has expired".to_owned(),
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
        refresh::run(host, &self.credentials, || self.program(), account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let stored =
            refresh::ensure_fresh(host, account, &self.credentials, || self.program(), None)
                .await?;
        let url = format!("{}/billing?format=credits", self.endpoints.base);
        let reply = hofund_plugin_sdk::Call::get(&url)
            .sensitive_header(
                "authorization",
                format!(
                    "Bearer {}",
                    hofund_plugin_sdk::oauth::secret_text(&stored.credential.key)
                ),
            )
            .header("accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => return quota::windows(reply.body()),
            401 => {}
            status => {
                return Err(PluginError::status_error(
                    status,
                    "the CLI backend did not return billing",
                ));
            }
        }
        let stored = refresh::ensure_fresh(
            host,
            account,
            &self.credentials,
            || self.program(),
            Some(&stored.credential.key),
        )
        .await?;
        let reply = hofund_plugin_sdk::Call::get(url)
            .sensitive_header(
                "authorization",
                format!(
                    "Bearer {}",
                    hofund_plugin_sdk::oauth::secret_text(&stored.credential.key)
                ),
            )
            .header("accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => quota::windows(reply.body()),
            401 => Err(PluginError::status_error(401, "Unauthorized")),
            status => Err(PluginError::status_error(
                status,
                "the CLI backend did not return billing",
            )),
        }
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        models::list(host, &self.endpoints, account, &self.credentials, || {
            self.program()
        })
        .await
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(Arc::new(upstream::GrokUpstream::new(
            account.clone(),
            host.clone(),
            Arc::clone(&self.endpoints),
            self.x_search,
            self.credentials.clone(),
            self.program_fn(),
        )))
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

#[cfg(unix)]
pub(crate) fn private_dir(path: &Path) -> Result<(), PluginError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| PluginError::io("cannot create a private directory"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| PluginError::io("cannot restrict a private directory"))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn private_dir(path: &Path) -> Result<(), PluginError> {
    std::fs::create_dir_all(path).map_err(|_| PluginError::io("cannot create a directory"))?;
    Ok(())
}

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod tests;
