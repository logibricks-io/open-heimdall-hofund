//! The `hofund subscription` commands: the sign-in driver, account listing,
//! sign-out, refresh, and quota.
//!
//! Each command loads the config, builds every `[[subscriptions]]` entry the
//! way the daemon does, and calls the plugin in process. The account file is
//! the one a running daemon rereads, so a sign-in or sign-out shows up as a
//! route member change within one account tick.
//!
//! The sign-in driver never opens a browser. It prints what the plugin's
//! challenge asks for and waits for the first of: the loopback callback, a
//! pasted address or token, Ctrl-C, or [`SIGN_IN_DEADLINE`]. Every exit path,
//! success included, calls `cancel_sign_in` and releases the loopback
//! listener.

use std::future::Future;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::{Stream, StreamExt};
use hofund_config::load_path;
use hofund_plugin_api::{
    AccountId, AccountStatus, HostContext, PluginId, SignInChallenge, SignInCtx, SignInFlowKind,
    SignInProof, Subscription,
};
use hofund_plugin_host::LoopbackRegistry;
use serde_json::json;

use crate::registry::DaemonRegistry;
use crate::{DaemonError, RegistryAdapter, show, subscriptions};

/// How long one sign-in may wait for the user before it fails.
pub const SIGN_IN_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// The exit code for a sign-in the user interrupted with Ctrl-C.
pub const INTERRUPTED_EXIT: u8 = 130;

/// How long the driver waits for a plugin's `cancel_sign_in`.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a sign-in did not finish.
#[derive(Debug)]
pub struct SignInError {
    message: String,
    interrupted: bool,
}

impl SignInError {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            interrupted: false,
        }
    }

    fn interrupted() -> Self {
        Self {
            message: "sign-in interrupted".to_owned(),
            interrupted: true,
        }
    }

    /// Whether the user pressed Ctrl-C.
    #[must_use]
    pub const fn is_interrupted(&self) -> bool {
        self.interrupted
    }

    /// The process exit code: [`INTERRUPTED_EXIT`] after Ctrl-C, otherwise 1.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        if self.interrupted {
            INTERRUPTED_EXIT
        } else {
            1
        }
    }
}

impl std::fmt::Display for SignInError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SignInError {}

impl From<DaemonError> for SignInError {
    fn from(error: DaemonError) -> Self {
        Self::failed(error.to_string())
    }
}

/// Per-sign-in choices from the command line. The plugin reads them from
/// its [`SignInCtx`] as the `host` and `label` options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignInOptions {
    /// Vendor host, such as an enterprise domain.
    pub host: Option<String>,
    /// Account label to store.
    pub label: Option<String>,
}

/// One configured subscription and its accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionListing {
    /// Subscription plugin id.
    pub id: String,
    /// The plugin's display name.
    pub name: String,
    /// The sign-in flow: `pkce`, `device`, `token-file`, or `bridge`.
    pub flow: &'static str,
    /// Accounts ordered by label and then id.
    pub accounts: Vec<AccountListing>,
}

/// One signed-in account. It carries no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountListing {
    /// Account id.
    pub id: String,
    /// Account label.
    pub label: String,
    /// `None` when the account can serve calls, or why it cannot.
    pub needs_sign_in: Option<String>,
}

/// The quota a plugin reported for one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaListing {
    /// Subscription plugin id.
    pub subscription: String,
    /// Account id.
    pub account: String,
    /// The windows, or why the plugin could not report them.
    pub windows: Result<Vec<QuotaLine>, String>,
}

/// One quota window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaLine {
    /// Window name.
    pub name: String,
    /// Units used.
    pub used: u64,
    /// Units allowed, when known.
    pub limit: Option<u64>,
    /// Seconds until the window resets, when known.
    pub resets_in_secs: Option<u64>,
    /// Whether a spent window stops calls.
    pub blocking: bool,
    /// The one model the window applies to, when scoped.
    pub scope_model: Option<String>,
}

/// One built subscription with the host its commands use.
pub(crate) struct Opened {
    pub(crate) id: String,
    name: String,
    pub(crate) instance: Arc<dyn Subscription>,
    pub(crate) host: HostContext,
}

/// Builds every configured subscription with a loopback-capable host.
fn open(
    path: &Path,
    registry: &Arc<dyn DaemonRegistry>,
    loopback: &Arc<LoopbackRegistry>,
) -> Result<Vec<Opened>, DaemonError> {
    let config = load_path(path).map_err(show)?;
    let built = subscriptions::build(&config, path, None, registry, None)?;
    let mut opened = Vec::with_capacity(built.len());
    for (id, state) in built {
        let name = registry.subscription(&id).map_or_else(
            || id.clone(),
            |factory| factory.spec().display_name().to_owned(),
        );
        let host = state
            .host()
            .clone()
            .with_loopback(Arc::clone(loopback) as _);
        opened.push(Opened {
            id,
            name,
            instance: Arc::clone(state.instance()),
            host,
        });
    }
    Ok(opened)
}

