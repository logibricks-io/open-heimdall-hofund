//! On-disk models.dev cache beside the hofund data root.

use std::io;
use std::path::{Path, PathBuf};

use hofund_plugin_sdk::json::Value;

/// Relative path under the hofund data directory (same as the kernel catalog cache).
pub(crate) const CACHE_FILE: &str = "catalog/models.dev.json";

/// Location of the cached models.dev envelope the daemon maintains.
#[derive(Clone, Debug)]
pub(crate) struct DevCatalogCache {
    /// Absolute path of the cache file.
    pub path: PathBuf,
}

impl DevCatalogCache {
    /// Cache file under the daemon data root.
    pub(crate) fn beside_data_root(data_root: &Path) -> Self {
        Self {
            path: data_root.join(CACHE_FILE),
        }
    }

    /// Reads the stored catalog object when the file exists.
    ///
    /// # Errors
    ///
    /// Returns an I/O or JSON error when the file exists but cannot be read or parsed.
    pub(crate) fn read_catalog(&self) -> io::Result<Option<Value>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let root: Value = hofund_plugin_sdk::json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(root.get("catalog").cloned())
    }
}
