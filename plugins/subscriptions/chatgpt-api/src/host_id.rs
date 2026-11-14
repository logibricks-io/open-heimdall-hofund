//! Stable machine id for OpenAI dynamic client registration.

use hofund_plugin_api::{HostContext, PluginError};
use hofund_plugin_sdk::uuid;

const FILE: &str = "chatgpt-api-host";
const PREFIX: &str = "urn:uuid:";

/// Returns this machine's `ext_agent_host_id`, creating it once.
pub(crate) fn load(host: &HostContext) -> Result<String, PluginError> {
    let dir = host.dirs().state_dir()?;
    let path = dir.join(FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        let id = String::from_utf8_lossy(&bytes).trim().to_owned();
        if id.starts_with(PREFIX) {
            return Ok(id);
        }
    }
    let id = format!(
        "{PREFIX}{}",
        uuid::v4().map_err(|error| {
            PluginError::internal("host id generation failed").with_source(error)
        })?
    );
    std::fs::create_dir_all(&dir)
        .map_err(|error| PluginError::io("cannot create plugin state dir").with_source(error))?;
    write_private(&path, format!("{id}\n"))?;
    Ok(id)
}

#[cfg(test)]
pub(crate) fn load_for_test(host: &HostContext, value: &str) -> Result<(), PluginError> {
    let dir = host.dirs().state_dir()?;
    let path = dir.join(FILE);
    std::fs::create_dir_all(&dir)
        .map_err(|error| PluginError::io("cannot create plugin state dir").with_source(error))?;
    write_private(&path, format!("{value}\n"))
}

fn write_private(path: &std::path::Path, text: String) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| PluginError::io("cannot write host id").with_source(error))?;
        file.write_all(text.as_bytes())
            .map_err(|error| PluginError::io("cannot write host id").with_source(error))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
            .map_err(|error| PluginError::io("cannot write host id").with_source(error))?;
        Ok(())
    }
}
