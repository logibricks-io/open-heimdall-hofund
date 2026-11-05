//! Moves agents off route models a disabled or removed provider no longer serves.

use crate::{AgentError, Catalog, GatewayTarget, Session, SidecarCheckpoint};

/// Rewrites agent model settings that pointed at route models served through
/// `provider_id`.
#[allow(clippy::too_many_arguments)]
pub fn reseat_after_provider_change(
    catalog: &Catalog,
    session: &Session,
    gateway_target: &GatewayTarget,
    provider_id: &str,
    provider_still_present: bool,
    match_route_models: &[String],
    _sendable_fallback_routes: &[String],
    agent_visible_sendable: impl Fn(&str) -> Vec<String>,
    checkpoint: &mut SidecarCheckpoint,
) -> Result<Vec<String>, AgentError> {
    let provider_id = provider_id.trim();
    let prefix = format!("{provider_id}/");
    let same_provider: Vec<String> = if provider_still_present {
        match_route_models.to_vec()
    } else {
        Vec::new()
    };
    let mut moved = Vec::new();
    for adapter in catalog.adapters() {
        if !adapter.detect(session) {
            continue;
        }
        let Some(current) = adapter.configured_model(session)? else {
            continue;
        };
        let route_model = configured_route_model(adapter.model_prefix(), &current);
        let on_provider = route_model.as_deref() == Some(provider_id)
            || route_model
                .as_deref()
                .is_some_and(|model| model.starts_with(&prefix))
            || route_model
                .as_deref()
                .is_some_and(|model| match_route_models.iter().any(|route| route == model));
        if !on_provider {
            continue;
        }
        let agent_models = agent_visible_sendable(adapter.id());
        let target = gateway_target.clone().with_models(agent_models.clone());
        adapter.record_sidecar_paths(session, checkpoint)?;
        if agent_models.is_empty() {
            adapter.set_model_tracked(session, &target, None, checkpoint)?;
            moved.push(adapter.id().to_owned());
            continue;
        }
        let current_route = route_model.as_deref().unwrap_or(current.as_str());
        let replacement: Option<&str> = same_provider
            .iter()
            .find(|model| model.as_str() != current_route)
            .map(String::as_str)
            .or_else(|| same_provider.first().map(String::as_str))
            .or_else(|| {
                agent_models
                    .iter()
                    .find(|model| model.as_str() != current_route)
                    .map(String::as_str)
            })
            .or_else(|| agent_models.first().map(String::as_str));
        adapter.set_model_tracked(session, &target, replacement, checkpoint)?;
        moved.push(adapter.id().to_owned());
    }
    Ok(moved)
}

fn configured_route_model(model_prefix: Option<&str>, configured: &str) -> Option<String> {
    if let Some(prefix) = model_prefix {
        configured
            .strip_prefix(prefix)
            .map(|model| model.to_owned())
    } else {
        Some(configured.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::GatewayTarget;

    use super::configured_route_model;
    use super::reseat_after_provider_change;
    use crate::{Session, SidecarCheckpoint, catalog_from};

    #[test]
    fn r7a_configured_route_model_strips_adapter_prefix() {
        assert_eq!(
            configured_route_model(Some("hofund/"), "hofund/on-added").as_deref(),
            Some("on-added")
        );
        assert_eq!(
            configured_route_model(None, "on-added").as_deref(),
            Some("on-added")
        );
    }

    #[test]
    fn r7a_reseat_picks_visible_sendable_route_when_provider_fallback_list_is_empty() {
        use hofund_plugin_api::{
            AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId, Slot, Visible,
        };

        static FILES: &[AgentFile] = &[AgentFile::from_static(
            "settings",
            FileKind::Json,
            "~/.claude/settings.json",
            "~/.claude/settings.json",
            "~/.claude/settings.json",
            None,
        )];
        static SETS: &[AgentSet] = &[AgentSet::from_static(
            "settings",
            "model",
            Slot::model_if_unset(None),
        )];
        static CLAUDE: AgentDescription = AgentDescription::from_static(
            PluginId::from_static("claude-code"),
            "Claude Code",
            "claude",
            FILES,
            SETS,
            Some("model"),
            None,
            Visible::All,
            AgentRoute::Gateway,
            &[],
        )
        .with_model_file("settings");
        static COMPILED: &[&AgentDescription] = &[&CLAUDE];

        let dir = tempfile::tempdir().expect("dir");
        let settings = dir
            .path()
            .join("home")
            .join(".claude")
            .join("settings.json");
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
        std::fs::write(&settings, r#"{"model":"on-dead"}"#).expect("settings");
        let session = Session::new(dir.path().join("home"));
        let catalog = catalog_from(COMPILED).expect("catalog");
        let target = GatewayTarget::new(
            "http://127.0.0.1:3425/",
            "token",
            vec!["group/pair".to_owned()],
        );
        let mut checkpoint = SidecarCheckpoint::new();
        reseat_after_provider_change(
            &catalog,
            &session,
            &target,
            "dead",
            false,
            &["on-dead".to_owned()],
            &[],
            |_| vec!["group/pair".to_owned()],
            &mut checkpoint,
        )
        .expect("reseat onto visible group route");
        let text = std::fs::read_to_string(&settings).expect("settings");
        assert!(
            text.contains("group/pair"),
            "must not clear model when a visible sendable route exists, got: {text}"
        );
    }
}
