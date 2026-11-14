//! Claude Code subscription.
//!
//! Sign-in runs `claude auth login --claudeai` in a directory of its own,
//! shows the link it prints, and hands it the code the user pastes back.
//! hofund keeps the credentials file Claude Code wrote, and copies it into a
//! per-account directory whenever it runs `claude` for that account. A
//! rotated file is written back through the account lease. Requests through
//! Claude Code are not served yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, AccountStatus, AgentSignIn, BinaryName, BodyStream, BuildError, ConfigError,
    HostContext, ModelList, PipedChild, PluginError, PluginId, QuotaWindow, RawTable, RawValue,
    Schema, SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Subscription,
    SubscriptionFactory, SubscriptionSpec, UpstreamProvider, ValidConfig,
};
use hofund_plugin_sdk::Contribution;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

mod account;
mod agent_sign_in;
pub mod bridge;
mod catalog_cache;
mod cli;
mod creds;
mod discover;
mod import;
mod launcher;
mod lock;
mod models;
mod run_lock;
mod setup_token;
mod upstream;
mod usage;
mod usage_reset;
mod wslrun;

use account::{account_dir, remove_tree};
use creds::{Credentials, Profile};

/// The command Claude Code installs.
pub(crate) const COMMAND: BinaryName = BinaryName::new("claude");

/// How long sign-in waits for Claude Code to print its link.
const LINK_WAIT: Duration = Duration::from_secs(30);

/// How long Claude Code gets to trade a pasted code.
const CODE_WAIT: Duration = Duration::from_secs(60);

/// How long sign-in waits for the browser when nothing is pasted.
const BROWSER_WAIT: Duration = Duration::from_secs(600);

/// How long `claude auth status` gets.
pub(crate) const STATUS_WAIT: Duration = Duration::from_secs(15);

static SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
    PluginId::from_static("claude"),
    "Claude Code",
    SignInFlowKind::Bridge,
)
.with_agent_sign_in(
    AgentSignIn::new(
        &["claude", "claude-code", "cc"],
        "CLAUDE_CONFIG_DIR",
        ".claude",
    )
    .with_sign_in_spends(true),
)
.with_daily_warmup(true);

/// Registers the Claude Code subscription.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().subscription(ClaudeFactory)
}

/// Builds Claude Code subscription instances.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ClaudeFactory;

/// The validated config.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct ClaudeConfig {
    /// An absolute path to `claude`, instead of looking it up on `PATH`.
    command: Option<PathBuf>,
    /// Cap on concurrent Claude Code processes; `None` uses the memory heuristic.
    runs_most: Option<usize>,
}

impl ValidConfig for ClaudeConfig {}

impl SubscriptionFactory for ClaudeFactory {
    type Config = ClaudeConfig;

    fn spec(&self) -> &'static SubscriptionSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![
                ("command".to_owned(), Schema::string()),
                ("claude-runs-most".to_owned(), Schema::integer()),
            ],
            &[],
        )
        .expect("the command schema is valid")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        hofund_plugin_sdk::daily_warmup::read_times(raw)?;
        for key in raw.keys() {
            if key != "command"
                && key != "claude-runs-most"
                && key != hofund_plugin_sdk::daily_warmup::TIMES_KEY
            {
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
        let runs_most = match raw.get("claude-runs-most") {
            None => None,
            Some(RawValue::Integer(value)) if *value > 0 => Some(*value as usize),
            Some(RawValue::Integer(0)) => None,
            Some(RawValue::Integer(_)) => {
                return Err(
                    ConfigError::new("claude-runs-most must be a non-negative integer")
                        .with_path("claude-runs-most"),
                );
            }
            Some(_) => {
                return Err(ConfigError::new("claude-runs-most must be an integer")
                    .with_path("claude-runs-most"));
            }
        };
        Ok(ClaudeConfig { command, runs_most })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn Subscription>, BuildError> {
        Ok(Arc::new(Claude {
            command: cfg.command.clone(),
            flow: SignInFlow::Bridge { command: COMMAND },
            pending: Mutex::new(HashMap::new()),
            bridge: bridge::Bridge::with_room(bridge::room::Room::from_config(cfg.runs_most)),
        }))
    }
}

/// How long an unfinished sign-in may sit idle before its process is stopped.
#[cfg(not(test))]
const PENDING_IDLE: Duration = Duration::from_secs(15 * 60);

#[cfg(test)]
const PENDING_IDLE: Duration = Duration::from_millis(50);

fn pending_idle() -> Duration {
    PENDING_IDLE
}

/// A `claude auth login` waiting for its code.
struct Pending {
    command: launcher::ClaudeCommand,
    dir: PathBuf,
    child: PipedChild,
    stdout: BodyStream,
    last: String,
    started: Duration,
}

struct Claude {
    command: Option<PathBuf>,
    flow: SignInFlow,
    /// Sign-ins in progress, by session id.
    pending: Mutex<HashMap<String, Pending>>,
    /// Bridge run table, shared across `upstream()` providers for this instance.
    bridge: bridge::Bridge,
}

impl Claude {
    fn take_pending(&self, session: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
    }

    fn expire_idle(&self, host: &HostContext) {
        let now = host.clock().monotonic();
        let idle = pending_idle();
        let expired: Vec<String> = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, pending)| now.saturating_sub(pending.started) >= idle)
            .map(|(session, _)| session.clone())
            .collect();
        for session in expired {
            if let Some(pending) = self.take_pending(&session) {
                drop(pending.child);
                remove_dir(&pending.dir);
            }
        }
    }

    fn pending_expired(&self, ctx: &SignInCtx, host: &HostContext) -> bool {
        let now = host.clock().monotonic();
        let idle = pending_idle();
        let dir = {
            let guard = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(pending) = guard.get(ctx.session()) else {
                return false;
            };
            if now.saturating_sub(pending.started) < idle {
                return false;
            }
            pending.dir.clone()
        };
        if let Some(pending) = self.take_pending(ctx.session()) {
            drop(pending.child);
        }
        remove_dir(&dir);
        true
    }

    fn launcher(&self) -> Result<launcher::ClaudeCommand, PluginError> {
        launcher::ClaudeCommand::resolve(self.command.as_ref(), COMMAND)
    }
}

