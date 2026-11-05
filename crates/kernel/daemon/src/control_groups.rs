//! Routing group control handlers.

use std::sync::Arc;

use hofund_agent::{GatewayTarget, catalog};
use hofund_config::{
    Stays, edit_group_create, edit_group_delete, edit_group_patch, edit_group_restore, load,
    validate_group_rules,
};
use hofund_control_proto::{
    ControlError, ControlErrorKind, FieldIssue, Group, GroupCreateRequest, GroupDeleteRequest,
    GroupDeleteResponse, GroupListResponse, GroupMember, GroupMemberCandidate,
    GroupMemberCandidatesResponse, GroupPatchRequest, GroupRestoreRequest, GroupRule,
    GroupWriteResponse,
};
use hofund_gateway::control::ControlFailure;
use hofund_plugin_api::AgentDescription;

use crate::config_ops::ConfigOps;
use crate::models::ModelsHost;
use crate::route_stays;

pub(crate) async fn list_groups(ops: &Arc<ConfigOps>) -> Result<GroupListResponse, ControlFailure> {
    let current = ops.get().await?;
    let config = load(&current.text).map_err(config_failure)?;
    let mut response = GroupListResponse::default();
    response.disk_revision = current.disk_revision;
    response.groups = config
        .routes
        .iter()
        .filter_map(|route| group_from_route(route, &config))
        .collect();
    Ok(response)
}

pub(crate) async fn create_group(
    ops: &Arc<ConfigOps>,
    request: GroupCreateRequest,
) -> Result<GroupWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    let current = ops.get().await?;
    let (edited, id) = edit_group_create(&current.text, &request.name, &request.first_member)
        .map_err(config_failure)?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    let config = load(&applied.text).map_err(config_failure)?;
    let route = config
        .routes
        .iter()
        .find(|route| route.model == format!("group/{id}"))
        .ok_or_else(|| internal("group create did not persist".to_owned()))?;
    Ok(group_write_response(route, &config, &applied))
}

pub(crate) async fn patch_group(
    ops: &Arc<ConfigOps>,
    descriptions: &'static [&'static AgentDescription],
    agent_session: Option<hofund_agent::Session>,
    _gateway_target: Option<GatewayTarget>,
    group_id: &str,
    request: GroupPatchRequest,
) -> Result<GroupWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    if let Some(rules) = &request.rules {
        validate_group_rules(rules).map_err(rules_failure)?;
    }
    let current = ops.get().await?;
    let stays = request
        .stays
        .as_deref()
        .map(|name| parse_stays(name).ok_or_else(|| stays_failure(name)))
        .transpose()?;
    let family = request.family.as_ref().map(|value| value.as_deref());
    let context = request.context;
    let classifier = request.classifier.as_ref().map(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value.as_str())
        }
    });
    let members = request.members.as_ref().map(|entries| {
        entries
            .iter()
            .map(|member| {
                (
                    member.reference.clone(),
                    member.effort.clone(),
                    member.fast,
                    member.enabled,
                )
            })
            .collect::<Vec<_>>()
    });
    let rename_from = group_id.to_owned();
    let (edited, effective_id) = edit_group_patch(
        &current.text,
        group_id,
        request.routing.as_deref(),
        stays,
        family,
        context,
        request.levels.as_deref(),
        classifier,
        members.as_deref(),
        request.rules.as_deref(),
        request.rename_to.as_deref(),
    )
    .map_err(config_failure)?;
    let backup = current.text.clone();
    let applied = ops.apply(&request.expected_revision, edited).await?;
    if request.rename_to.is_some() && effective_id != rename_from {
        let session = agent_session.unwrap_or_else(crate::agent_session);
        if let Err(error) =
            rename_group_agents(descriptions, &session, &rename_from, &effective_id).await
        {
            let current = ops.get().await?;
            ops.apply(&current.disk_revision, backup).await?;
            return Err(error);
        }
    }
    let config = load(&applied.text).map_err(config_failure)?;
    let model = format!("group/{effective_id}");
    let route = config
        .routes
        .iter()
        .find(|route| route.model == model)
        .ok_or_else(|| not_found(&effective_id))?;
    Ok(group_write_response(route, &config, &applied))
}

pub(crate) async fn delete_group(
    ops: &Arc<ConfigOps>,
    group_id: &str,
    request: GroupDeleteRequest,
) -> Result<GroupDeleteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    let current = ops.get().await?;
    let edited = edit_group_delete(&current.text, group_id).map_err(config_failure)?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    let mut response = GroupDeleteResponse::default();
    response.disk_revision = applied.disk_revision;
    response.removed = group_id.to_owned();
    Ok(response)
}

pub(crate) async fn restore_group(
    ops: &Arc<ConfigOps>,
    group_id: &str,
    request: GroupRestoreRequest,
) -> Result<GroupWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    let current = ops.get().await?;
    let edited = edit_group_restore(&current.text, group_id).map_err(|error| match error {
        hofund_config::ConfigError::Invalid(message)
            if message.contains("unknown group") || message.contains("not hidden") =>
        {
            not_found(group_id)
        }
        other => config_failure(other),
    })?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    let config = load(&applied.text).map_err(config_failure)?;
    let model = format!("group/{group_id}");
    let route = config
        .routes
        .iter()
        .find(|route| route.model == model)
        .ok_or_else(|| not_found(group_id))?;
    Ok(group_write_response(route, &config, &applied))
}

