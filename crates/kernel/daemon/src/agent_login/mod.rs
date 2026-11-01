//! Agent sign-in switching for subscription-backed coding agents.
//!
//! Vendor file writes stay in the subscription plugins; this module keeps
//! policy, state, and orchestration.

mod logic;
mod state;

mod cli;

pub use cli::{accounts_import_in, accounts_switch_in};
pub use logic::{
    LoginPolicy, LoginQuota, LoginRow, account_by_name, next_in_order, next_login, next_on_forget,
};
pub use state::{AgentState, agent_home, policy_from_config, read, write};

use std::path::Path;
use std::time::Duration;

use hofund_config::Config;
use hofund_plugin_api::{AccountId, AgentSignIn};

use crate::registry::DaemonRegistry;
use crate::sign_in::{Opened, find, sorted_accounts};
use crate::subscriptions::Subscriptions;
use crate::{DaemonError, show};

/// How often an agent sign-in is moved when the current account is spent.
pub const LOGIN_SWITCH_EVERY: Duration = Duration::from_secs(5 * 60);

fn resolve_agent(
    registry: &dyn DaemonRegistry,
    agent: &str,
) -> Result<(String, AgentSignIn), DaemonError> {
    let mut known = Vec::new();
    for id in registry.subscription_ids() {
        let Some(decl) = registry.agent_sign_in(id) else {
            continue;
        };
        for name in decl.names() {
            known.push((*name).to_owned());
            if name.eq_ignore_ascii_case(agent) {
                return Ok((id.to_owned(), decl));
            }
        }
        if id.eq_ignore_ascii_case(agent) {
            return Ok((id.to_owned(), decl));
        }
    }
    let listed = if known.is_empty() {
        "a subscription that declares agent sign-in".to_owned()
    } else {
        known.join(", ")
    };
    Err(DaemonError::new(format!(
        "`{agent}` is not an agent whose sign-in hofund switches; use {listed}"
    )))
}

/// Switches an agent sign-in to `reference`, which may be `next` or an account
/// name, email, id, or alias.
///
/// # Errors
///
/// Returns [`DaemonError`] when the agent, account, or write fails.
pub async fn accounts_switch(
    config: &Config,
    registry: &dyn DaemonRegistry,
    state_root: &Path,
    opened: &[Opened],
    agent: &str,
    reference: &str,
) -> Result<String, DaemonError> {
    let (subscription, decl) = resolve_agent(registry, agent)?;
    let entry = find(opened, &subscription)?;
    let sub_cfg = config
        .subscriptions
        .iter()
        .find(|sub| sub.id == subscription)
        .ok_or_else(|| {
            DaemonError::new(format!("subscription `{subscription}` is not configured"))
        })?;
    let policy = policy_from_config(&sub_cfg.routing, &sub_cfg.options, decl.sign_in_spends());
    let home = agent_home(&decl)?;
    let known = sorted_accounts(entry).await?;
    let mut agents = read(state_root)?;
    let mut agent_state = agents.get(&subscription).cloned().unwrap_or_default();
    let rows = login_rows(&known, &agent_state);
    let aliases: Vec<(String, String)> = agent_state
        .aliases
        .iter()
        .map(|(key, name)| (key.clone(), name.clone()))
        .collect();
    let target = if reference.eq_ignore_ascii_case("next") {
        let quotas = quota_peeks(entry, &known).await;
        let (to, over) = next_in_order(&rows, &quotas, &policy, true).map_err(DaemonError::new)?;
        if over {
            eprintln!(
                "every account is past its line; switching to the one with the most room left"
            );
        }
        to
    } else {
        account_by_name(&rows, &aliases, reference).map_err(DaemonError::new)?
    };
    let account_id = known
        .iter()
        .find(|(id, label)| {
            id.as_str() == target
                || label.eq_ignore_ascii_case(&target)
                || label.to_lowercase() == target.to_lowercase()
        })
        .map(|(id, _)| id.clone())
        .ok_or_else(|| {
            DaemonError::new(format!(
                "subscription `{subscription}` has no account `{target}`"
            ))
        })?;
    apply_switch(
        entry,
        &subscription,
        &account_id,
        &home,
        &mut agent_state,
        &policy,
        true,
        &target,
    )
    .await?;
    agents.insert(subscription, agent_state);
    write(state_root, &agents)?;
    Ok(target)
}

