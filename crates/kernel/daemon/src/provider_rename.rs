//! Atomic provider id rename across config, model state, gateway keys, and agents.
//!
//! Usage rows keep the upstream provider id recorded when each call was made; they
//! are not rewritten (same rule as historical rename maps in the reference).

use std::sync::Arc;

use hofund_config::{edit_provider_rename, validate_provider_rename};
use hofund_control_proto::{ConfigResponse, ProviderRenameResponse};
use hofund_gateway::control::ControlFailure;
use hofund_plugin_api::{AgentDescription, MemberId};
use hofund_secrets::KeyStore;

use std::sync::Mutex;

use crate::config_ops::ConfigOps;
use crate::membership;
use crate::models::ModelsHost;
use crate::provider_keys;

/// Injected failure point for integration tests (`g3_9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRenameFailAfter {
    /// Fail while rewriting gateway key scopes.
    Keys,
    /// Fail while rewriting agent configuration.
    Agents,
}

static FAIL_AFTER: Mutex<Option<ProviderRenameFailAfter>> = Mutex::new(None);

/// Sets a one-shot failure injection for [`rename_provider`].
#[doc(hidden)]
pub fn set_rename_fail_after(step: Option<ProviderRenameFailAfter>) {
    *FAIL_AFTER.lock().expect("rename fail hook") = step;
}

fn fail_after(step: ProviderRenameFailAfter) -> bool {
    *FAIL_AFTER.lock().expect("rename fail hook") == Some(step)
}

struct Backups {
    config: String,
    keys: Vec<hofund_secrets::Key>,
}

/// Renames a configured provider and every reference to it.
///
/// # Errors
///
/// Returns [`ControlFailure`] when validation fails or a step cannot complete;
/// earlier steps are rolled back when possible.
pub(crate) async fn rename_provider(
    ops: &Arc<ConfigOps>,
    models: &Arc<ModelsHost>,
    keys: &Arc<KeyStore>,
    descriptions: &'static [&'static AgentDescription],
    from: &str,
    to: &str,
) -> Result<ProviderRenameResponse, ControlFailure> {
    let from = from.trim().to_ascii_lowercase();
    let to = to.trim().to_ascii_lowercase();
    if from == to {
        let current = ops.get().await?;
        return Ok(response(&from, &to, &current, Vec::new()));
    }
    let current = ops.get().await?;
    validate_provider_rename(&current.text, &from, &to).map_err(|error| {
        ControlFailure::from(hofund_control_proto::ControlError::new(
            hofund_control_proto::ControlErrorKind::Unprocessable,
            error.to_string(),
            "control",
        ))
    })?;
    let edited = edit_provider_rename(&current.text, &from, &to).map_err(|error| {
        ControlFailure::from(hofund_control_proto::ControlError::new(
            hofund_control_proto::ControlErrorKind::Unprocessable,
            error.to_string(),
            "control",
        ))
    })?;
    let key_moves = member_moves(&current.text, &from, &to).map_err(|message| {
        ControlFailure::from(hofund_control_proto::ControlError::new(
            hofund_control_proto::ControlErrorKind::Unprocessable,
            message,
            "control",
        ))
    })?;
    let backups = Backups {
        config: current.text.clone(),
        keys: keys
            .export()
            .map_err(|error| internal_failure(error.to_string()))?,
    };
    let applied = ops
        .apply_with_key_moves(&current.disk_revision, edited, key_moves)
        .await?;
    if let Err(error) = models.rename_provider(&from, &to) {
        let _ = restore_config(ops, &backups).await;
        return Err(internal_failure(error.to_string()));
    }
    if fail_after(ProviderRenameFailAfter::Keys) {
        let _ = models.rename_provider(&to, &from);
        let _ = restore_config(ops, &backups).await;
        return Err(internal_failure(
            "injected provider-rename keys failure".to_owned(),
        ));
    }
    if let Err(error) = keys.replace_provider(&from, &to) {
        let _ = models.rename_provider(&to, &from);
        let _ = restore_config(ops, &backups).await;
        return Err(internal_failure(error.to_string()));
    }
    let moved = match tokio::task::spawn_blocking({
        let from = from.clone();
        let to = to.clone();
        move || -> Result<Vec<String>, ControlFailure> {
            if fail_after(ProviderRenameFailAfter::Agents) {
                return Err(internal_failure(
                    "injected provider-rename agents failure".to_owned(),
                ));
            }
            let catalog = hofund_agent::catalog(descriptions)
                .map_err(|error| internal_failure(error.to_string()))?;
            let session = crate::agent_session();
            catalog
                .rename_provider_refs(&session, &from, &to)
                .map_err(|error| internal_failure(error.to_string()))
        }
    })
    .await
    {
        Ok(Ok(agents)) => agents,
        Ok(Err(error)) => {
            let _ = keys.restore(&backups.keys);
            let _ = models.rename_provider(&to, &from);
            let _ = restore_config(ops, &backups).await;
            return Err(error);
        }
        Err(error) => {
            let _ = keys.restore(&backups.keys);
            let _ = models.rename_provider(&to, &from);
            let _ = restore_config(ops, &backups).await;
            return Err(internal_failure(error.to_string()));
        }
    };
    Ok(response(&from, &to, &applied, moved))
}

async fn restore_config(ops: &Arc<ConfigOps>, backups: &Backups) -> Result<(), ControlFailure> {
    let current = ops.get().await?;
    let _ = ops
        .apply_with_key_moves(&current.disk_revision, backups.config.clone(), Vec::new())
        .await?;
    Ok(())
}

fn member_moves(text: &str, from: &str, to: &str) -> Result<Vec<(MemberId, MemberId)>, String> {
    let config = hofund_config::load(text).map_err(|error| error.to_string())?;
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == from)
        .ok_or_else(|| format!("unknown provider `{from}`"))?;
    let resolved = provider_keys::resolve(provider).map_err(|error| error.to_string())?;
    let several = resolved.iter().filter(|key| key.entry.on).count() > 1;
    let mut moves = Vec::new();
    if several {
        for key in resolved.iter().filter(|key| key.entry.on) {
            moves.push((
                membership::key_member_id(from, &key.id),
                membership::key_member_id(to, &key.id),
            ));
        }
    } else {
        moves.push((membership::member_id(from), membership::member_id(to)));
    }
    Ok(moves)
}

fn response(
    from: &str,
    to: &str,
    applied: &ConfigResponse,
    agents: Vec<String>,
) -> ProviderRenameResponse {
    let mut out = ProviderRenameResponse::default();
    out.from = from.to_owned();
    out.to = to.to_owned();
    out.disk_revision.clone_from(&applied.disk_revision);
    out.agents = agents;
    out
}

fn internal_failure(message: String) -> ControlFailure {
    ControlFailure::from(hofund_control_proto::ControlError::new(
        hofund_control_proto::ControlErrorKind::Internal,
        message,
        "control",
    ))
}