/// `<state>/signin/<session>`. The session is 32 hex digits.
fn sign_in_dir(host: &HostContext, ctx: &SignInCtx) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join("signin").join(ctx.session()))
}

fn remove_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Starts `claude auth login` in `dir` and reads up to its link.
async fn start_login(
    host: &HostContext,
    command: launcher::ClaudeCommand,
    dir: PathBuf,
) -> Result<(Pending, String), PluginError> {
    let request = command.spawn_request(&cli::LOGIN_ARGS, &dir, &[])?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let mut last = String::new();
    let link = hofund_plugin_sdk::timeout(LINK_WAIT, cli::read_link(&mut stdout, &mut last)).await;
    let Some(Some(link)) = link else {
        return Err(PluginError::auth(format!(
            "Claude Code did not print a sign-in link; {}",
            cli::said(&last)
        )));
    };
    Ok((
        Pending {
            command,
            dir,
            child,
            stdout,
            last,
            started: host.clock().monotonic(),
        },
        link,
    ))
}

/// What a pasted code must look like: `code#state`, with no spaces.
fn pasted_code(text: &str) -> Option<&str> {
    let code = text.trim();
    let (left, right) = code.split_once('#')?;
    let valid = !left.is_empty()
        && !right.is_empty()
        && !right.contains('#')
        && !code.chars().any(|ch| ch.is_whitespace() || ch.is_control());
    valid.then_some(code)
}

/// Hands Claude Code the proof, waits for it to exit, and reads who signed in.
async fn finish_login(
    host: &HostContext,
    pending: &mut Pending,
    proof: SignInProof,
) -> Result<(AccountId, String, SecretObject), PluginError> {
    let wait = match &proof {
        SignInProof::Token(code) => {
            let code = pasted_code(secret_text(code)).ok_or_else(|| {
                PluginError::config(
                    "paste the whole code the Claude page shows; it looks like code#state",
                )
            })?;
            pending
                .child
                .write_stdin(format!("{code}\n").into_bytes().into())
                .await?;
            CODE_WAIT
        }
        SignInProof::Await => BROWSER_WAIT,
        _ => {
            return Err(PluginError::config(
                "Claude Code sign-in takes the pasted code, or waits for the browser",
            ));
        }
    };
    let exited =
        hofund_plugin_sdk::timeout(wait, cli::drain(&mut pending.stdout, &mut pending.last)).await;
    if exited.is_none() {
        return Err(PluginError::auth(
            "Claude Code did not finish signing in in time",
        ));
    }
    let Some(credentials) = creds::read_credentials(&pending.dir) else {
        return Err(PluginError::auth(format!(
            "Claude Code did not sign in; {}",
            cli::said(&pending.last)
        )));
    };
    let mut profile = Profile::read(&pending.dir);
    if profile.email().is_none()
        && let Some(status) =
            account::status_command(host, &pending.command, &pending.dir, &SecretObject::new())
                .await
    {
        profile.add_status(&status);
    }
    let label = profile
        .email()
        .ok_or_else(|| PluginError::auth("Claude Code signed in but did not say which account"))?
        .to_owned();
    let id = profile.account_id()?;
    Ok((id, label, signed_in_blob(&credentials, &profile)))
}

fn signed_in_blob(credentials: &Credentials, profile: &Profile) -> SecretObject {
    let blob = creds::blob(credentials, profile);
    match &credentials.plan {
        Some(plan) => blob.with_text("plan", plan),
        None => blob,
    }
}