/// Before removing an account, switch the agent off it when another exists.
pub async fn switch_before_forget(
    opened: &[Opened],
    registry: &dyn DaemonRegistry,
    config: &Config,
    state_root: &Path,
    subscription: &str,
    removing: &str,
) -> Result<(), DaemonError> {
    let Some(decl) = registry.agent_sign_in(subscription) else {
        return Ok(());
    };
    let entry = find(opened, subscription)?;
    let sub_cfg = config
        .subscriptions
        .iter()
        .find(|sub| sub.id == subscription)
        .ok_or_else(|| {
            DaemonError::new(format!("subscription `{subscription}` is not configured"))
        })?;
    let policy = policy_from_config(&sub_cfg.routing, &sub_cfg.options, decl.sign_in_spends());
    let mut agents = read(state_root)?;
    let mut agent_state = agents.get(subscription).cloned().unwrap_or_default();
    let known = sorted_accounts(entry).await?;
    let rows = login_rows(&known, &agent_state);
    let next = match next_on_forget(&rows, removing) {
        Some(next) => next,
        None => return Ok(()),
    };
    let account_id = known
        .iter()
        .find(|(id, label)| id.as_str() == next || label.eq_ignore_ascii_case(&next))
        .map(|(id, _)| id.clone())
        .ok_or_else(|| {
            DaemonError::new(format!(
                "subscription `{subscription}` has no account `{next}`"
            ))
        })?;
    let home = agent_home(&decl)?;
    apply_switch(
        entry,
        subscription,
        &account_id,
        &home,
        &mut agent_state,
        &policy,
        false,
        &next,
    )
    .await?;
    agents.insert(subscription.to_owned(), agent_state);
    write(state_root, &agents)
}

#[allow(clippy::too_many_arguments)]
async fn apply_switch(
    entry: &Opened,
    _subscription: &str,
    account: &AccountId,
    home: &Path,
    agent_state: &mut AgentState,
    policy: &LoginPolicy,
    user_choice: bool,
    active_label: &str,
) -> Result<(), DaemonError> {
    if user_choice {
        agent_state.return_to.clear();
    }
    let previous = agent_state.active.clone();
    entry
        .instance
        .write_agent_sign_in(account, home, &entry.host)
        .await
        .map_err(show)?;
    agent_state.active = active_label.to_lowercase();
    if user_choice {
        return Ok(());
    }
    if !previous.is_empty() && previous != agent_state.active && policy.returns_to_first() {
        agent_state.return_to = previous;
    }
    Ok(())
}

fn login_rows(known: &[(AccountId, String)], agent_state: &AgentState) -> Vec<LoginRow> {
    known
        .iter()
        .map(|(id, label)| LoginRow {
            key: label.to_lowercase(),
            on: true,
            active: agent_state.active.eq_ignore_ascii_case(id.as_str())
                || agent_state.active.eq_ignore_ascii_case(label),
            switchable: true,
        })
        .collect()
}

async fn quota_peeks(entry: &Opened, known: &[(AccountId, String)]) -> Vec<(String, LoginQuota)> {
    let mut out = Vec::new();
    for (id, label) in known {
        let windows = entry.instance.quota(id, &entry.host).await;
        let peek = match windows {
            Ok(windows) => {
                let mut used = 0.0_f64;
                for window in &windows {
                    if window.scope_model().is_some() {
                        continue;
                    }
                    if window.blocking() {
                        let share = match window.limit() {
                            Some(limit) if limit > 0 => {
                                (window.used() as f64) * 100.0 / (limit as f64)
                            }
                            _ => window.used() as f64,
                        };
                        used = used.max(share);
                    }
                }
                LoginQuota {
                    readable: true,
                    used_share: used,
                    used_up: used >= 100.0,
                    held: false,
                    error: false,
                }
            }
            Err(_) => LoginQuota {
                readable: false,
                error: true,
                ..LoginQuota::default()
            },
        };
        out.push((label.to_lowercase(), peek));
    }
    out
}

/// Moves each declared agent sign-in when its current account is spent.
pub async fn tick_sign_ins(
    registry: &dyn DaemonRegistry,
    states: &Subscriptions,
) -> Result<(), DaemonError> {
    for (id, state) in states {
        let Some(decl) = registry.agent_sign_in(id) else {
            continue;
        };
        let Ok(state_root) = state.host.dirs().state_dir() else {
            continue;
        };
        let policy = policy_from_config(
            &state.config.routing,
            &state.config.options,
            decl.sign_in_spends(),
        );
        let Ok(home) = agent_home(&decl) else {
            continue;
        };
        let _ = tick_one(
            id,
            &policy,
            state.instance.as_ref(),
            &state.host,
            &state_root,
            &home,
        )
        .await;
    }
    Ok(())
}

