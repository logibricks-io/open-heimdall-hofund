//! Registry instance lookups used by the daemon.
//!
//! `hofundd` links the registry itself and passes its own [`DaemonRegistry`]
//! to [`crate::start_linked`]. The compatibility adapter here serves the
//! desktop until its rework, and keeps the
//! daemon's remaining direct registry dependency in this one module.
//! Registry-owned plain data and pure selection functions are also
//! re-exported here so the later registry switch has one edge to replace.

use std::sync::Arc;

use hofund_plugin_api::{
    AgentDescription, DynSubscriptionFactory, DynUpstreamFactory, InboundSurface, ProtocolId,
};

pub use hofund_plugin_registry::CompiledPreset;
pub(crate) use hofund_plugin_registry::{
    model_plan_of, protocol_for, resolve_region, select, wire_for_protocol,
};

/// Registry operations needed to construct and serve the daemon.
pub trait DaemonRegistry: Send + Sync {
    /// Returns the upstream factory for `id`.
    fn factory(&self, id: &str) -> Option<Arc<dyn DynUpstreamFactory>>;

    /// Whether `id` names a kernel-owned factory.
    fn is_kernel_factory(&self, id: &str) -> bool;

    /// Factory ids eligible for builtin-owned compiled presets.
    fn builtin_factory_ids(&self) -> &'static [&'static str];

    /// Protocols served by all builtin factories, in catalog order.
    fn builtin_protocols(&self) -> Vec<ProtocolId>;

    /// Resolves a compiled preset id or alias and its default region.
    fn find_preset(&self, id: &str) -> Option<(&'static CompiledPreset, Option<&'static str>)>;

    /// Returns static coding-agent descriptions.
    fn agents(&self) -> &'static [&'static AgentDescription];

    /// Returns every contributed inbound surface.
    fn surfaces(&self) -> Vec<Arc<dyn InboundSurface>>;

    /// Returns the subscription factory for `id`. The default links none.
    fn subscription(&self, id: &str) -> Option<Arc<dyn DynSubscriptionFactory>> {
        let _ = id;
        None
    }

    /// The linked plugin that serves a kernel-built base on `host` for
    /// `protocol`. The default claims none.
    fn host_plugin(&self, protocol: ProtocolId, host: &str) -> Option<&'static str> {
        let _ = (protocol, host);
        None
    }

    /// The linked vendor plan reader. The default links none.
    fn plan_quota(&self) -> Option<Arc<dyn hofund_plugin_api::PlanQuota>> {
        None
    }

    /// Subscription plugin ids the registry linked.
    fn subscription_ids(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Agent sign-in declaration for subscription `id`.
    fn agent_sign_in(&self, id: &str) -> Option<hofund_plugin_api::AgentSignIn> {
        self.subscription(id)
            .and_then(|factory| factory.spec().agent_sign_in())
    }

    /// The linked vendor balance reader. The default links none.
    fn balance_quota(&self) -> Option<Arc<dyn hofund_plugin_api::BalanceQuota>> {
        None
    }

    /// Accounts an upstream plugin declares may take requests at once.
    fn upstream_max_accounts(&self, id: &str) -> u64 {
        self.factory(id)
            .map_or(0, |factory| factory.spec().max_accounts())
    }

    /// Accounts a subscription plugin declares may take requests at once.
    fn subscription_max_accounts(&self, id: &str) -> u64 {
        self.subscription(id)
            .map_or(0, |factory| factory.spec().max_accounts())
    }
}

/// Compatibility adapter backed by the compile-time plugin registry.
///
/// The desktop and `hofundd subscription` use it until the desktop rework.
/// `hofundd` constructs its own adapter instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegistryAdapter;

impl DaemonRegistry for RegistryAdapter {
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

