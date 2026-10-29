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

use hofund_plugin_api::{
    DynSubscriptionFactory, DynUpstreamFactory, InboundSurface, PluginKind, ProtocolId,
};

mod builtin;
#[allow(dead_code)]
mod check;
mod select;

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

#[cfg(feature = "test-fixtures")]
mod fixture_preset_descriptions {
    include!(concat!(env!("OUT_DIR"), "/fixture_presets.rs"));
}

#[cfg(feature = "test-fixtures")]
mod fixture_upstream;

#[cfg(feature = "test-fixtures")]
mod fixture_subscription;

#[cfg(test)]
mod agent_toml;
#[cfg(test)]
#[allow(dead_code)]
mod preset_toml;

use check::path_matches;

pub use agent_descriptions::{agent, agents};
#[cfg(feature = "test-fixtures")]
#[doc(hidden)]
pub use builtin::{anthropic, anthropic_adapter, openai_compat, openai_compat_adapter};
pub use check::builtin_factory_ids;
#[cfg(feature = "test-fixtures")]
pub use fixture_agent_descriptions::fixture_agents;
#[cfg(feature = "test-fixtures")]
pub use fixture_preset_descriptions::{fixture_aliases, fixture_presets};
pub use generated::{
    CompiledAlias, CompiledPreset, CompiledRegion, CompiledRoute, compiled_aliases,
    compiled_presets, compiled_routes,
};
pub use select::{
    SelectError, Selection, model_plan_of, protocol_for, resolve_region, select, wire_for_protocol,
};

/// Every linked plugin contribution, keyed by plugin kind and id, in `plugins.toml` order.
///
/// With the `test-fixtures` feature, the test-only two-protocol listed
/// fixture `fixture-multi-listed` is appended. The generated list never names
/// a fixture.
#[must_use]
pub fn contributions() -> Vec<(PluginKind, &'static str, hofund_plugin_sdk::Contribution)> {
    #[cfg_attr(not(feature = "test-fixtures"), allow(unused_mut))]
    let mut linked = generated::contributions();
    #[cfg(feature = "test-fixtures")]
    linked.push((
        PluginKind::Provider,
        "fixture-multi-listed",
        fixture_upstream::contribute(),
    ));
    #[cfg(feature = "test-fixtures")]
    linked.push((
        PluginKind::Subscription,
        "fixture-sub",
        fixture_subscription::contribute(),
    ));
    linked
}

/// The factory for `id`.
///
/// A linked plugin wins. Otherwise a built-in factory is returned. Pull
/// request E adds the plugin and this lookup keeps the same id.
#[must_use]
pub fn factory(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    upstream(id).or_else(|| builtin::factory(id))
}

/// Whether `id` names a kernel-owned factory rather than a linked plugin.
///
/// Unlike [`builtin_factory_ids`], which lists only factories eligible for
/// data-only preset fragments, this includes the catalog-only Vertex and
/// Bedrock factories and any future factory added to the builtin module.
#[must_use]
pub fn is_kernel_factory(id: &str) -> bool {
    builtin::factory(id).is_some()
}

/// The protocols served by all builtin factories, in catalog order.
///
/// The order is chat, responses, messages, generative, so a builtin-owned
/// preset without wire URLs defaults to chat, as `resolve` does for
/// endpoint-style catalog rows.
#[must_use]
pub fn builtin_protocols() -> Vec<ProtocolId> {
    let served: Vec<ProtocolId> = builtin_factory_ids()
        .iter()
        .filter_map(|id| factory(id))
        .flat_map(|factory| {
            DynUpstreamFactory::spec(factory.as_ref())
                .protocols()
                .to_vec()
        })
        .collect();
    [
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
        ProtocolId::GENERATIVE_LANGUAGE,
    ]
    .into_iter()
    .filter(|protocol| served.contains(protocol))
    .collect()
}

/// The preset record for `id`, when a linked plugin declares it.
#[must_use]
pub fn preset(id: &str) -> Option<&'static CompiledPreset> {
    compiled_presets().iter().find(|preset| preset.id == id)
}

/// Resolves a compiled preset id or alias and returns the alias's default region.
#[must_use]
pub fn find_preset(id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
    if let Some(preset) = preset(id) {
        return Some((preset, None));
    }
    if let Some(alias) = compiled_aliases().iter().find(|alias| alias.id == id) {
        return preset(alias.preset).map(|preset| (preset, alias.region));
    }
    None
}

/// Resolves a test fixture preset id or alias and returns the alias's default region.
#[cfg(feature = "test-fixtures")]
#[doc(hidden)]
#[must_use]
pub fn find_fixture_preset(id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
    if let Some(preset) = fixture_presets().iter().find(|preset| preset.id == id) {
        return Some((preset, None));
    }
    let alias = fixture_aliases().iter().find(|alias| alias.id == id)?;
    fixture_presets()
        .iter()
        .find(|preset| preset.id == alias.preset)
        .map(|preset| (preset, alias.region))
}

/// Every inbound surface the linked plugins contribute.
///
/// Surfaces on contributions of any other [`PluginKind`] are ignored.
#[must_use]
pub fn surfaces() -> Vec<Arc<dyn InboundSurface>> {
    let mut found = Vec::new();
    for (kind, _, contribution) in contributions() {
        if kind == PluginKind::Inbound {
            found.extend(contribution.surfaces().iter().cloned());
        }
    }
    found
}

