//! Compile-time plugin registry.
//!
//! `build.rs` reads `plugins.toml` and each plugin's `plugin.toml`. Plugins
//! register through [`hofund_plugin_sdk::Contribution`]. This crate is the only
//! one that depends on plugin implementation crates.
//!
//! [`factory`] resolves a plugin id, including the built-in Chat, Responses,
//! Messages, and Generative factories. [`preset`] resolves a preset id to the
//! plugin and its static config.

#![deny(missing_docs)]

use std::sync::Arc;

use hofund_plugin_api::{DynUpstreamFactory, InboundSurface};

mod builtin;
#[allow(dead_code)]
mod check;

include!("reserved_agent_ids.rs");

mod generated {
    include!(concat!(env!("OUT_DIR"), "/registry.rs"));
}

mod agent_descriptions {
    include!(concat!(env!("OUT_DIR"), "/agents.rs"));
}

#[cfg(feature = "test-fixtures")]
mod fixture_agent_descriptions {
    include!(concat!(env!("OUT_DIR"), "/fixture_agents.rs"));
}

#[cfg(test)]
mod agent_toml;

use check::path_matches;

pub use agent_descriptions::{agent, agents};
#[cfg(feature = "test-fixtures")]
pub use fixture_agent_descriptions::fixture_agents;
pub use generated::{
    CompiledPreset, CompiledRoute, compiled_presets, compiled_routes, contributions,
};

/// The factory for `id`.
///
/// A linked plugin wins. Otherwise a built-in factory is returned. Pull
/// request E adds the plugin and this lookup keeps the same id.
#[must_use]
pub fn factory(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    upstream(id).or_else(|| builtin::factory(id))
}

/// The preset record for `id`, when a linked plugin declares it.
#[must_use]
pub fn preset(id: &str) -> Option<&'static CompiledPreset> {
    compiled_presets().iter().find(|preset| preset.id == id)
}

/// Every inbound surface the linked plugins contribute.
#[must_use]
pub fn surfaces() -> Vec<Arc<dyn InboundSurface>> {
    let mut found = Vec::new();
    for (_, contribution) in contributions() {
        found.extend(contribution.surfaces().iter().cloned());
    }
    found
}

/// The upstream factory for `id`, when that plugin is linked and contributes one.
#[must_use]
pub fn upstream(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    contributions()
        .into_iter()
        .find_map(|(name, contribution)| {
            if name == id {
                contribution.upstreams().first().cloned()
            } else {
                None
            }
        })
}

/// The inbound surface for `id`, when that plugin is linked and contributes one.
#[must_use]
pub fn surface(id: &str) -> Option<Arc<dyn InboundSurface>> {
    contributions()
        .into_iter()
        .find_map(|(name, contribution)| {
            if name == id {
                contribution.surfaces().first().cloned()
            } else {
                None
            }
        })
}

/// The plugin id of the longest matching route.
///
/// The method matches case-insensitively. The path equals the prefix, or
/// continues after a `/`. `/` matches every path. A miss returns `None`.
#[must_use]
pub fn match_route(method: &str, path: &str) -> Option<&'static str> {
    let mut best: Option<&CompiledRoute> = None;
    for route in compiled_routes() {
        if !route.method.eq_ignore_ascii_case(method) || !path_matches(route.prefix, path) {
            continue;
        }
        let longer = best.is_none_or(|current| route.prefix.len() > current.prefix.len());
        if longer {
            best = Some(route);
        }
    }
    best.map(|route| route.plugin)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{CredentialStyle, ListKind, ProtocolId};

    use super::{match_route, surface, upstream};

    #[test]
    fn echo_matches_get_only() {
        let surface = surface("echo").expect("echo");
        assert_eq!(surface.id().as_str(), "echo");
        assert_eq!(surface.routes()[0].prefix(), "/hofund/echo");
        assert_eq!(match_route("GET", "/hofund/echo"), Some("echo"));
        assert_eq!(match_route("GET", "/hofund/echo/nested"), Some("echo"));
        assert_eq!(match_route("GET", "/hofund/echov2"), None);
        assert_eq!(match_route("POST", "/hofund/echo"), None);
        assert!(upstream("echo").is_none());
    }

    #[test]
    fn relay_spec_is_the_plugin_id() {
        let factory = upstream("relay").expect("relay");
        assert_eq!(factory.spec().plugin().as_str(), "relay");
        assert_eq!(factory.spec().protocols(), &[ProtocolId::OPENAI_CHAT]);
        assert_eq!(factory.spec().credential(), CredentialStyle::Bearer);
        assert_eq!(factory.spec().list_kind(), ListKind::Unsupported);
        assert!(surface("relay").is_none());
        assert!(upstream("missing").is_none());
        assert!(match_route("GET", "/missing").is_none());
        let record = super::preset("relay").expect("relay preset");
        assert_eq!(record.plugin, "relay");
        assert!(record.config.is_empty());
        assert!(super::preset("openai").is_none());
        assert_eq!(
            super::factory("openai-compat")
                .expect("builtin")
                .spec()
                .plugin()
                .as_str(),
            "openai-compat"
        );
        assert!(super::factory("anthropic").is_some());
        assert!(super::factory("generative").is_some());
        assert!(super::factory("missing").is_none());
        assert!(super::factory("relay").is_some());
    }

    #[test]
    fn bedrock_spec_declares_sigv4() {
        let factory = super::factory("bedrock").expect("bedrock");
        assert_eq!(factory.spec().credential(), CredentialStyle::SigV4);
    }
}
