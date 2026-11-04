//! Static model ids when billing does not list models.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError};

use crate::site::Site;

pub(crate) fn fallback(site: Site) -> Result<ModelList, PluginError> {
    let ids = match site {
        Site::China => &["deepseek-v3", "gpt-4o"][..],
        Site::International => &["gemini-flash", "gpt-4o"][..],
    };
    let models = ids
        .iter()
        .filter_map(|id| ModelSpec::new(*id, Some((*id).to_owned())).ok())
        .collect();
    Ok(ModelList::new(models, None))
}
