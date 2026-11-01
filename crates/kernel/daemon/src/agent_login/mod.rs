//! Agent sign-in switching for subscription-backed coding agents.
//!
//! Vendor file writes stay in the subscription plugins; this module keeps
//! policy, state, and orchestration.

mod logic;
mod state;

mod cli;

pub use cli::accounts_switch_in;
pub use logic::{
    LoginPolicy, LoginQuota, LoginRow, account_by_name, next_in_order, next_on_forget,
};
pub use state::{AgentState, agent_home, policy_from_config, read, subscription_for_agent, write};

use std::path::Path;

use hofund_config::Config;
use hofund_plugin_api::AccountId;

use crate::sign_in::{Opened, find, sorted_accounts};
use crate::{DaemonError, show};

/// Switches an agent sign-in to `reference`, which may be `next` or an account
/// name, email, id, or alias.
///
/// # Errors
///
/// Returns [`DaemonError`] when the agent, account, or write fails.
pub async fn accounts_switch(
    config: &Config,
    state_root: &Path,
    opened: &[Opened],
    agent: &str,
    reference: &str,
) -> Result<String, DaemonError> {
    let subscription = subscription_for_agent(agent).ok_or_else(|| {
        DaemonError::new(format!(
            "`{agent}` is not an agent whose sign-in hofund switches; use claude or codex"
        ))
    })?;
    let entry = find(opened, subscription)?;
    let sub_cfg = config
        .subscriptions
        .iter()
        .find(|sub| sub.id == subscription)
        .ok_or_else(|| {
            DaemonError::new(format!("subscription `{subscription}` is not configured"))
        })?;
    let mut policy = policy_from_config(&sub_cfg.routing, &sub_cfg.options);
    if subscription == "claude" {
        policy.sign_in_spends = true;
    }
    let home = agent_home(subscription)?;
    let known = sorted_accounts(entry).await?;
    let mut agents = read(state_root)?;
    let mut agent_state = agents.get(subscription).cloned().unwrap_or_default();
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
        subscription,
        &account_id,
        &home,
        &mut agent_state,
        &policy,
        true,
        &target,
    )
    .await?;
    agents.insert(subscription.to_owned(), agent_state);
    write(state_root, &agents)?;
    Ok(target)
}

/// Before removing an account, switch the agent off it when another exists.
pub async fn switch_before_forget(
    opened: &[Opened],
    config: &Config,
    state_root: &Path,
    subscription: &str,
    removing: &str,
) -> Result<(), DaemonError> {
    let entry = find(opened, subscription)?;
    let sub_cfg = config
        .subscriptions
        .iter()
        .find(|sub| sub.id == subscription)
        .ok_or_else(|| {
            DaemonError::new(format!("subscription `{subscription}` is not configured"))
        })?;
    let mut policy = policy_from_config(&sub_cfg.routing, &sub_cfg.options);
    if subscription == "claude" {
        policy.sign_in_spends = true;
    }
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
    let home = agent_home(subscription)?;
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
                        used = used.max(window.used() as f64);
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
