//! Agent sign-in switching is not written into Z Code config files.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};

pub(crate) async fn write(
    _account: &AccountId,
    _home: &Path,
    _host: &HostContext,
) -> Result<(), PluginError> {
    Err(PluginError::config(
        "Z Code agent switching is not implemented; the Z Code app keeps its own encrypted credential store",
    ))
}
