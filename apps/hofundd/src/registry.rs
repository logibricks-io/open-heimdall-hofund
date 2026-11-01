//! The production plugin registry link.
//!
//! `hofundd` is the composition root that links the compile-time plugin
//! registry and hands it to the daemon as a [`DaemonRegistry`].

use std::sync::Arc;

use hofund_daemon::{CompiledPreset, DaemonRegistry};
use hofund_plugin_api::{
    AgentDescription, DynSubscriptionFactory, DynUpstreamFactory, InboundSurface, ProtocolId,
};

/// The compile-time plugin registry, as the daemon sees it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinkedRegistry;

impl DaemonRegistry for LinkedRegistry {
    fn factory(&self, id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        hofund_plugin_registry::factory(id)
    }

    fn is_kernel_factory(&self, id: &str) -> bool {
        hofund_plugin_registry::is_kernel_factory(id)
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        hofund_plugin_registry::builtin_factory_ids()
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        hofund_plugin_registry::builtin_protocols()
    }

    fn find_preset(&self, id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
        hofund_plugin_registry::find_preset(id)
    }

    fn agents(&self) -> &'static [&'static AgentDescription] {
        hofund_plugin_registry::agents()
    }

    fn surfaces(&self) -> Vec<Arc<dyn InboundSurface>> {
        hofund_plugin_registry::surfaces()
    }

    fn subscription(&self, id: &str) -> Option<Arc<dyn DynSubscriptionFactory>> {
        hofund_plugin_registry::subscription(id)
    }

    fn upstream_max_accounts(&self, id: &str) -> u64 {
        hofund_plugin_registry::declared_max_accounts("provider", id).unwrap_or_else(|| {
            hofund_plugin_registry::factory(id).map_or(0, |factory| factory.spec().max_accounts())
        })
    }

    fn subscription_max_accounts(&self, id: &str) -> u64 {
        hofund_plugin_registry::declared_max_accounts("subscription", id).unwrap_or_else(|| {
            hofund_plugin_registry::subscription(id)
                .map_or(0, |factory| factory.spec().max_accounts())
        })
    }

    fn host_plugin(&self, protocol: ProtocolId, host: &str) -> Option<&'static str> {
        hofund_plugin_registry::host_plugin(protocol, host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: &[&str] = &[
        "openai",
        "anthropic",
        "azure",
        "bedrock",
        "bedrock-mantle",
        "google-vertex",
        "relay",
        "claude",
        "codex",
        "copilot",
        "gemini",
        "missing",
    ];

    #[test]
    fn factories_match_the_registry() {
        let linked = LinkedRegistry;
        assert!(!linked.builtin_factory_ids().is_empty());
        assert_eq!(
            linked.builtin_factory_ids(),
            hofund_plugin_registry::builtin_factory_ids()
        );
        let mut kernel = 0;
        let mut plugin = 0;
        for id in linked.builtin_factory_ids().iter().chain(IDS) {
            let ours = linked
                .factory(id)
                .map(|factory| std::ptr::from_ref(factory.spec()));
            let theirs = hofund_plugin_registry::factory(id)
                .map(|factory| std::ptr::from_ref(factory.spec()));
            assert_eq!(ours, theirs, "{id}");
            let is_kernel = linked.is_kernel_factory(id);
            assert_eq!(
                is_kernel,
                hofund_plugin_registry::is_kernel_factory(id),
                "{id}"
            );
            if is_kernel {
                kernel += 1;
            } else if ours.is_some() {
                plugin += 1;
            }
        }
        assert!(kernel > 0 && plugin > 0, "kernel {kernel}, plugin {plugin}");
        assert!(linked.factory("missing").is_none());
    }

    #[test]
    fn protocols_presets_agents_and_surfaces_match_the_registry() {
        let linked = LinkedRegistry;
        let protocols = linked.builtin_protocols();
        assert!(!protocols.is_empty());
        assert_eq!(protocols, hofund_plugin_registry::builtin_protocols());
        let mut found = 0;
        for preset in hofund_plugin_registry::compiled_presets() {
            let ours = linked
                .find_preset(preset.id)
                .map(|(row, region)| (std::ptr::from_ref(row), region));
            let theirs = hofund_plugin_registry::find_preset(preset.id)
                .map(|(row, region)| (std::ptr::from_ref(row), region));
            assert_eq!(ours, theirs, "{}", preset.id);
            found += usize::from(ours.is_some());
        }
        assert!(found > 0);
        assert!(linked.find_preset("missing").is_none());
        assert!(!linked.agents().is_empty());
        assert!(std::ptr::eq(
            linked.agents(),
            hofund_plugin_registry::agents()
        ));
        let surfaces: Vec<_> = linked
            .surfaces()
            .iter()
            .map(|surface| surface.id().as_str().to_owned())
            .collect();
        assert!(!surfaces.is_empty());
        let expected: Vec<_> = hofund_plugin_registry::surfaces()
            .iter()
            .map(|surface| surface.id().as_str().to_owned())
            .collect();
        assert_eq!(surfaces, expected);
    }

    #[test]
    fn subscriptions_and_host_plugins_match_the_registry() {
        let linked = LinkedRegistry;
        let mut linked_subscriptions = 0;
        for id in IDS {
            let ours = linked.subscription(id).is_some();
            assert_eq!(
                ours,
                hofund_plugin_registry::subscription(id).is_some(),
                "{id}"
            );
            linked_subscriptions += usize::from(ours);
        }
        assert!(linked_subscriptions > 0);
        let mut claimed = 0;
        for protocol in [
            ProtocolId::OPENAI_CHAT,
            ProtocolId::ANTHROPIC_MESSAGES,
            ProtocolId::BEDROCK_CONVERSE,
        ] {
            for host in [
                "api.openai.com",
                "bedrock-mantle.us-east-1.api.aws",
                "bedrock-runtime.us-east-1.amazonaws.com",
            ] {
                let ours = linked.host_plugin(protocol, host);
                assert_eq!(ours, hofund_plugin_registry::host_plugin(protocol, host));
                claimed += usize::from(ours.is_some());
            }
        }
        assert!(claimed > 0);
    }
}