fn upstream_in(
    contributions: &[(PluginKind, &'static str, hofund_plugin_sdk::Contribution)],
    id: &str,
) -> Option<Arc<dyn DynUpstreamFactory>> {
    contributions.iter().find_map(|(kind, name, contribution)| {
        if *kind == PluginKind::Provider && *name == id {
            contribution.upstreams().first().cloned()
        } else {
            None
        }
    })
}

/// The upstream factory for `id`, when that provider plugin is linked and contributes one.
#[must_use]
pub fn upstream(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    upstream_in(&contributions(), id)
}

fn subscription_in(
    contributions: &[(PluginKind, &'static str, hofund_plugin_sdk::Contribution)],
    id: &str,
) -> Option<Arc<dyn DynSubscriptionFactory>> {
    contributions.iter().find_map(|(kind, name, contribution)| {
        if *kind == PluginKind::Subscription && *name == id {
            contribution.subscriptions().first().cloned()
        } else {
            None
        }
    })
}

/// The subscription factory for `id`, when that subscription plugin is linked.
#[must_use]
pub fn subscription(id: &str) -> Option<Arc<dyn DynSubscriptionFactory>> {
    subscription_in(&contributions(), id)
}

fn surface_in(
    contributions: &[(PluginKind, &'static str, hofund_plugin_sdk::Contribution)],
    id: &str,
) -> Option<Arc<dyn InboundSurface>> {
    contributions.iter().find_map(|(kind, name, contribution)| {
        if *kind == PluginKind::Inbound && *name == id {
            contribution.surfaces().first().cloned()
        } else {
            None
        }
    })
}

/// The inbound surface for `id`, when that inbound plugin is linked and contributes one.
#[must_use]
pub fn surface(id: &str) -> Option<Arc<dyn InboundSurface>> {
    surface_in(&contributions(), id)
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
    use hofund_plugin_api::{CredentialStyle, ListKind, PluginKind, ProtocolId};

    use super::{
        match_route, subscription, subscription_in, surface, surface_in, upstream, upstream_in,
    };

    #[test]
    fn same_id_lookups_ignore_contributions_of_another_kind() {
        let provider = [(
            PluginKind::Provider,
            "shared",
            hofund_plugin_relay::contribute(),
        )];
        let non_provider = [(
            PluginKind::Inbound,
            "shared",
            hofund_plugin_relay::contribute(),
        )];
        assert!(upstream_in(&provider, "shared").is_some());
        assert!(upstream_in(&non_provider, "shared").is_none());

        let non_inbound = [(
            PluginKind::Provider,
            "shared",
            hofund_plugin_echo::contribute(),
        )];
        let inbound = [(
            PluginKind::Inbound,
            "shared",
            hofund_plugin_echo::contribute(),
        )];
        assert!(surface_in(&non_inbound, "shared").is_none());
        assert!(surface_in(&inbound, "shared").is_some());
    }

    #[test]
    fn subscription_lookup_ignores_other_kinds() {
        #[cfg(feature = "test-fixtures")]
        {
            assert!(subscription("fixture-sub").is_some());
            assert_eq!(
                subscription("fixture-sub")
                    .expect("fixture")
                    .spec()
                    .plugin()
                    .as_str(),
                "fixture-sub"
            );
            assert!(upstream("fixture-sub").is_none());
        }
        let provider = [(
            PluginKind::Provider,
            "shared",
            hofund_plugin_relay::contribute(),
        )];
        assert!(subscription_in(&provider, "shared").is_none());
    }

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
        assert!(super::preset("missing").is_none());
        assert_eq!(
            super::preset("openai").map(|preset| preset.plugin),
            Some("openai-compat")
        );
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
    fn builtin_factory_list_matches_fragment_factories() {
        assert_eq!(
            super::builtin_factory_ids(),
            ["anthropic", "generative", "openai-compat"]
        );
        for (id, protocols) in [
            (
                "openai-compat",
                &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES][..],
            ),
            ("anthropic", &[ProtocolId::ANTHROPIC_MESSAGES][..]),
            ("generative", &[ProtocolId::GENERATIVE_LANGUAGE][..]),
        ] {
            let factory = super::factory(id).expect("fragment factory");
            assert_eq!(factory.spec().plugin().as_str(), id);
            assert_eq!(factory.spec().protocols(), protocols);
        }
    }

    #[test]
    fn kernel_factory_predicate_covers_builtins_and_excludes_linked_plugins() {
        for id in [
            "openai-compat",
            "anthropic",
            "generative",
            "google-vertex",
            "bedrock",
        ] {
            assert!(super::builtin::factory(id).is_some(), "{id}");
            assert!(super::is_kernel_factory(id), "{id}");
        }
        for id in ["azure", "missing"] {
            assert!(super::builtin::factory(id).is_none(), "{id}");
            assert!(!super::is_kernel_factory(id), "{id}");
        }
        for (_, id, _) in super::contributions() {
            assert!(!super::is_kernel_factory(id), "{id}");
        }
    }

    #[test]
    fn bedrock_spec_declares_sigv4() {
        let factory = super::factory("bedrock").expect("bedrock");
        assert_eq!(factory.spec().credential(), CredentialStyle::SigV4);
    }
}
