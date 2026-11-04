//! Persistent X-Device-Mid for zcode.z.ai portal requests.

use std::io::Write;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{HostContext, PluginError};

pub(crate) fn device_path(host: &HostContext) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join("zcode-device-mid"))
}

pub(crate) fn device_mid(path: &Path) -> Result<String, PluginError> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let id = text.trim().to_ascii_lowercase();
        if uuid_v4(&id) {
            return Ok(id);
        }
    }
    let id = hofund_plugin_sdk::uuid::v4()
        .map_err(|error| PluginError::io(format!("cannot generate a Z Code device id: {error}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Z Code device id directory"))?;
    }
    write_private_file(path, format!("{id}\n").as_bytes())?;
    Ok(id)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| PluginError::io("cannot write the Z Code device id"))?;
        file.write_all(bytes)
            .map_err(|_| PluginError::io("cannot write the Z Code device id"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
            .map_err(|_| PluginError::io("cannot write the Z Code device id"))?;
        Ok(())
    }
}

fn uuid_v4(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let pattern = b"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
    for (index, byte) in bytes.iter().enumerate() {
        match pattern[index] {
            b'x' => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
            b'-' if *byte != b'-' => return false,
            _ => {}
        }
    }
    true
}
