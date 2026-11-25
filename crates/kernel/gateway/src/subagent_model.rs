//! Move an eligible subagent turn onto a model a hook names.
//!
//! The hook lives outside this crate. This module only applies the choice:
//! a served, enabled, allowed target replaces the model used for routing and
//! token counting, and may set that target's effort. Anything else keeps the
//! model the client asked for and records why.

use std::sync::Arc;

use hofund_control_proto::SubagentRoute;
use hofund_ir::{Effort, Request as ModelRequest};
use hofund_secrets::Identity;
use serde_json::Value;

use crate::{Inner, route_scope};

/// What the hook may read. The body is the raw client payload.
pub struct SubagentModelQuery<'a> {
    /// Agent id resolved for this request.
    pub agent_id: &'a str,
    /// Raw request body.
    pub body: &'a [u8],
}

/// A model the hook wants this turn to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentModelChoice {
    /// Route id, without an effort suffix.
    pub model: String,
    /// Effort label from `<model>:<effort>`, when the setting named one.
    pub effort_label: Option<String>,
}

/// Reads a subagent model for one request. `None` leaves the request unchanged.
pub type SubagentModelHook =
    Arc<dyn Fn(SubagentModelQuery<'_>) -> Option<SubagentModelChoice> + Send + Sync>;

/// The request to route, when it differs, and the trace note, when one applies.
pub(crate) struct Prepared {
    /// Replacement used for routing and token counting.
    pub request: Option<ModelRequest>,
    /// Trace note. Absent when this turn is not a subagent-model candidate.
    pub note: Option<SubagentRoute>,
}

/// Applies the configured hook to `request`.
pub(crate) fn prepare(
    app: &Inner,
    agent_id: &str,
    subagent_id: &str,
    body: &[u8],
    identity: &Identity,
    request: &ModelRequest,
) -> Prepared {
    if subagent_id.is_empty() {
        return Prepared {
            request: None,
            note: None,
        };
    }
    let Some(hook) = &app.subagent_model else {
        return Prepared {
            request: None,
            note: None,
        };
    };
    let Some(choice) = hook(SubagentModelQuery { agent_id, body }) else {
        return Prepared {
            request: None,
            note: None,
        };
    };
    if choice.model.is_empty() || choice.model == request.model {
        return Prepared {
            request: None,
            note: None,
        };
    }
    let router = app.snapshot.load();
    let disabled = app.model_catalog.as_ref().is_some_and(|catalog| {
        catalog
            .visibility()
            .disabled_pattern(&choice.model)
            .is_some()
    });
    let asked = request.model.clone();
    if disabled {
        return keep(&asked, &choice.model, "it is disabled");
    }
    let Ok(resolved) = router.resolve(&choice.model) else {
        return keep(&asked, &choice.model, "the gateway does not serve it now");
    };
    let restricted = !identity.models.is_empty() || !identity.accounts.is_empty();
    if restricted && !router.admits(&choice.model, Some(&route_scope(identity))) {
        return keep(&asked, &choice.model, "the gateway key may not use it");
    }
    let mut routed = request.clone();
    retarget(&mut routed, &resolved.route, choice.effort_label.as_deref());
    Prepared {
        request: Some(routed),
        note: Some(SubagentRoute::new(asked, choice.model, "")),
    }
}

fn keep(asked: &str, to: &str, reason: &str) -> Prepared {
    Prepared {
        request: None,
        note: Some(SubagentRoute::new(asked, to, reason)),
    }
}

fn retarget(request: &mut ModelRequest, model: &str, effort_label: Option<&str>) {
    use crate::bridge_effort::{BRIDGE_EFFORT, THINK_OFF};

    request.model = model.to_owned();
    let effort = effort_label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| {
            if label.eq_ignore_ascii_case("off") {
                "none"
            } else {
                label
            }
        })
        .and_then(Effort::from_label);
    request.params.effort = effort;
    let bridge_label = match effort {
        Some(Effort::Minimal) => "low",
        Some(Effort::None) | None => "",
        Some(effort) => effort.openai_label(),
    };
    request
        .extensions
        .insert(BRIDGE_EFFORT, Value::String(bridge_label.to_owned()));
    request
        .extensions
        .insert(THINK_OFF, Value::Bool(effort == Some(Effort::None)));
    request.extensions.remove("thinking");
    if let Some(mut config) = request.extensions.get("output_config").cloned()
        && let Some(object) = config.as_object_mut()
    {
        object.remove("effort");
        request.extensions.insert("output_config", config);
    }
    if effort == Some(Effort::None) {
        request
            .extensions
            .insert("thinking", serde_json::json!({"type": "disabled"}));
        return;
    }
    if let Some(effort) = effort {
        let mut config = request
            .extensions
            .get("output_config")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if let Some(object) = config.as_object_mut() {
            object.insert(
                "effort".to_owned(),
                Value::String(effort.messages_label().to_owned()),
            );
            request.extensions.insert("output_config", config);
        }
    }
}
