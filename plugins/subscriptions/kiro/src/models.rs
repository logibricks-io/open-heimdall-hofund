//! Static Kiro model ids the reference lists for Claude-family models.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError};

pub(crate) fn catalog() -> Result<ModelList, PluginError> {
    let models = vec![
        ModelSpec::new("claude-sonnet-4.5", Some("Claude Sonnet 4.5".to_owned()))?
            .with_endpoints(vec!["/v1/messages".to_owned()]),
        ModelSpec::new("claude-sonnet-4.6", Some("Claude Sonnet 4.6".to_owned()))?
            .with_endpoints(vec!["/v1/messages".to_owned()]),
    ];
    Ok(ModelList::new(models, None))
}
