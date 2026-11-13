use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_control_proto::{
    AccountDiscoverImportResponse, AccountDiscoverResponse, AccountImportResponse,
    AccountRemoveResponse, AccountSwitchResponse, ControlError, ControlErrorKind,
    DiscoveredAccount, EventPayload, SignInCancelResponse, SignInChallenge as WireChallenge,
    SignInProgressEvent, SignInProofRequest, SignInSessionState, SignInStartRequest,
    SignInStartResponse, SignInStatusResponse, SubscriptionAccount, SubscriptionEntry,
    SubscriptionFlow, SubscriptionListResponse, SubscriptionRefreshResponse,
};
use hofund_gateway::control::ControlFailure;
use hofund_plugin_api::{SignInChallenge, SignInCtx, SignInFlowKind};
use hofund_plugin_host::LoopbackRegistry;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::events::EventBus;
use crate::runtime::Runtime;
use crate::sign_in::{Opened, SignInOptions};
use crate::{DaemonError, DaemonRegistry, account_discovery, agent_login, sign_in};

const START_CHALLENGE_WAIT: Duration = Duration::from_secs(2);

pub(crate) struct AccountOps {
    runtime: Arc<Runtime>,
    registry: Arc<dyn DaemonRegistry>,
    loopback: Arc<LoopbackRegistry>,
    events: Arc<EventBus>,
    sessions: AsyncMutex<HashMap<String, Arc<ControlSession>>>,
    deadline: Duration,
}

struct ControlSession {
    subscription: String,
    ctx: SignInCtx,
    state: Mutex<SignInSessionState>,
    challenge: Mutex<Option<WireChallenge>>,
    paste: tokio::sync::mpsc::UnboundedSender<String>,
    cancel: Mutex<Option<oneshot::Sender<()>>>,
}

impl ControlSession {
    fn id(&self) -> &str {
        self.ctx.session()
    }

    fn state(&self) -> SignInSessionState {
        *self.state.lock().expect("sign-in state")
    }

    fn active(&self) -> bool {
        matches!(
            self.state(),
            SignInSessionState::Pending | SignInSessionState::NeedsProof
        )
    }

    fn signal_cancel(&self) {
        if let Some(cancel) = self.cancel.lock().expect("sign-in cancel").take() {
            let _ = cancel.send(());
        }
    }

    fn response(&self) -> SignInStatusResponse {
        let state = self.state();
        let challenge = self.challenge.lock().expect("sign-in challenge").clone();
        let challenge = challenge.filter(|challenge| {
            state == SignInSessionState::NeedsProof
                || (state == SignInSessionState::Pending
                    && matches!(challenge, WireChallenge::DeviceCode { .. }))
        });
        let mut response = SignInStatusResponse::default();
        response.state = state;
        response.challenge = challenge;
        response
    }
}

