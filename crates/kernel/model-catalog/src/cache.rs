//! On-disk cache envelope with ETag metadata.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::parse::{self, CatalogIndex, DevCatalog};

/// Relative path under the hofund data directory.
pub const CACHE_FILE: &str = "catalog/models.dev.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<DateTime<Utc>>,
    pub catalog: serde_json::Value,
}

impl CacheEnvelope {
    #[must_use]
    pub fn catalog(&self) -> Option<DevCatalog> {
        parse::parse_catalog(
            serde_json::to_vec(&self.catalog)
                .unwrap_or_default()
                .as_slice(),
        )
        .ok()
    }
}

#[derive(Clone, Debug)]
pub struct CacheFile {
    pub path: PathBuf,
}

impl CacheFile {
    #[must_use]
    pub fn beside_data_root(data_root: &Path) -> Self {
        Self {
            path: data_root.join(CACHE_FILE),
        }
    }

    /// # Errors
    ///
    /// Returns an I/O or JSON error when the file cannot be read or parsed.
    pub fn read(&self) -> io::Result<Option<CacheEnvelope>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let envelope: CacheEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Some(envelope))
    }

    /// # Errors
    ///
    /// Returns an I/O error when the directory or file cannot be written.
    pub fn write(&self, etag: Option<&str>, catalog: &DevCatalog) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let catalog_value = serde_json::to_value(catalog)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let envelope = CacheEnvelope {
            etag: etag.map(str::to_owned),
            saved_at: Some(Utc::now()),
            catalog: catalog_value,
        };
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&self.path, bytes)
    }

    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        fs::metadata(&self.path).ok()?.modified().ok()
    }
}

/// Loads the cache file when present and builds indexes.
///
/// # Errors
///
/// Returns an error when the cache exists but cannot be parsed.
pub fn load_index(
    cache: &CacheFile,
    overrides: &hofund_config::CatalogOverrides,
) -> io::Result<Option<CatalogIndex>> {
    let Some(envelope) = cache.read()? else {
        return Ok(None);
    };
    let catalog = envelope
        .catalog()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "catalog payload"))?;
    Ok(Some(parse::build_index(&catalog, overrides)))
}