pub(crate) fn find<'a>(opened: &'a [Opened], id: &str) -> Result<&'a Opened, DaemonError> {
    opened
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| DaemonError::new(format!("no subscription `{id}` in the config")))
}

fn runtime() -> Result<tokio::runtime::Runtime, DaemonError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(show)
}

pub(crate) fn with_opened<T>(
    path: &Path,
    run: impl AsyncFnOnce(&[Opened]) -> Result<T, DaemonError>,
) -> Result<T, DaemonError> {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let loopback = Arc::new(LoopbackRegistry::new());
    let opened = open(path, &registry, &loopback)?;
    runtime()?.block_on(run(&opened))
}

/// Lists each configured subscription and its accounts.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config does not load, a subscription
/// does not build, or the account file cannot be read.
pub fn subscription_list(path: &Path) -> Result<Vec<SubscriptionListing>, DaemonError> {
    with_opened(path, async |opened| list_in(opened).await)
}

async fn list_in(opened: &[Opened]) -> Result<Vec<SubscriptionListing>, DaemonError> {
    let mut listings = Vec::with_capacity(opened.len());
    for entry in opened {
        let mut accounts = Vec::new();
        for account in sorted_accounts(entry).await? {
            let needs_sign_in = match entry.instance.account_status(&account.0, &entry.host).await {
                Ok(AccountStatus::Ready) => None,
                Ok(AccountStatus::NeedsSignIn { reason }) => Some(reason),
                Ok(_) => Some("unknown account status".to_owned()),
                Err(error) => Some(error.to_string()),
            };
            accounts.push(AccountListing {
                id: account.0.to_string(),
                label: account.1,
                needs_sign_in,
            });
        }
        listings.push(SubscriptionListing {
            id: entry.id.clone(),
            name: entry.name.clone(),
            flow: flow_name(entry.instance.flow().kind()),
            accounts,
        });
    }
    Ok(listings)
}

/// Account ids and labels, ordered by label and then id.
pub(crate) async fn sorted_accounts(
    entry: &Opened,
) -> Result<Vec<(AccountId, String)>, DaemonError> {
    let mut accounts: Vec<(AccountId, String)> = entry
        .host
        .accounts()
        .list()
        .await
        .map_err(show)?
        .into_iter()
        .map(|account| (account.id().clone(), account.label().to_owned()))
        .collect();
    accounts.sort_by(|left, right| {
        (left.1.as_str(), left.0.as_str()).cmp(&(right.1.as_str(), right.0.as_str()))
    });
    Ok(accounts)
}

const fn flow_name(kind: SignInFlowKind) -> &'static str {
    match kind {
        SignInFlowKind::Pkce => "pkce",
        SignInFlowKind::Device => "device",
        SignInFlowKind::TokenFile => "token-file",
        SignInFlowKind::Bridge => "bridge",
        _ => "other",
    }
}

