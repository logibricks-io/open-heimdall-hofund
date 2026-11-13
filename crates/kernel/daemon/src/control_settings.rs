use std::sync::Arc;

use hofund_config::SettingsEdit;
use hofund_control_proto::{
    ControlError, ControlErrorKind, SettingsPatchRequest, SettingsResponse,
};
use hofund_gateway::control::ControlFailure;

use crate::config_ops::{ConfigOps, LiveConfig, config_issue};

pub(crate) async fn get(ops: &ConfigOps) -> Result<SettingsResponse, ControlFailure> {
    let current = ops.get().await?;
    let config = hofund_config::load(&current.text).map_err(|error| {
        let issue = config_issue(&error, &current.text);
        let mut failure = ControlError::new(
            ControlErrorKind::InvalidConfig,
            issue.message.clone(),
            "settings",
        );
        failure.details = vec![issue];
        ControlFailure::from(failure)
    })?;
    Ok(response(&config, &ops.live(), current.disk_revision))
}

pub(crate) async fn patch(
    ops: &Arc<ConfigOps>,
    request: SettingsPatchRequest,
) -> Result<SettingsResponse, ControlFailure> {
    let edit = SettingsEdit {
        listen: request.listen,
        currency: request.currency,
        quota_left: request.quota_left,
        discover_accounts: request.discover_accounts,
        lan_allow: request.lan_allow,
    };
    let applied = ops.apply_settings(&request.expected_revision, edit).await?;
    let live = ops.live();
    Ok(response(&live.config, &live, applied.disk_revision))
}

fn response(
    config: &hofund_config::Config,
    live: &LiveConfig,
    disk_revision: String,
) -> SettingsResponse {
    let mut response = SettingsResponse::default();
    response.disk_revision = disk_revision;
    response.listen = config.listen.to_string();
    response.currency = config.usage.currency.as_str().to_owned();
    response.quota_left = config.usage.quota_left;
    response.discover_accounts = config.discover_agent_accounts;
    response.lan_allow.clone_from(&config.lan_allow);
    response.pending_restart = live.pending_restart.iter().any(|field| field == "listen");
    response
}