pub(crate) fn member_candidates(
    config: &hofund_config::Config,
    models: &ModelsHost,
) -> GroupMemberCandidatesResponse {
    let mut response = GroupMemberCandidatesResponse::default();
    for provider in &config.providers {
        if !provider.enabled {
            continue;
        }
        let cached = models.cached(&provider.id);
        for model in &cached.models {
            let reference = format!("{}/{}", provider.id, model.id());
            if model.decides() {
                response.deciders.push(candidate(reference, true));
            } else {
                response.served.push(candidate(reference, false));
            }
        }
    }
    for route in &config.routes {
        if route.hidden || !route.model.starts_with("group/") {
            continue;
        }
        response.served.push(candidate(route.model.clone(), false));
    }
    response
        .served
        .sort_by(|left, right| left.reference.cmp(&right.reference));
    response
        .deciders
        .sort_by(|left, right| left.reference.cmp(&right.reference));
    response
}

async fn rename_group_agents(
    descriptions: &'static [&'static AgentDescription],
    session: &hofund_agent::Session,
    from: &str,
    to: &str,
) -> Result<(), ControlFailure> {
    let from = from.to_owned();
    let to = to.to_owned();
    let session = session.clone();
    tokio::task::spawn_blocking(move || {
        let catalog = catalog(descriptions).map_err(|error| internal(error.to_string()))?;
        catalog
            .rename_group_refs(&session, &from, &to)
            .map_err(|error| internal(error.to_string()))?;
        Ok::<(), ControlFailure>(())
    })
    .await
    .map_err(|_| internal("group rename agent task failed".to_owned()))??;
    Ok(())
}

fn candidate(reference: String, decision: bool) -> GroupMemberCandidate {
    let mut candidate = GroupMemberCandidate::default();
    candidate.reference = reference;
    candidate.decision = decision;
    candidate
}

fn group_from_route(route: &hofund_config::Route, config: &hofund_config::Config) -> Option<Group> {
    let id = route.model.strip_prefix("group/")?;
    let mut group = Group::default();
    group.id = id.to_owned();
    group.name = route.name.clone().unwrap_or_else(|| id.to_owned());
    group.routing = route.routing.clone();
    group.stays = route_stays(config, route).as_str().to_owned();
    group.family = route.family.clone();
    group.context = route.context;
    group.levels = route.levels.clone();
    group.classifier = route.classifier.clone().unwrap_or_default();
    group.members = route.upstreams.iter().map(member_from_upstream).collect();
    group.rules = route
        .rules
        .iter()
        .map(|line| {
            let mut rule = GroupRule::default();
            rule.line = line.clone();
            rule
        })
        .collect();
    group.hidden = route.hidden;
    group.found = route.hidden;
    Some(group)
}

fn member_from_upstream(upstream: &hofund_config::Upstream) -> GroupMember {
    let reference = match &upstream.target {
        hofund_config::UpstreamTarget::Provider(id) => {
            if let Some(model) = &upstream.model {
                format!("{id}/{model}")
            } else {
                id.clone()
            }
        }
        hofund_config::UpstreamTarget::Subscription(id) => {
            if let Some(model) = &upstream.model {
                format!("{id}/{model}")
            } else {
                id.clone()
            }
        }
        hofund_config::UpstreamTarget::Group(id) => format!("group/{id}"),
    };
    let mut member = GroupMember::default();
    member.reference = reference;
    member.effort = upstream.effort.clone().unwrap_or_default();
    member.fast = upstream.fast;
    member.enabled = upstream.enabled;
    member
}

fn group_write_response(
    route: &hofund_config::Route,
    config: &hofund_config::Config,
    applied: &hofund_control_proto::ConfigResponse,
) -> GroupWriteResponse {
    let mut response = GroupWriteResponse::default();
    response.group = group_from_route(route, config).expect("group route");
    response.disk_revision = applied.disk_revision.clone();
    response.runtime_revision = applied.runtime_revision.clone();
    response.pending_restart = applied.pending_restart.clone();
    response
}

fn parse_stays(name: &str) -> Option<Stays> {
    match name {
        "auto" => Some(Stays::Auto),
        "session" => Some(Stays::Session),
        "turn" => Some(Stays::Turn),
        "off" => Some(Stays::Off),
        "strict" => Some(Stays::Strict),
        _ => None,
    }
}

fn not_found(id: &str) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::NotFound,
        format!("unknown group `{id}`"),
        "control",
    ))
}

fn rules_failure(error: hofund_config::ConfigError) -> ControlFailure {
    let message = error.to_string();
    let mut control = ControlError::new(ControlErrorKind::Unprocessable, &message, "control");
    control
        .details
        .push(FieldIssue::new("rules", "invalid", &message));
    ControlFailure::from(control)
}

fn stays_failure(name: &str) -> ControlFailure {
    let message = format!("unknown stays `{name}`");
    let mut control = ControlError::new(ControlErrorKind::Unprocessable, &message, "control");
    control
        .details
        .push(FieldIssue::new("stays", "invalid", &message));
    ControlFailure::from(control)
}

fn config_failure(error: hofund_config::ConfigError) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Unprocessable,
        error.to_string(),
        "control",
    ))
}

fn internal(message: String) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Internal,
        message,
        "control",
    ))
}
