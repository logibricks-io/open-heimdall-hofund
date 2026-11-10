//! Claude Code bridge state and stream-json codec.
//!
//! This slice lands the [`Bridge`] stub, the stream-json decoder, and a
//! test-only fake runner. `Subscription::upstream` stays unsupported
//! until a later slice completes a text-only turn on the fake runner.

pub mod decode;

#[cfg(test)]
pub(crate) mod runner;

/// Owns Claude Code bridge runs for one plugin instance.
///
/// Later slices hold child handles, idle runs, and callback registrations
/// here. This slice only constructs an empty table and does not serve
/// requests.
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
