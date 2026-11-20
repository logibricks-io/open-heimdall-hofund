//! Role identifiers priced as a named model.
//!
//! The strings stay in this crate. The vendor-neutral pricer receives them as
//! data and does not name a vendor itself.

/// Role ids and the model whose list price they use.
///
/// A merely similar name is absent, so it stays unpriced.
#[must_use]
pub fn orchestrator_price_ids() -> &'static [(&'static str, &'static str)] {
    &[
        ("gemini-pro-orchestrator", "gemini-4-carbon"),
        ("gemini-pro-orchestrator-carbon", "gemini-4-carbon"),
        ("gemini-pro-orchestrator-argon", "gemini-4-argon"),
        ("gemini-flash-worker-agy", "gemini-4-hydrogen"),
    ]
}

#[cfg(test)]
mod tests {
    use super::orchestrator_price_ids;

    #[test]
    fn d222_orchestrator_roles_price_as_the_named_model() {
        let pairs = orchestrator_price_ids();
        assert_eq!(
            pairs
                .iter()
                .find(|(role, _)| *role == "gemini-pro-orchestrator")
                .map(|(_, priced)| *priced),
            Some("gemini-4-carbon")
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(role, _)| *role == "gemini-pro-orchestrator-argon")
                .map(|(_, priced)| *priced),
            Some("gemini-4-argon")
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(role, _)| *role == "gemini-flash-worker-agy")
                .map(|(_, priced)| *priced),
            Some("gemini-4-hydrogen")
        );
        assert!(
            pairs
                .iter()
                .all(|(role, _)| *role != "gemini-pro-orchestrator-neon")
        );
    }
}