impl AccountOps {
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        registry: Arc<dyn DaemonRegistry>,
        events: Arc<EventBus>,
        deadline: Duration,
    ) -> Self {
        Self {
            runtime,
            registry,
            loopback: Arc::new(LoopbackRegistry::new()),
            events,
            sessions: AsyncMutex::new(HashMap::new()),
            deadline,
        }
    }

    fn opened(&self) -> Vec<Opened> {
        sign_in::open_built(
            self.runtime.subscription_states(),
            &self.registry,
            &self.loopback,
        )
    }

    pub(crate) async fn list(&self) -> Result<SubscriptionListResponse, ControlFailure> {
        let listings = sign_in::list_in(&self.opened())
            .await
            .map_err(account_failure)?;
        let mut response = SubscriptionListResponse::default();
        response.subscriptions = listings
            .into_iter()
            .map(|listing| {
                let mut entry = SubscriptionEntry::default();
                entry.id = listing.id;
                entry.name = listing.name;
                entry.flow = flow_from_name(listing.flow);
                entry.accounts = listing
                    .accounts
                    .into_iter()
                    .map(|account| {
                        let mut row = SubscriptionAccount::default();
                        row.id = account.id;
                        row.label = account.label;
                        row.ready = account.needs_sign_in.is_none();
                        row.needs_sign_in = account.needs_sign_in;
                        row
                    })
                    .collect();
                entry
            })
            .collect();
        Ok(response)
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        id: String,
        request: SignInStartRequest,
    ) -> Result<SignInStartResponse, ControlFailure> {
        let opened = self.opened();
        let entry = sign_in::find(&opened, &id).map_err(|_| not_found("subscription"))?;
        let flow = flow(entry.instance.flow().kind());
        let options = SignInOptions {
            host: request.host,
            label: request.label,
            site: request.site,
        };
        let ctx = sign_in::sign_in_ctx(entry.instance.id(), &options)
            .map_err(|_| invalid("sign-in options were rejected"))?;
        let session_id = ctx.session().to_owned();
        let mut sessions = self.sessions.lock().await;
        if sessions
            .values()
            .any(|session| session.subscription == id && session.active())
        {
            return Err(conflict("this subscription already has an active sign-in"));
        }
        let (paste, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (challenge_sender, mut challenge_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, interrupt) = oneshot::channel();
        let session = Arc::new(ControlSession {
            subscription: id,
            ctx: ctx.clone(),
            state: Mutex::new(SignInSessionState::Pending),
            challenge: Mutex::new(None),
            paste,
            cancel: Mutex::new(Some(cancel)),
        });
        sessions.insert(session_id.clone(), Arc::clone(&session));
        drop(sessions);
        self.emit(&session, SignInSessionState::Pending);

        let subscription = Arc::clone(&entry.instance);
        let host = entry.host.clone();
        let loopback = Arc::clone(&self.loopback);
        let task_session = Arc::clone(&session);
        let ops = Arc::clone(self);
        let deadline = self.deadline;
        let paste = Box::pin(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move { receiver.recv().await.map(|line| (line, receiver)) },
        ));
        tokio::spawn(async move {
            let interrupt = async {
                let _ = interrupt.await;
            };
            let mut out = std::io::sink();
            let result = sign_in::drive_sign_in_with_challenge(
                subscription.as_ref(),
                &host,
                &loopback,
                ctx,
                paste,
                interrupt,
                deadline,
                &mut out,
                &challenge_sender,
            )
            .await;
            let terminal = if result.is_ok() {
                SignInSessionState::Ready
            } else if task_session.state() == SignInSessionState::Cancelled {
                SignInSessionState::Cancelled
            } else {
                SignInSessionState::Failed
            };
            ops.set_state(&task_session, terminal);
            if result.is_ok() {
                ops.runtime.refresh_subscriptions().await;
            }
        });

        let challenge =
            match tokio::time::timeout(START_CHALLENGE_WAIT, challenge_receiver.recv()).await {
                Ok(Some(challenge)) => challenge,
                Ok(None) | Err(_) => {
                    session.signal_cancel();
                    self.set_state(&session, SignInSessionState::Failed);
                    return Err(ControlError::new(
                        ControlErrorKind::Unavailable,
                        "the sign-in could not be started",
                        "accounts",
                    )
                    .into());
                }
            };
        let challenge = wire_challenge(challenge);
        let state = match challenge {
            WireChallenge::OpenUrl { .. } | WireChallenge::Instructions { .. } => {
                SignInSessionState::NeedsProof
            }
            WireChallenge::DeviceCode { .. } => SignInSessionState::Pending,
            _ => SignInSessionState::Failed,
        };
        if state == SignInSessionState::Failed {
            session.signal_cancel();
            self.set_state(&session, state);
            return Err(invalid(
                "the plugin returned an unsupported sign-in challenge",
            ));
        }
        *session.challenge.lock().expect("sign-in challenge") = Some(challenge.clone());
        if session.state() == SignInSessionState::Pending {
            self.set_state(&session, state);
        }
        let mut response = SignInStartResponse::default();
        response.session = session_id;
        response.flow = flow;
        response.challenge = challenge;
        Ok(response)
    }

    pub(crate) async fn status(
        &self,
        subscription: &str,
        id: &str,
    ) -> Result<SignInStatusResponse, ControlFailure> {
        let session = self.session(subscription, id).await?;
        Ok(session.response())
    }

    pub(crate) async fn proof(
        &self,
        subscription: &str,
        id: &str,
        request: SignInProofRequest,
    ) -> Result<SignInStatusResponse, ControlFailure> {
        let session = self.session(subscription, id).await?;
        if session.state() != SignInSessionState::NeedsProof {
            return Err(conflict("this sign-in is not accepting proof"));
        }
        let challenge = session.challenge.lock().expect("sign-in challenge").clone();
        match (challenge, request.url, request.token) {
            (Some(WireChallenge::OpenUrl { .. }), Some(url), None) => {
                self.loopback
                    .submit_pasted_callback(session.id(), &url)
                    .map_err(|_| invalid("the pasted callback was rejected"))?;
            }
            (Some(WireChallenge::Instructions { .. }), None, Some(token)) => {
                session
                    .paste
                    .send(token.trim().to_owned())
                    .map_err(|_| conflict("this sign-in has already finished"))?;
            }
            _ => return Err(invalid("the proof does not match this sign-in challenge")),
        }
        if session.state() == SignInSessionState::NeedsProof {
            self.set_state(&session, SignInSessionState::Pending);
        }
        Ok(session.response())
    }

    pub(crate) async fn cancel(
        &self,
        subscription: &str,
        id: &str,
    ) -> Result<SignInCancelResponse, ControlFailure> {
        let session = self.session(subscription, id).await?;
        session.signal_cancel();
        self.set_state(&session, SignInSessionState::Cancelled);
        Ok(SignInCancelResponse::default())
    }

    pub(crate) async fn refresh(
        &self,
        id: String,
        account: Option<String>,
    ) -> Result<SubscriptionRefreshResponse, ControlFailure> {
        let opened = self.opened();
        sign_in::find(&opened, &id).map_err(|_| not_found("subscription"))?;
        let refreshed = sign_in::refresh_in(&opened, &id, account.as_deref())
            .await
            .map_err(account_failure)?;
        self.runtime.refresh_subscriptions().await;
        let mut response = SubscriptionRefreshResponse::default();
        response.refreshed = refreshed;
        Ok(response)
    }

    pub(crate) async fn switch(
        &self,
        agent: String,
        target: String,
    ) -> Result<AccountSwitchResponse, ControlFailure> {
        let opened = self.opened();
        let config = self.runtime.config_updates().borrow().clone();
        let state_root = opened
            .iter()
            .find_map(|entry| entry.host.dirs().state_dir().ok())
            .ok_or_else(|| invalid("the daemon state directory is unavailable"))?;
        let account = agent_login::accounts_switch(
            &config,
            self.registry.as_ref(),
            &state_root,
            &opened,
            &agent,
            &target,
        )
        .await
        .map_err(account_failure)?;
        let mut response = AccountSwitchResponse::default();
        response.agent = agent;
        response.account = account;
        Ok(response)
    }

    pub(crate) async fn import(
        &self,
        path: String,
    ) -> Result<AccountImportResponse, ControlFailure> {
        let text = std::fs::read_to_string(Path::new(&path))
            .map_err(|_| invalid("the account export could not be read"))?;
        let imported = agent_login::accounts_import(&self.opened(), &text)
            .await
            .map_err(account_failure)?;
        self.runtime.refresh_subscriptions().await;
        let mut response = AccountImportResponse::default();
        response.imported = imported;
        Ok(response)
    }

    pub(crate) async fn discover(&self, add: bool) -> Result<DiscoveryOutcome, ControlFailure> {
        if account_discovery::discovery_blocked() {
            return Err(invalid("account discovery is disabled for this run"));
        }
        let config = self.runtime.config_updates().borrow().clone();
        let report = account_discovery::discover_opened(
            &self.opened(),
            self.registry.as_ref(),
            &config,
            add,
        )
        .await
        .map_err(account_failure)?;
        if add {
            self.runtime.refresh_subscriptions().await;
        }
        Ok(DiscoveryOutcome {
            found: report
                .lines
                .into_iter()
                .map(|line| {
                    let mut row = DiscoveredAccount::default();
                    row.subscription = line.subscription;
                    row.label = line.label;
                    row.already_stored = line.already_stored;
                    row
                })
                .collect(),
            imported: report.imported,
        })
    }

    pub(crate) async fn remove(
        &self,
        id: String,
        account: String,
    ) -> Result<AccountRemoveResponse, ControlFailure> {
        let opened = self.opened();
        sign_in::find(&opened, &id).map_err(|_| not_found("subscription"))?;
        let config = self.runtime.config_updates().borrow().clone();
        sign_in::sign_out_in(&opened, &config, &id, &account)
            .await
            .map_err(account_failure)?;
        self.runtime.refresh_subscriptions().await;
        let mut response = AccountRemoveResponse::default();
        response.removed = true;
        Ok(response)
    }

    async fn session(
        &self,
        subscription: &str,
        id: &str,
    ) -> Result<Arc<ControlSession>, ControlFailure> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .filter(|session| session.subscription == subscription)
            .ok_or_else(|| not_found("sign-in session"))
    }

    fn set_state(&self, session: &Arc<ControlSession>, state: SignInSessionState) {
        let previous = {
            let mut current = session.state.lock().expect("sign-in state");
            let previous = *current;
            *current = state;
            previous
        };
        if previous != state {
            self.emit(session, state);
        }
    }

    fn emit(&self, session: &ControlSession, state: SignInSessionState) {
        let mut event = SignInProgressEvent::default();
        event.provider = session.subscription.clone();
        event.session = Some(session.id().to_owned());
        event.state = Some(state_name(state).to_owned());
        self.events.emit(EventPayload::SignInProgress(event));
    }
}

