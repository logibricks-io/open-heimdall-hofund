//! Claude Code bridge state, stream-json codec, and text-only start.

pub mod decode;
pub mod run;
pub mod wsl;

#[cfg(test)]
pub(crate) mod runner;

/// Owns Claude Code bridge runs for one plugin instance.
///
/// This slice may return an [`hofund_plugin_api::UpstreamProvider`] for a
/// text-only turn. Child handles, idle runs, and callback registrations
/// land in later slices.
#[derive(Debug, Default)]
pub struct Bridge {}

impl Bridge {
    /// Builds an empty bridge table.
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }
}

#[cfg(test)]
mod tests {
    use super::Bridge;

    #[test]
    fn a_new_bridge_is_empty() {
        let _bridge = Bridge::new();
    }
}
