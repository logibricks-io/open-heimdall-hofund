//! Registry instance lookups used by the daemon.
//!
//! The compatibility adapter temporarily keeps the daemon's direct registry
//! dependency in this one module. Registry-owned plain data and pure selection
//! functions are also re-exported here so the later registry switch has one
//! edge to replace.

use std::sync::Arc;

use hofund_plugin_api::{AgentDescription, DynUpstreamFactory, InboundSurface, ProtocolId};

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
}

/// Compatibility adapter backed by the compile-time plugin registry.
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
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hofund-registry-sources-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("nested/deeper")).expect("directories");
        for path in [
            root.join("a.rs"),
            root.join("nested/b.rs"),
            root.join("nested/deeper/c.rs"),
            root.join("nested/registry.rs"),
            root.join("notes.txt"),
        ] {
            std::fs::write(path, "").expect("fixture");
        }

        assert_eq!(
            rust_sources(&root),
            [
                root.join("a.rs"),
                root.join("nested/b.rs"),
                root.join("nested/deeper/c.rs"),
            ]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
