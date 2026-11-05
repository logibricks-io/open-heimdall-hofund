//! On-disk models.dev cache beside the hofund data root.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use hofund_plugin_sdk::json::Value;

/// Relative path under the hofund data directory (same as the kernel catalog cache).
pub(crate) const CACHE_FILE: &str = "catalog/models.dev.json";

/// Maximum bytes read from the cache file (models.dev envelopes are well under 1 MiB).
const READ_CAP: u64 = 1024 * 1024;

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
        let meta = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !meta.is_file() || meta.len() > READ_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model catalog cache must be a regular file within the size cap",
            ));
        }
        let mut bytes = vec![0_u8; meta.len() as usize];
        std::fs::File::open(&self.path)?.read_exact(&mut bytes)?;
        let root: Value = hofund_plugin_sdk::json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(root.get("catalog").cloned())
    }
}