    fn host_plugin(&self, protocol: ProtocolId, host: &str) -> Option<&'static str> {
        hofund_plugin_registry::host_plugin(protocol, host)
    }

    fn plan_quota(&self) -> Option<Arc<dyn hofund_plugin_api::PlanQuota>> {
        hofund_plugin_registry::plan_quota()
    }

    fn balance_quota(&self) -> Option<Arc<dyn hofund_plugin_api::BalanceQuota>> {
        hofund_plugin_registry::balance_quota()
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

    fn subscription_ids(&self) -> Vec<&'static str> {
        hofund_plugin_registry::subscription_ids()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn rust_sources(root: &Path) -> Vec<PathBuf> {
        let mut sources = Vec::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).expect("daemon sources") {
                let path = entry.expect("source entry").path();
                if path.is_dir() {
                    directories.push(path);
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                    && path.file_name().and_then(|name| name.to_str()) != Some("registry.rs")
                {
                    sources.push(path);
                }
            }
        }
        sources.sort();
        sources
    }

    #[test]
    fn production_registry_calls_stay_in_this_module() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for path in rust_sources(&root) {
            let source = std::fs::read_to_string(&path).expect("source text");
            let mut test_item = false;
            let mut test_depth = 0_u32;
            let mut test_braces = false;
            for line in source.lines() {
                let trimmed = line.trim();
                if test_item {
                    let opens = line.matches('{').count() as u32;
                    let closes = line.matches('}').count() as u32;
                    test_braces |= opens > 0;
                    test_depth = test_depth.saturating_add(opens).saturating_sub(closes);
                    if test_braces && test_depth == 0 {
                        test_item = false;
                        test_braces = false;
                    }
                    continue;
                }
                if trimmed == "#[cfg(test)]" {
                    test_item = true;
                    continue;
                }
                assert!(
                    !line.contains("hofund_plugin_registry"),
                    "direct registry call in {}: {line}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn rust_sources_recurses_and_skips_registry() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("nested/deeper")).expect("directories");
        for path in [
            root.path().join("a.rs"),
            root.path().join("nested/b.rs"),
            root.path().join("nested/deeper/c.rs"),
            root.path().join("nested/registry.rs"),
            root.path().join("notes.txt"),
        ] {
            std::fs::write(path, "").expect("fixture");
        }

        assert_eq!(
            rust_sources(root.path()),
            [
                root.path().join("a.rs"),
                root.path().join("nested/b.rs"),
                root.path().join("nested/deeper/c.rs"),
            ]
        );
    }
}

#[cfg(test)]
mod registry_defaults {
    use std::sync::Arc;

    use hofund_plugin_api::{
        ConfigError, CredentialStyle, DynSubscriptionFactory, DynUpstreamFactory, ListKind,
        PluginId, ProtocolId, RawTable, Schema, SignInFlowKind, SubscriptionSpec, UpstreamSpec,
    };

    use super::{CompiledPreset, DaemonRegistry};

    static UPSTREAM_SPEC: UpstreamSpec = UpstreamSpec::from_static(
        PluginId::from_static("test-upstream"),
        &[ProtocolId::OPENAI_CHAT],
        CredentialStyle::Bearer,
        ListKind::Unsupported,
    )
    .with_max_accounts(9);

    static SUBSCRIPTION_SPEC: SubscriptionSpec = SubscriptionSpec::from_static(
        PluginId::from_static("test-subscription"),
        "Test subscription",
        SignInFlowKind::Device,
    )
    .with_max_accounts(11);

    struct EmptyConfig;
    impl hofund_plugin_api::ValidConfig for EmptyConfig {}

    struct UpstreamOnly(Arc<dyn DynUpstreamFactory>);
    struct SubscriptionOnly(Arc<dyn DynSubscriptionFactory>);

    struct UpstreamFactory;
    struct SubscriptionFactory;

    impl hofund_plugin_api::UpstreamFactory for UpstreamFactory {
        type Config = EmptyConfig;

        fn spec(&self) -> &'static UpstreamSpec {
            &UPSTREAM_SPEC
        }

        fn config_schema(&self) -> Schema {
            Schema::empty()
        }

        fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
            Ok(EmptyConfig)
        }

        fn build(
            &self,
            _cfg: &Self::Config,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<Arc<dyn hofund_plugin_api::UpstreamProvider>, hofund_plugin_api::BuildError>
        {
            Err(hofund_plugin_api::BuildError::new("unused in test"))
        }
    }

    impl hofund_plugin_api::SubscriptionFactory for SubscriptionFactory {
        type Config = EmptyConfig;

        fn spec(&self) -> &'static SubscriptionSpec {
            &SUBSCRIPTION_SPEC
        }

        fn config_schema(&self) -> Schema {
            Schema::empty()
        }

        fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
            Ok(EmptyConfig)
        }

        fn build(
            &self,
            _cfg: &Self::Config,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<Arc<dyn hofund_plugin_api::Subscription>, hofund_plugin_api::BuildError>
        {
            Err(hofund_plugin_api::BuildError::new("unused in test"))
        }
    }

    impl DaemonRegistry for UpstreamOnly {
        fn factory(&self, _id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
            Some(Arc::clone(&self.0))
        }

        fn is_kernel_factory(&self, _id: &str) -> bool {
            false
        }

        fn builtin_factory_ids(&self) -> &'static [&'static str] {
            &[]
        }

        fn builtin_protocols(&self) -> Vec<ProtocolId> {
            Vec::new()
        }

        fn find_preset(
            &self,
            _id: &str,
        ) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
            None
        }

        fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
            &[]
        }

        fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
            Vec::new()
        }
    }

    impl DaemonRegistry for SubscriptionOnly {
        fn factory(&self, _id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
            None
        }

        fn is_kernel_factory(&self, _id: &str) -> bool {
            false
        }

        fn builtin_factory_ids(&self) -> &'static [&'static str] {
            &[]
        }

        fn builtin_protocols(&self) -> Vec<ProtocolId> {
            Vec::new()
        }

        fn find_preset(
            &self,
            _id: &str,
        ) -> Option<(&'static CompiledPreset, Option<&'static str>)> {
            None
        }

        fn agents(&self) -> &'static [&'static hofund_plugin_api::AgentDescription] {
            &[]
        }

        fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
            Vec::new()
        }

        fn subscription(&self, _id: &str) -> Option<Arc<dyn DynSubscriptionFactory>> {
            Some(Arc::clone(&self.0))
        }
    }

    #[test]
    fn p3z_default_max_accounts_reads_linked_factory_specs() {
        let upstream = UpstreamOnly(Arc::new(UpstreamFactory));
        assert_eq!(upstream.upstream_max_accounts("ignored"), 9);
        let subscription = SubscriptionOnly(Arc::new(SubscriptionFactory));
        assert_eq!(subscription.subscription_max_accounts("ignored"), 11);
    }
}