pub(crate) struct DiscoveryOutcome {
    pub(crate) found: Vec<DiscoveredAccount>,
    pub(crate) imported: Vec<String>,
}

impl From<DiscoveryOutcome> for AccountDiscoverResponse {
    fn from(outcome: DiscoveryOutcome) -> Self {
        let mut response = Self::default();
        response.found = outcome.found;
        response
    }
}

impl From<DiscoveryOutcome> for AccountDiscoverImportResponse {
    fn from(outcome: DiscoveryOutcome) -> Self {
        let mut response = Self::default();
        response.imported = outcome.imported;
        response
    }
}

fn flow(kind: SignInFlowKind) -> SubscriptionFlow {
    match kind {
        SignInFlowKind::Pkce => SubscriptionFlow::Pkce,
        SignInFlowKind::Device => SubscriptionFlow::Device,
        SignInFlowKind::TokenFile => SubscriptionFlow::TokenFile,
        SignInFlowKind::Bridge => SubscriptionFlow::Bridge,
        _ => SubscriptionFlow::Unknown,
    }
}

fn flow_from_name(name: &str) -> SubscriptionFlow {
    match name {
        "pkce" => SubscriptionFlow::Pkce,
        "device" => SubscriptionFlow::Device,
        "token-file" => SubscriptionFlow::TokenFile,
        "bridge" => SubscriptionFlow::Bridge,
        _ => SubscriptionFlow::Unknown,
    }
}

