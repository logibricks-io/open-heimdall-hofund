//! On-disk models.dev cache beside the hofund data root.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use hofund_plugin_sdk::json::Value;

/// Relative path under the hofund data directory (same as the kernel catalog cache).
pub(crate) const CACHE_FILE: &str = "catalog/models.dev.json";

/// Maximum bytes read from the cache file (matches [`hofund_plugin_sdk::page::BODY_CAP`]).
const READ_CAP: u64 = 8 * 1024 * 1024;

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
        let pre = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !pre.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model catalog cache must be a regular file",
            ));
        }
        let file = std::fs::OpenOptions::new().read(true).open(&self.path)?;
        let opened = file.metadata()?;
        if !opened.is_file() || !same_file(&pre, &opened) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model catalog cache path changed while opening",
            ));
        }
        // Reading at most one byte past the cap bounds memory however large the file is.
        let mut bytes = Vec::new();
        file.take(READ_CAP.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(READ_CAP.saturating_add(1)) > READ_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model catalog cache exceeds the size cap",
            ));
        }
        let root: Value = hofund_plugin_sdk::json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(root.get("catalog").cloned())
    }
}

/// Whether two metadata reads describe the same file (device and inode on Unix).
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn r5_41_symlink_at_cache_path_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("target.json");
        fs::write(&target, br#"{"catalog":{"anthropic":{"models":{}}}}"#).expect("target");
        let cache_path = dir.path().join(CACHE_FILE);
        fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &cache_path).expect("symlink");
        #[cfg(not(unix))]
        return;

        let cache = DevCatalogCache { path: cache_path };
        let error = cache.read_catalog().expect_err("symlink refused");
        assert!(
            error.to_string().contains("must be a regular file"),
            "{error}"
        );
    }

    #[test]
    fn r8_1_directory_at_cache_path_is_refused_before_open() {
        let dir = tempfile::tempdir().expect("dir");
        let cache_path = dir.path().join(CACHE_FILE);
        fs::create_dir_all(&cache_path).expect("mkdir");
        let cache = DevCatalogCache { path: cache_path };
        let error = cache.read_catalog().expect_err("directory refused");
        assert!(
            error.to_string().contains("must be a regular file"),
            "{error}"
        );
    }

    #[test]
    fn r8_2_oversize_cache_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let cache_path = dir.path().join(CACHE_FILE);
        fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
        let file = fs::File::create(&cache_path).expect("create");
        file.set_len(READ_CAP + 1).expect("grow");
        drop(file);
        let cache = DevCatalogCache { path: cache_path };
        let error = cache.read_catalog().expect_err("oversize refused");
        assert!(
            error.to_string().contains("exceeds the size cap"),
            "{error}"
        );
    }

    #[test]
    fn r8_3_cache_at_the_cap_is_read() {
        let dir = tempfile::tempdir().expect("dir");
        let cache_path = dir.path().join(CACHE_FILE);
        fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
        let mut body = br#"{"catalog":{"anthropic":{"models":{}}}}"#.to_vec();
        body.resize(usize::try_from(READ_CAP).expect("cap"), b' ');
        fs::write(&cache_path, &body).expect("write");
        let cache = DevCatalogCache { path: cache_path };
        assert!(cache.read_catalog().expect("read").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn r8_4_same_file_compares_device_and_inode() {
        let dir = tempfile::tempdir().expect("dir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"a").expect("a");
        fs::write(&b, b"b").expect("b");
        let meta_a = fs::metadata(&a).expect("meta a");
        let meta_b = fs::metadata(&b).expect("meta b");
        assert!(same_file(&meta_a, &fs::metadata(&a).expect("meta a again")));
        assert!(!same_file(&meta_a, &meta_b));
    }
}