/// One subscription's automatic switch. Returns the account label moved to.
pub async fn tick_one(
    subscription: &str,
    policy: &LoginPolicy,
    instance: &dyn hofund_plugin_api::Subscription,
    host: &hofund_plugin_api::HostContext,
    state_root: &Path,
    home: &Path,
) -> Result<Option<String>, DaemonError> {
    let listed = host.accounts().list().await.map_err(show)?;
    let known: Vec<(AccountId, String)> = listed
        .into_iter()
        .map(|account| (account.id().clone(), account.label().to_owned()))
        .collect();
    let mut agents = read(state_root)?;
    let mut agent_state = agents.get(subscription).cloned().unwrap_or_default();
    let rows = login_rows(&known, &agent_state);
    let quotas = quota_peeks_instance(instance, host, &known).await;
    let return_key = if agent_state.return_to.is_empty() {
        None
    } else {
        Some(agent_state.return_to.as_str())
    };
    let Some((from, to, back)) = next_login(&rows, &quotas, policy, return_key) else {
        return Ok(None);
    };
    let account_id = known
        .iter()
        .find(|(id, label)| label.eq_ignore_ascii_case(&to) || id.as_str() == to)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| {
            DaemonError::new(format!(
                "subscription `{subscription}` has no account `{to}`"
            ))
        })?;
    instance
        .write_agent_sign_in(&account_id, home, host)
        .await
        .map_err(show)?;
    agent_state.active = to.clone();
    if back {
        agent_state.return_to.clear();
    } else if policy.returns_to_first() {
        agent_state.return_to = from;
    }
    agents.insert(subscription.to_owned(), agent_state);
    write(state_root, &agents)?;
    Ok(Some(to))
}

async fn quota_peeks_instance(
    instance: &dyn hofund_plugin_api::Subscription,
    host: &hofund_plugin_api::HostContext,
    known: &[(AccountId, String)],
) -> Vec<(String, LoginQuota)> {
    let mut out = Vec::new();
    for (id, label) in known {
        let windows = instance.quota(id, host).await;
        let peek = match windows {
            Ok(windows) => {
                let mut used = 0.0_f64;
                for window in &windows {
                    if window.scope_model().is_some() || !window.blocking() {
                        continue;
                    }
                    let share = match window.limit() {
                        Some(limit) if limit > 0 => (window.used() as f64) * 100.0 / (limit as f64),
                        _ => window.used() as f64,
                    };
                    used = used.max(share);
                }
                LoginQuota {
                    readable: true,
                    used_share: used,
                    used_up: used >= 100.0,
                    held: false,
                    error: false,
                }
            }
            Err(_) => LoginQuota {
                readable: false,
                error: true,
                ..LoginQuota::default()
            },
        };
        out.push((label.to_lowercase(), peek));
    }
    out
}

