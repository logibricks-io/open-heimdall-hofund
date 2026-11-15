//! Routing labels that share one subscription plugin and one account store.

/// Maps a config `[[subscriptions]]` id to the plugin id that owns secrets.
const STORAGE_ALIASES: &[(&str, &str)] = &[("qoder-cn", "qoder")];

/// The plugin id that owns account secrets for `id`.
#[must_use]
pub fn storage_id(id: &str) -> &str {
    STORAGE_ALIASES
        .iter()
        .find_map(|(alias, storage)| (*alias == id).then_some(*storage))
        .unwrap_or(id)
}

/// Extra routing labels appended after linked subscription ids.
#[must_use]
pub const fn routing_labels() -> &'static [(&'static str, &'static str)] {
    STORAGE_ALIASES
}