/// Renders `subscription list --json`. Only ids, labels, the flow, and the
/// account status appear; no secret field exists to render.
#[must_use]
pub fn subscriptions_json(listings: &[SubscriptionListing]) -> String {
    let value: Vec<_> = listings
        .iter()
        .map(|listing| {
            json!({
                "id": listing.id,
                "name": listing.name,
                "flow": listing.flow,
                "accounts": listing.accounts.iter().map(|account| json!({
                    "id": account.id,
                    "label": account.label,
                    "ready": account.needs_sign_in.is_none(),
                    "needs_sign_in": account.needs_sign_in,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::Value::Array(value).to_string()
}

/// Removes `account` from subscription `id`. A running daemon drops its
/// route member on the next account tick.
///
/// # Errors
///
/// Returns [`DaemonError`] when the subscription or account is unknown or
/// the account file cannot be written.
pub fn subscription_sign_out(path: &Path, id: &str, account: &str) -> Result<(), DaemonError> {
    let config = load_path(path).map_err(show)?;
    with_opened(path, async |opened| {
        sign_out_in(opened, &config, id, account).await
    })
}

async fn sign_out_in(
    opened: &[Opened],
    config: &hofund_config::Config,
    id: &str,
    account: &str,
) -> Result<(), DaemonError> {
    let entry = find(opened, id)?;
    let known = sorted_accounts(entry).await?;
    let Some((target, _)) = known.iter().find(|(known, _)| known.as_str() == account) else {
        return Err(DaemonError::new(format!(
            "subscription `{id}` has no account `{account}`"
        )));
    };
    if crate::RegistryAdapter.agent_sign_in(id).is_some() {
        let state_root = entry.host.dirs().state_dir().map_err(show)?;
        crate::agent_login::switch_before_forget(
            opened,
            &crate::RegistryAdapter,
            config,
            &state_root,
            id,
            account,
        )
        .await?;
    }
    entry.host.accounts().remove(target).await.map_err(show)
}

/// Refreshes the tokens of one account, or of every account of `id`.
/// Returns the refreshed account ids.
///
/// # Errors
///
/// Returns [`DaemonError`] when the subscription or account is unknown, or
/// any refresh failed. The message names each failed account.
pub fn subscription_refresh(
    path: &Path,
    id: &str,
    account: Option<&str>,
) -> Result<Vec<String>, DaemonError> {
    with_opened(path, async |opened| refresh_in(opened, id, account).await)
}

async fn refresh_in(
    opened: &[Opened],
    id: &str,
    account: Option<&str>,
) -> Result<Vec<String>, DaemonError> {
    let entry = find(opened, id)?;
    let mut targets: Vec<AccountId> = sorted_accounts(entry)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if let Some(account) = account {
        targets.retain(|known| known.as_str() == account);
        if targets.is_empty() {
            return Err(DaemonError::new(format!(
                "subscription `{id}` has no account `{account}`"
            )));
        }
    }
    let mut refreshed = Vec::new();
    let mut failed = Vec::new();
    for target in targets {
        match entry.instance.refresh(&target, &entry.host).await {
            Ok(()) => refreshed.push(target.to_string()),
            Err(error) => failed.push(format!("{target}: {error}")),
        }
    }
    if failed.is_empty() {
        Ok(refreshed)
    } else {
        Err(DaemonError::new(format!(
            "refresh failed for {}",
            failed.join("; ")
        )))
    }
}

/// Reads the quota of every account, for one subscription or all of them.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config does not load, `id` is unknown,
/// or the account file cannot be read. A plugin quota failure is reported
/// per account in [`QuotaListing::windows`].
pub fn subscription_quota(path: &Path, id: Option<&str>) -> Result<Vec<QuotaListing>, DaemonError> {
    with_opened(path, async |opened| quota_in(opened, id).await)
}

async fn quota_in(opened: &[Opened], id: Option<&str>) -> Result<Vec<QuotaListing>, DaemonError> {
    let selected: Vec<&Opened> = match id {
        Some(id) => vec![find(opened, id)?],
        None => opened.iter().collect(),
    };
    let now = SystemTime::now();
    let mut listings = Vec::new();
    for entry in selected {
        for (account, _) in sorted_accounts(entry).await? {
            let windows = entry
                .instance
                .quota(&account, &entry.host)
                .await
                .map(|windows| {
                    windows
                        .iter()
                        .map(|window| QuotaLine {
                            name: window.name().to_owned(),
                            used: window.used(),
                            limit: window.limit(),
                            resets_in_secs: window
                                .resets_at()
                                .map(|at| at.duration_since(now).map_or(0, |left| left.as_secs())),
                            blocking: window.blocking(),
                            scope_model: window.scope_model().map(str::to_owned),
                        })
                        .collect()
                })
                .map_err(|error| error.to_string());
            listings.push(QuotaListing {
                subscription: entry.id.clone(),
                account: account.to_string(),
                windows,
            });
        }
    }
    Ok(listings)
}

/// Renders `subscription quota --json`.
#[must_use]
pub fn quota_json(listings: &[QuotaListing]) -> String {
    let value: Vec<_> = listings
        .iter()
        .map(|listing| {
            let mut row = json!({
                "subscription": listing.subscription,
                "account": listing.account,
            });
            match &listing.windows {
                Ok(windows) => {
                    row["windows"] = windows
                        .iter()
                        .map(|window| {
                            json!({
                                "name": window.name,
                                "used": window.used,
                                "limit": window.limit,
                                "resets_in_secs": window.resets_in_secs,
                                "blocking": window.blocking,
                                "scope_model": window.scope_model,
                            })
                        })
                        .collect();
                }
                Err(error) => row["error"] = json!(error),
            }
            row
        })
        .collect();
    serde_json::Value::Array(value).to_string()
}

/// Signs in to subscription `id` from the terminal. Prompts go to stderr;
/// a pasted address or token is read from stdin. Returns the account id.
///
/// # Errors
///
/// Returns [`SignInError`] when the config or subscription is unknown, the
/// plugin refuses the sign-in, [`SIGN_IN_DEADLINE`] passes, or the user
/// presses Ctrl-C ([`SignInError::is_interrupted`]).
pub fn subscription_sign_in(
    path: &Path,
    id: &str,
    options: &SignInOptions,
) -> Result<String, SignInError> {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let loopback = Arc::new(LoopbackRegistry::new());
    let opened = open(path, &registry, &loopback)?;
    let entry = find(&opened, id)?;
    let ctx = sign_in_ctx(entry.instance.id(), options)?;
    let runtime = runtime()?;
    runtime.block_on(async {
        let paste = Box::pin(stdin_lines());
        let interrupt = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let mut out = std::io::stderr();
        drive_sign_in(
            entry.instance.as_ref(),
            &entry.host,
            &loopback,
            ctx,
            paste,
            interrupt,
            SIGN_IN_DEADLINE,
            &mut out,
        )
        .await
        .map(|account| account.to_string())
    })
}

fn sign_in_ctx(plugin: &PluginId, options: &SignInOptions) -> Result<SignInCtx, SignInError> {
    let failed = |error: hofund_plugin_api::PluginError| SignInError::failed(error.to_string());
    let mut ctx = hofund_plugin_host::sign_in_ctx(plugin.clone()).map_err(failed)?;
    if let Some(host) = &options.host {
        ctx = ctx.with_option("host", host.as_str()).map_err(failed)?;
    }
    if let Some(label) = &options.label {
        ctx = ctx.with_option("label", label.as_str()).map_err(failed)?;
    }
    Ok(ctx)
}

/// Lines from stdin. A detached thread reads them, so a blocked read does
/// not hold the runtime open after the sign-in ends.
fn stdin_lines() -> impl Stream<Item = String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|line| (line, receiver))
    })
}

/// Runs one sign-in to the end and cleans up.
///
/// The flow races `interrupt` and `deadline`. Whatever ends it, the driver
/// then calls `cancel_sign_in` and releases the session's loopback
/// listener.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's inputs are the seams tests replace"
)]
async fn drive_sign_in<P, I, W>(
    subscription: &dyn Subscription,
    host: &HostContext,
    loopback: &LoopbackRegistry,
    ctx: SignInCtx,
    mut paste: P,
    interrupt: I,
    deadline: Duration,
    out: &mut W,
) -> Result<AccountId, SignInError>
where
    P: Stream<Item = String> + Unpin,
    I: Future<Output = ()>,
    W: Write,
{
    let outcome = {
        let flow = run_flow(
            subscription,
            host,
            loopback,
            &ctx,
            &mut paste,
            deadline,
            out,
        );
        tokio::select! {
            biased;
            () = interrupt => Err(SignInError::interrupted()),
            result = tokio::time::timeout(deadline, flow) => {
                result.unwrap_or_else(|_| Err(SignInError::failed("the sign-in timed out")))
            }
        }
    };
    if let Ok(Err(error)) =
        tokio::time::timeout(CANCEL_TIMEOUT, subscription.cancel_sign_in(&ctx, host)).await
    {
        tracing::debug!(%error, "cancel_sign_in failed");
    }
    loopback.release(ctx.session());
    outcome
}

async fn run_flow<P, W>(
    subscription: &dyn Subscription,
    host: &HostContext,
    loopback: &LoopbackRegistry,
    ctx: &SignInCtx,
    paste: &mut P,
    deadline: Duration,
    out: &mut W,
) -> Result<AccountId, SignInError>
where
    P: Stream<Item = String> + Unpin,
    W: Write,
{
    let failed = |error: hofund_plugin_api::PluginError| SignInError::failed(error.to_string());
    let challenge = subscription
        .start_sign_in(ctx, host)
        .await
        .map_err(failed)?;
    let proof = match challenge {
        SignInChallenge::OpenUrl { url } => {
            say(
                out,
                &format!(
                    "Open this address in a browser:\n  {url}\n\
                     Waiting for the browser. If it cannot reach this machine, paste the \
                     address the browser ended on and press Enter."
                ),
            )?;
            loopback
                .await_callback(ctx.session(), paste, deadline)
                .await
                .map_err(failed)?
        }
        SignInChallenge::DeviceCode {
            user_code,
            verification_url,
        } => {
            say(
                out,
                &format!(
                    "Go to {verification_url} and enter the code {user_code}\n\
                     Waiting for the approval."
                ),
            )?;
            SignInProof::Await
        }
        SignInChallenge::Instructions { text } => {
            say(
                out,
                &format!("{text}\nPaste a token and press Enter, or press Enter to wait."),
            )?;
            match paste.next().await {
                Some(line) if !line.trim().is_empty() => SignInProof::token(line.trim().to_owned()),
                _ => SignInProof::Await,
            }
        }
        _ => {
            return Err(SignInError::failed(
                "this plugin asked for a sign-in step this hofund does not support",
            ));
        }
    };
    subscription
        .complete_sign_in(ctx, proof, host)
        .await
        .map_err(failed)
}

fn say<W: Write>(out: &mut W, text: &str) -> Result<(), SignInError> {
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|error| SignInError::failed(format!("cannot write the prompt: {error}")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::{Stream, stream};
    use hofund_plugin_api::{
        AccountId, AccountStatus, HostContext, LoopbackSpec, ModelList, PluginError, PluginId,
        PluginKind, QuotaWindow, SecretString, SignInChallenge, SignInCtx, SignInFlow, SignInProof,
        Subscription, UpstreamProvider,
    };
    use hofund_plugin_host::TestHost;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        INTERRUPTED_EXIT, Opened, SIGN_IN_DEADLINE, SignInError, SignInOptions, drive_sign_in,
        list_in, quota_in, quota_json, refresh_in, sign_in_ctx, sign_out_in, subscriptions_json,
    };

    const PLUGIN: &str = "fake-sub";
    const SECRET: &str = "sk-fake-secret-value";

    #[derive(Clone, Copy)]
    enum Mode {
        OpenUrl,
        Device,
        Instructions,
        Hang,
    }

    #[derive(Default)]
    struct Calls {
        cancels: AtomicUsize,
        proofs: Mutex<Vec<String>>,
        refreshes: Mutex<Vec<String>>,
        redirect: Mutex<Option<String>>,
    }

    struct Fake {
        id: PluginId,
        mode: Mode,
        flow: SignInFlow,
        calls: Arc<Calls>,
    }

    #[hofund_plugin_sdk::async_trait]
    impl Subscription for Fake {
        fn id(&self) -> &PluginId {
            &self.id
        }

        fn flow(&self) -> &SignInFlow {
            &self.flow
        }

        async fn start_sign_in(
            &self,
            ctx: &SignInCtx,
            host: &HostContext,
        ) -> Result<SignInChallenge, PluginError> {
            match self.mode {
                Mode::OpenUrl => {
                    let binding = host
                        .loopback()
                        .bind(ctx, &LoopbackSpec::ephemeral().expect_state("right"))?;
                    *self.calls.redirect.lock().expect("redirect") =
                        Some(binding.redirect_uri().to_owned());
                    Ok(SignInChallenge::OpenUrl {
                        url: "https://auth.example.test/authorize?state=right".to_owned(),
                    })
                }
                Mode::Device | Mode::Hang => Ok(SignInChallenge::DeviceCode {
                    user_code: "WDJB-MJHT".to_owned(),
                    verification_url: "https://example.test/device".to_owned(),
                }),
                Mode::Instructions => Ok(SignInChallenge::Instructions {
                    text: "Run the vendor login command.".to_owned(),
                }),
            }
        }

        async fn complete_sign_in(
            &self,
            _ctx: &SignInCtx,
            proof: SignInProof,
            _host: &HostContext,
        ) -> Result<AccountId, PluginError> {
            if matches!(self.mode, Mode::Hang) {
                std::future::pending::<()>().await;
            }
            let seen = match proof {
                SignInProof::Callback { state, .. } => format!("callback {state}"),
                SignInProof::Token(_) => "token".to_owned(),
                SignInProof::Await => "await".to_owned(),
                _ => "other".to_owned(),
            };
            self.calls.proofs.lock().expect("proofs").push(seen);
            Ok(AccountId::from_static("fake-account"))
        }

        async fn cancel_sign_in(
            &self,
            _ctx: &SignInCtx,
            _host: &HostContext,
        ) -> Result<(), PluginError> {
            self.calls.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn account_status(
            &self,
            account: &AccountId,
            _host: &HostContext,
        ) -> Result<AccountStatus, PluginError> {
            if account.as_str() == "lapsed" {
                Ok(AccountStatus::NeedsSignIn {
                    reason: "token lapsed".to_owned(),
                })
            } else {
                Ok(AccountStatus::Ready)
            }
        }

        async fn refresh(
            &self,
            account: &AccountId,
            _host: &HostContext,
        ) -> Result<(), PluginError> {
            self.calls
                .refreshes
                .lock()
                .expect("refreshes")
                .push(account.to_string());
            Ok(())
        }

        async fn quota(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<Vec<QuotaWindow>, PluginError> {
            Ok(vec![QuotaWindow::new("five-hour", 3, Some(10), None)?])
        }

        async fn list_models(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<ModelList, PluginError> {
            Ok(ModelList::new(Vec::new(), None))
        }

        fn upstream(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
            Err(PluginError::unsupported("fake-sub does not serve"))
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        test: TestHost,
        fake: Arc<Fake>,
        calls: Arc<Calls>,
    }

    fn new_rig(mode: Mode) -> Rig {
        let dir = tempfile::tempdir().expect("dir");
        let plugin = PluginId::from_static(PLUGIN);
        let test = TestHost::open_for(PluginKind::Subscription, plugin.clone(), dir.path());
        let calls = Arc::new(Calls::default());
        let fake = Arc::new(Fake {
            id: plugin,
            mode,
            flow: SignInFlow::Pkce,
            calls: Arc::clone(&calls),
        });
        Rig {
            _dir: dir,
            test,
            fake,
            calls,
        }
    }

    impl Rig {
        fn opened(&self) -> Vec<Opened> {
            vec![Opened {
                id: PLUGIN.to_owned(),
                name: "Fake Subscription".to_owned(),
                instance: Arc::clone(&self.fake) as Arc<dyn Subscription>,
                host: self.test.context().clone(),
            }]
        }

        fn ctx(&self) -> SignInCtx {
            sign_in_ctx(&PluginId::from_static(PLUGIN), &SignInOptions::default()).expect("ctx")
        }

        async fn add_account(&self, id: &str, label: &str) {
            let accounts = self.test.context().accounts();
            let account = accounts
                .account(AccountId::try_from(id).expect("id"), label)
                .expect("account");
            accounts
                .upsert(account, SecretString::from(SECRET.to_owned()))
                .await
                .expect("upsert");
        }

        async fn redirect(&self) -> String {
            loop {
                if let Some(uri) = self.calls.redirect.lock().expect("redirect").clone() {
                    return uri;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    /// A paste stream the test feeds while the driver waits.
    fn paste_channel() -> (
        tokio::sync::mpsc::UnboundedSender<String>,
        impl Stream<Item = String> + Unpin,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let lines = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|line| (line, receiver))
        });
        (sender, Box::pin(lines))
    }

    fn with_state(redirect: &str, state: &str) -> String {
        format!("{redirect}?code=the-code&state={state}")
    }

    fn port_of(redirect: &str) -> u16 {
        url::Url::parse(redirect)
            .expect("redirect")
            .port()
            .expect("port")
    }

    #[test]
    fn deadline_and_exit_codes() {
        assert_eq!(SIGN_IN_DEADLINE, Duration::from_secs(600));
        assert_eq!(INTERRUPTED_EXIT, 130);
        assert_eq!(SignInError::interrupted().exit_code(), 130);
        assert!(SignInError::interrupted().is_interrupted());
        assert_eq!(SignInError::failed("x").exit_code(), 1);
    }

    #[test]
    fn ctx_carries_host_and_label_options() {
        let options = SignInOptions {
            host: Some("corp.example.test".to_owned()),
            label: Some("work".to_owned()),
        };
        let ctx = sign_in_ctx(&PluginId::from_static(PLUGIN), &options).expect("ctx");
        assert_eq!(ctx.option("host"), Some("corp.example.test"));
        assert_eq!(ctx.option("label"), Some("work"));
        assert_eq!(ctx.session().len(), 32);
    }

    #[tokio::test]
    async fn sign_in_completes_from_loopback() {
        let rig = new_rig(Mode::OpenUrl);
        let ctx = rig.ctx();
        let session = ctx.session().to_owned();
        let (_sender, paste) = paste_channel();
        let mut out = Vec::new();
        let browser = async {
            let redirect = rig.redirect().await;
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port_of(&redirect)))
                .await
                .expect("connect");
            socket
                .write_all(
                    b"GET /callback?code=the-code&state=right HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("request");
            let mut response = Vec::new();
            let _ = socket.read_to_end(&mut response).await;
        };
        let (result, ()) = tokio::join!(
            drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                ctx,
                paste,
                std::future::pending::<()>(),
                Duration::from_secs(10),
                &mut out,
            ),
            browser,
        );
        assert_eq!(result.expect("signed in").as_str(), "fake-account");
        assert_eq!(
            *rig.calls.proofs.lock().expect("proofs"),
            vec!["callback right"]
        );
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
        assert_released(&rig, &session).await;
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("https://auth.example.test/authorize?state=right"));
        assert!(printed.contains("paste the address the browser ended on"));
    }

    #[tokio::test]
    async fn sign_in_completes_from_pasted_url() {
        let rig = new_rig(Mode::OpenUrl);
        let (sender, paste) = paste_channel();
        let mut out = Vec::new();
        let user = async {
            let redirect = rig.redirect().await;
            sender.send(with_state(&redirect, "right")).expect("paste");
        };
        let (result, ()) = tokio::join!(
            drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                rig.ctx(),
                paste,
                std::future::pending::<()>(),
                Duration::from_secs(10),
                &mut out,
            ),
            user,
        );
        assert_eq!(result.expect("signed in").as_str(), "fake-account");
        assert_eq!(
            *rig.calls.proofs.lock().expect("proofs"),
            vec!["callback right"]
        );
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pasted_url_from_other_session_rejected() {
        // A paste with another sign-in's state is skipped; the right one still wins.
        let rig = new_rig(Mode::OpenUrl);
        let (sender, paste) = paste_channel();
        let mut out = Vec::new();
        let user = async {
            let redirect = rig.redirect().await;
            sender.send(with_state(&redirect, "other")).expect("paste");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(rig.calls.proofs.lock().expect("proofs").is_empty());
            sender.send(with_state(&redirect, "right")).expect("paste");
        };
        let (result, ()) = tokio::join!(
            drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                rig.ctx(),
                paste,
                std::future::pending::<()>(),
                Duration::from_secs(10),
                &mut out,
            ),
            user,
        );
        assert!(result.is_ok());
        assert_eq!(
            *rig.calls.proofs.lock().expect("proofs"),
            vec!["callback right"]
        );

        // Only the other session's address: the deadline ends the sign-in.
        let rig = new_rig(Mode::OpenUrl);
        let (sender, paste) = paste_channel();
        let mut out = Vec::new();
        let user = async {
            let redirect = rig.redirect().await;
            sender.send(with_state(&redirect, "other")).expect("paste");
        };
        let (result, ()) = tokio::join!(
            drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                rig.ctx(),
                paste,
                std::future::pending::<()>(),
                Duration::from_millis(300),
                &mut out,
            ),
            user,
        );
        let error = result.expect_err("other session");
        assert!(!error.is_interrupted());
        assert_eq!(error.exit_code(), 1);
        assert!(rig.calls.proofs.lock().expect("proofs").is_empty());
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
    }

    async fn assert_released(rig: &Rig, session: &str) {
        let error = rig
            .test
            .loopback()
            .await_callback(
                session,
                stream::empty::<String>(),
                Duration::from_millis(10),
            )
            .await
            .expect_err("released session has no binding");
        assert!(error.to_string().contains("no loopback binding"), "{error}");
    }

    #[tokio::test]
    async fn ctrl_c_cancels_and_releases_port() {
        let rig = new_rig(Mode::OpenUrl);
        let ctx = rig.ctx();
        let session = ctx.session().to_owned();
        let (_sender, paste) = paste_channel();
        let mut out = Vec::new();
        let interrupt = async {
            rig.redirect().await;
        };
        let result = drive_sign_in(
            rig.fake.as_ref(),
            rig.test.context(),
            rig.test.loopback(),
            ctx,
            paste,
            interrupt,
            Duration::from_secs(10),
            &mut out,
        )
        .await;
        let error = result.expect_err("interrupted");
        assert!(error.is_interrupted());
        assert_eq!(error.exit_code(), INTERRUPTED_EXIT);
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
        assert!(rig.calls.proofs.lock().expect("proofs").is_empty());
        assert_released(&rig, &session).await;
        let port = port_of(&rig.redirect().await);
        let mut closed = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(closed, "the loopback port is still accepting");
    }

    #[tokio::test]
    async fn device_challenge_prints_code_and_waits() {
        let rig = new_rig(Mode::Device);
        let (_sender, paste) = paste_channel();
        let mut out = Vec::new();
        let result = drive_sign_in(
            rig.fake.as_ref(),
            rig.test.context(),
            rig.test.loopback(),
            rig.ctx(),
            paste,
            std::future::pending::<()>(),
            Duration::from_secs(10),
            &mut out,
        )
        .await;
        assert_eq!(result.expect("signed in").as_str(), "fake-account");
        assert_eq!(*rig.calls.proofs.lock().expect("proofs"), vec!["await"]);
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("WDJB-MJHT"), "{printed}");
        assert!(printed.contains("https://example.test/device"), "{printed}");
    }

    #[tokio::test]
    async fn instructions_send_a_pasted_token_or_await() {
        for (line, expected) in [("tok-123", "token"), ("   ", "await")] {
            let rig = new_rig(Mode::Instructions);
            let mut out = Vec::new();
            let result = drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                rig.ctx(),
                stream::iter(vec![line.to_owned()]),
                std::future::pending::<()>(),
                Duration::from_secs(10),
                &mut out,
            )
            .await;
            assert!(result.is_ok());
            assert_eq!(*rig.calls.proofs.lock().expect("proofs"), vec![expected]);
            let printed = String::from_utf8(out).expect("utf8");
            assert!(printed.contains("Run the vendor login command."));
        }
    }

    #[tokio::test]
    async fn deadline_fails_and_cancels() {
        let rig = new_rig(Mode::Hang);
        let (_sender, paste) = paste_channel();
        let mut out = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            drive_sign_in(
                rig.fake.as_ref(),
                rig.test.context(),
                rig.test.loopback(),
                rig.ctx(),
                paste,
                std::future::pending::<()>(),
                Duration::from_millis(100),
                &mut out,
            ),
        )
        .await
        .expect("the deadline ends the sign-in");
        let error = result.expect_err("timed out");
        assert!(!error.is_interrupted());
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(rig.calls.cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_json_has_no_secret_fields() {
        let rig = new_rig(Mode::Device);
        rig.add_account("lapsed", "Beta").await;
        rig.add_account("zeta", "Alpha").await;
        let listings = list_in(&rig.opened()).await.expect("list");
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].flow, "pkce");
        assert_eq!(listings[0].name, "Fake Subscription");
        let ids: Vec<&str> = listings[0].accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["zeta", "lapsed"], "ordered by label, then id");
        let json = subscriptions_json(&listings);
        assert!(!json.contains(SECRET), "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json");
        let subscription = value[0].as_object().expect("object");
        let keys: Vec<&str> = subscription.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["id", "name", "flow", "accounts"]);
        let account = value[0]["accounts"][0].as_object().expect("account");
        let keys: Vec<&str> = account.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["id", "label", "ready", "needs_sign_in"]);
        assert_eq!(value[0]["accounts"][0]["label"], "Alpha");
        assert_eq!(value[0]["accounts"][0]["ready"], true);
        assert!(value[0]["accounts"][0]["needs_sign_in"].is_null());
        assert_eq!(value[0]["accounts"][1]["ready"], false);
        assert_eq!(value[0]["accounts"][1]["needs_sign_in"], "token lapsed");
        assert_eq!(value[0]["id"], PLUGIN);
    }

    #[tokio::test]
    async fn sign_out_removes_account_and_member() {
        let rig = new_rig(Mode::Device);
        rig.add_account("one", "Alpha").await;
        rig.add_account("two", "Beta").await;
        let opened = rig.opened();
        let config = test_config();
        sign_out_in(&opened, &config, PLUGIN, "one")
            .await
            .expect("sign out");
        let listings = list_in(&opened).await.expect("list");
        let ids: Vec<&str> = listings[0].accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["two"]);
        assert!(sign_out_in(&opened, &config, PLUGIN, "one").await.is_err());
        assert!(
            sign_out_in(&opened, &config, "missing", "two")
                .await
                .is_err()
        );
    }

    fn test_config() -> hofund_config::Config {
        use hofund_config::{Accounts, Failover, Stays, Subscription};
        hofund_config::Config {
            listen: "127.0.0.1:3425".parse().expect("addr"),
            token_file: String::new(),
            usage_file: String::new(),
            failover: Failover::default(),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            providers: Vec::new(),
            routes: Vec::new(),
            subscriptions: vec![Subscription {
                id: PLUGIN.to_owned(),
                accounts: Accounts::All,
                account_weights: BTreeMap::new(),
                routing: "smart".to_owned(),
                at_once: None,
                stays: Stays::Auto,
                options: BTreeMap::new(),
            }],
            warnings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn refresh_forces_one_token_post() {
        let rig = new_rig(Mode::Device);
        rig.add_account("one", "Alpha").await;
        rig.add_account("two", "Beta").await;
        let opened = rig.opened();
        let refreshed = refresh_in(&opened, PLUGIN, Some("two"))
            .await
            .expect("refresh");
        assert_eq!(refreshed, vec!["two"]);
        assert_eq!(*rig.calls.refreshes.lock().expect("refreshes"), vec!["two"]);
        let refreshed = refresh_in(&opened, PLUGIN, None)
            .await
            .expect("refresh all");
        assert_eq!(refreshed, vec!["one", "two"]);
        assert_eq!(
            *rig.calls.refreshes.lock().expect("refreshes"),
            vec!["two", "one", "two"]
        );
        assert!(refresh_in(&opened, PLUGIN, Some("three")).await.is_err());
    }

    #[tokio::test]
    async fn quota_lists_each_account_window() {
        let rig = new_rig(Mode::Device);
        rig.add_account("one", "Alpha").await;
        let listings = quota_in(&rig.opened(), Some(PLUGIN)).await.expect("quota");
        assert_eq!(listings.len(), 1);
        let windows = listings[0].windows.as_ref().expect("windows");
        assert_eq!(windows[0].name, "five-hour");
        assert_eq!(windows[0].used, 3);
        assert_eq!(windows[0].limit, Some(10));
        let value: serde_json::Value = serde_json::from_str(&quota_json(&listings)).expect("json");
        assert_eq!(value[0]["windows"][0]["limit"], 10);
        assert!(quota_in(&rig.opened(), Some("missing")).await.is_err());
    }
}
