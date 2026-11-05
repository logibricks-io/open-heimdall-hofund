//! Routing group control handlers.

use std::sync::Arc;

use hofund_config::{
    Stays, edit_group_create, edit_group_delete, edit_group_patch, edit_group_restore, load,
    validate_group_rules,
};
use hofund_control_proto::{
    ControlError, ControlErrorKind, Group, GroupCreateRequest, GroupDeleteRequest,
    GroupDeleteResponse, GroupListResponse, GroupMember, GroupMemberCandidate,
    GroupMemberCandidatesResponse, GroupPatchRequest, GroupRestoreRequest, GroupRule,
    GroupWriteResponse,
};
use hofund_gateway::control::ControlFailure;

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
        .expect("group route");
    Ok(group_write_response(route, &config, &applied))
}

pub(crate) async fn patch_group(
    ops: &Arc<ConfigOps>,
    group_id: &str,
    request: GroupPatchRequest,
) -> Result<GroupWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    if let Some(rules) = &request.rules {
        validate_group_rules(rules).map_err(config_failure)?;
    }
    let current = ops.get().await?;
    let stays = request.stays.as_deref().and_then(parse_stays);
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
    let edited = edit_group_patch(
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
    let edited = edit_group_restore(&current.text, group_id).map_err(|error| {
        if error.to_string().contains("not hidden") {
            not_found(group_id)
        } else {
            config_failure(error)
        }
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
            if model.draws() || model.films() {
                continue;
            }
            let reference = format!("{}/{}", provider.id, model.id());
            response.served.push(candidate(reference, model.decides()));
        }
    }
    for route in &config.routes {
        if route.hidden {
            continue;
        }
        response.served.push(candidate(route.model.clone(), false));
    }
    response
        .served
        .sort_by(|left, right| left.reference.cmp(&right.reference));
    response.deciders = response
        .served
        .iter()
        .filter(|entry| entry.decision)
        .cloned()
        .collect();
    response
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
    group.found = false;
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

fn config_failure(error: hofund_config::ConfigError) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Unprocessable,
        error.to_string(),
        "control",
    ))
}