#[hofund_plugin_sdk::async_trait]
impl Subscription for Claude {
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
        self.expire_idle(host);
        let command = self.launcher()?;
        let dir = sign_in_dir(host, ctx)?;
        // A sign-in that restarts with the same session starts clean.
        drop(self.take_pending(ctx.session()));
        remove_dir(&dir);
        creds::private_dir(&dir)?;
        let (pending, link) = match start_login(host, command, dir.clone()).await {
            Ok(started) => started,
            Err(error) => {
                remove_dir(&dir);
                return Err(error);
            }
        };
        let challenge = SignInChallenge::instructions(format!(
            "Open this address in a browser and sign in to Claude:\n  {link}\n\
             Then paste the code the page shows."
        ));
        if challenge.is_ok() {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(ctx.session().to_owned(), pending);
        } else {
            drop(pending);
            remove_dir(&dir);
        }
        challenge
    }

    async fn complete_sign_in(
        &self,
        ctx: &SignInCtx,
        proof: SignInProof,
        host: &HostContext,
    ) -> Result<AccountId, PluginError> {
        if self.pending_expired(ctx, host) {
            return Err(PluginError::config(
                "this Claude Code sign-in timed out; start again",
            ));
        }
        self.expire_idle(host);
        if let SignInProof::Token(token) = &proof
            && setup_token::normalize(secret_text(token)).is_ok()
        {
            return setup_token::store(host, secret_text(token)).await;
        }
        if self.pending_expired(ctx, host) {
            return Err(PluginError::config(
                "this Claude Code sign-in timed out; start again",
            ));
        }
        let Some(mut pending) = self.take_pending(ctx.session()) else {
            return Err(PluginError::config(
                "no Claude Code sign-in is in progress for this session",
            ));
        };
        let finished = finish_login(host, &mut pending, proof).await;
        let dir = pending.dir.clone();
        drop(pending);
        remove_dir(&dir);
        let (id, label, blob) = finished?;
        let account = host.accounts().account(id.clone(), label)?;
        host.accounts().upsert(account, blob.to_secret()).await?;
        Ok(id)
    }

    async fn cancel_sign_in(&self, ctx: &SignInCtx, host: &HostContext) -> Result<(), PluginError> {
        self.expire_idle(host);
        if let Some(pending) = self.take_pending(ctx.session()) {
            let dir = pending.dir.clone();
            drop(pending);
            remove_dir(&dir);
        } else if let Ok(dir) = sign_in_dir(host, ctx) {
            remove_dir(&dir);
        }
        Ok(())
    }

    async fn account_status(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<AccountStatus, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Ok(needs_sign_in(
                "no Claude Code sign-in is stored for this account",
            ));
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            return Ok(needs_sign_in(
                "the stored Claude Code sign-in is unreadable",
            ));
        };
        if blob.flag(creds::LAPSED) {
            return Ok(needs_sign_in(creds::LAPSED_REASON));
        }
        if setup_token::is_setup_token(&blob) {
            return Ok(AccountStatus::Ready);
        }
        let credentials = blob
            .text(creds::CREDENTIALS)
            .and_then(creds::parse_credentials);
        match credentials {
            Some(credentials) if credentials.refreshable => Ok(AccountStatus::Ready),
            _ => Ok(needs_sign_in(
                "the stored Claude Code sign-in has no refresh token",
            )),
        }
    }

    async fn refresh(&self, account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
        account::refresh(host, || self.launcher(), account).await
    }

    async fn quota(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Vec<QuotaWindow>, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Claude Code sign-in is stored for this account",
            ));
        };
        let blob = SecretObject::parse(&secret)?;
        if setup_token::is_setup_token(&blob) {
            return Err(PluginError::unsupported(usage::SETUP_TOKEN_QUOTA));
        }
        account::read_usage(host, || self.launcher(), account).await
    }

    async fn list_models(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<ModelList, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Claude Code sign-in is stored for this account",
            ));
        };
        let _blob = SecretObject::parse(&secret)?;
        let dir = account_dir(host, account)?;
        creds::private_dir(&dir)?;
        let quota = Vec::new();
        models::list_for_account(host, &dir, &quota)
    }

    fn upstream(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
        Ok(std::sync::Arc::new(upstream::ClaudeUpstream::new(
            account.clone(),
            host.clone(),
            self.command.clone(),
            self.bridge.clone(),
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

    async fn import_accounts(
        &self,
        text: &str,
        host: &HostContext,
    ) -> Result<Vec<AccountId>, PluginError> {
        import::store(host, text).await
    }

    async fn daily_warmup(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        account::warmup(host, || self.launcher(), account).await
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

    async fn remove_account(
        &self,
        account: &AccountId,
        host: &HostContext,
    ) -> Result<(), PluginError> {
        remove_tree(host, account)
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