/// Imports accounts from `text` through the first subscription that reads it.
pub async fn accounts_import(opened: &[Opened], text: &str) -> Result<Vec<String>, DaemonError> {
    use hofund_plugin_api::PluginErrorKind;
    for entry in opened {
        match entry.instance.import_accounts(text, &entry.host).await {
            Ok(ids) => {
                return Ok(ids.into_iter().map(|id| id.to_string()).collect());
            }
            Err(error) if error.kind() == PluginErrorKind::Unsupported => {}
            Err(error) => return Err(show(error)),
        }
    }
    Err(DaemonError::new(
        "no configured subscription read that account export".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hofund_plugin_api::{
        AccountId, AccountStatus, AgentSignIn, HostContext, ModelList, PluginError, PluginId,
        PluginKind, QuotaWindow, SignInChallenge, SignInCtx, SignInFlow, SignInProof,
        UpstreamProvider,
    };
    use hofund_plugin_host::TestHost;
    use hofund_plugin_sdk::async_trait;

    use super::*;
    use crate::DaemonRegistry;

    struct Wrote(Mutex<Vec<String>>);

    struct TickSub {
        id: PluginId,
        flow: SignInFlow,
        wrote: Arc<Wrote>,
    }

    #[async_trait]
    impl hofund_plugin_api::Subscription for TickSub {
        fn id(&self) -> &PluginId {
            &self.id
        }

        fn flow(&self) -> &SignInFlow {
            &self.flow
        }

        async fn start_sign_in(
            &self,
            _ctx: &SignInCtx,
            _host: &HostContext,
        ) -> Result<SignInChallenge, PluginError> {
            Err(PluginError::unsupported("unused"))
        }

        async fn complete_sign_in(
            &self,
            _ctx: &SignInCtx,
            _proof: SignInProof,
            _host: &HostContext,
        ) -> Result<AccountId, PluginError> {
            Err(PluginError::unsupported("unused"))
        }

        async fn account_status(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<AccountStatus, PluginError> {
            Ok(AccountStatus::Ready)
        }

        async fn refresh(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<(), PluginError> {
            Ok(())
        }

        async fn quota(
            &self,
            account: &AccountId,
            _host: &HostContext,
        ) -> Result<Vec<QuotaWindow>, PluginError> {
            let used = if account.as_str() == "spent" { 99 } else { 10 };
            Ok(vec![
                QuotaWindow::new("window", used, Some(100), None).expect("window"),
            ])
        }

        async fn list_models(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<ModelList, PluginError> {
            Err(PluginError::unsupported("unused"))
        }

        fn upstream(
            &self,
            _account: &AccountId,
            _host: &HostContext,
        ) -> Result<Arc<dyn UpstreamProvider>, PluginError> {
            Err(PluginError::unsupported("unused"))
        }

        async fn write_agent_sign_in(
            &self,
            account: &AccountId,
            _home: &std::path::Path,
            _host: &HostContext,
        ) -> Result<(), PluginError> {
            self.wrote
                .0
                .lock()
                .expect("wrote")
                .push(account.to_string());
            Ok(())
        }
    }

    struct Names {
        id: &'static str,
        sign_in: AgentSignIn,
    }

    impl DaemonRegistry for Names {
        fn factory(&self, _id: &str) -> Option<Arc<dyn hofund_plugin_api::DynUpstreamFactory>> {
            None
        }

        fn is_kernel_factory(&self, _id: &str) -> bool {
            false
        }

        fn builtin_factory_ids(&self) -> &'static [&'static str] {
            &[]
        }

        fn builtin_protocols(&self) -> Vec<hofund_plugin_api::ProtocolId> {
            Vec::new()
        }

        fn find_preset(
            &self,
            _id: &str,
        ) -> Option<(&'static crate::CompiledPreset, Option<&'static str>)> {
            None
        }

        fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
            &[]
        }

        fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
            Vec::new()
        }

        fn subscription_ids(&self) -> Vec<&'static str> {
            vec![self.id]
        }

        fn agent_sign_in(&self, id: &str) -> Option<AgentSignIn> {
            (id == self.id).then_some(self.sign_in)
        }
    }

    #[test]
    fn d27_9_agent_names_come_from_the_registry() {
        let registry = Names {
            id: "alpha",
            sign_in: AgentSignIn::new(&["alpha", "aa"], "ALPHA_HOME", "alpha-home"),
        };
        let (id, decl) = resolve_agent(&registry, "aa").expect("alias");
        assert_eq!(id, "alpha");
        assert!(!decl.sign_in_spends());
        let error = resolve_agent(&registry, "other").expect_err("unknown");
        assert!(error.to_string().contains("alpha"));
    }

    #[tokio::test]
    async fn d27_12_tick_moves_a_spent_sign_in() {
        let dir = tempfile::tempdir().expect("dir");
        let plugin = PluginId::from_static("tick-sub");
        let host = TestHost::open_for(PluginKind::Subscription, plugin.clone(), dir.path());
        for (id, label) in [("spent", "Spent"), ("spare", "Spare")] {
            let account = host
                .context()
                .accounts()
                .account(AccountId::from_static(id), label)
                .expect("account");
            host.context()
                .accounts()
                .upsert(
                    account,
                    hofund_plugin_api::SecretString::from("secret".to_owned()),
                )
                .await
                .expect("store");
        }
        let state_root = host.context().dirs().state_dir().expect("state");
        let mut agents = std::collections::BTreeMap::new();
        agents.insert(
            "tick-sub".to_owned(),
            AgentState {
                active: "spent".to_owned(),
                ..AgentState::default()
            },
        );
        write(&state_root, &agents).expect("state");
        let wrote = Arc::new(Wrote(Mutex::new(Vec::new())));
        let sub = TickSub {
            id: plugin,
            flow: SignInFlow::Pkce,
            wrote: Arc::clone(&wrote),
        };
        let policy = LoginPolicy {
            routing: "smart".to_owned(),
            keep_login: false,
            switch_only: false,
            sign_in_spends: true,
        };
        let home = dir.path().join("agent-home");
        let moved = tick_one(
            "tick-sub",
            &policy,
            &sub,
            host.context(),
            &state_root,
            &home,
        )
        .await
        .expect("tick");
        assert_eq!(moved.as_deref(), Some("spare"));
        assert_eq!(wrote.0.lock().expect("wrote").as_slice(), ["spare"]);
    }
}