fn wire_challenge(challenge: SignInChallenge) -> WireChallenge {
    match challenge {
        SignInChallenge::OpenUrl { url } => WireChallenge::OpenUrl { url },
        SignInChallenge::DeviceCode {
            user_code,
            verification_url,
        } => WireChallenge::DeviceCode {
            user_code,
            verification_url,
        },
        SignInChallenge::Instructions { text } => WireChallenge::Instructions { text },
        _ => WireChallenge::Instructions {
            text: String::new(),
        },
    }
}

fn state_name(state: SignInSessionState) -> &'static str {
    match state {
        SignInSessionState::Pending => "pending",
        SignInSessionState::NeedsProof => "needs_proof",
        SignInSessionState::Ready => "ready",
        SignInSessionState::Failed => "failed",
        SignInSessionState::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn not_found(kind: &str) -> ControlFailure {
    ControlError::new(
        ControlErrorKind::NotFound,
        format!("unknown {kind}"),
        "accounts",
    )
    .into()
}

fn conflict(message: &str) -> ControlFailure {
    ControlError::new(ControlErrorKind::Conflict, message, "accounts").into()
}

fn invalid(message: &str) -> ControlFailure {
    ControlError::new(ControlErrorKind::Unprocessable, message, "accounts").into()
}

fn account_failure(_error: DaemonError) -> ControlFailure {
    invalid("the account operation failed")
}
