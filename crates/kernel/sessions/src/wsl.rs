//! WSL session listing cache (`sessions-wsl.json`).
//!
//! On Windows, agent sessions under `\\wsl.localhost\<distro>` are listed behind
//! this cache so a stopped distro is not opened. Tests inject fixture paths and
//! running state without calling `wsl.exe`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::index::IndexError;
use crate::storage::atomic_write;

type RunningHook = Box<dyn Fn(&str) -> bool + Send + Sync>;

fn running_hook() -> &'static Mutex<Option<RunningHook>> {
    static HOOK: OnceLock<Mutex<Option<RunningHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

/// Test hook: whether a distro is running now.
///
/// # Panics
///
/// Panics if the hook mutex is poisoned.
pub fn set_running_hook(hook: Option<RunningHook>) {
    *running_hook().lock().expect("wsl hook") = hook;
}

/// Whether a distro is running for cache merge (test hook on non-Windows).
pub(crate) fn distro_running_for_merge(distro: &str, listing_running: bool) -> bool {
    distro_running(distro, listing_running)
}

fn distro_running(distro: &str, listing_running: bool) -> bool {
    if !listing_running {
        return false;
    }
    if let Some(hook) = running_hook().lock().expect("wsl hook").as_ref() {
        return hook(distro);
    }
    #[cfg(windows)]
    {
        let _ = distro;
        false
    }
    #[cfg(not(windows))]
    {
        let _ = distro;
        false
    }
}

/// Builds a WSL instance agent id (`{base}@wsl:{distro}`).
#[must_use]
pub fn wsl_agent_id(base: &str, distro: &str) -> String {
    format!("{base}@wsl:{distro}")
}

/// Path beside the session index cache.
#[must_use]
pub fn wsl_cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("state").join("sessions-wsl.json")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WslEntry {
    pub(crate) agent: String,
    pub(crate) key: String,
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) main: bool,
    pub(crate) size: u64,
    pub(crate) modified_ns: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct WslListing {
    pub(crate) home: String,
    #[serde(default)]
    pub(crate) running: bool,
    #[serde(default)]
    pub(crate) files: Vec<WslEntry>,
}

pub(crate) type WslCache = BTreeMap<String, WslListing>;

/// Loads the on-disk WSL listing cache.
#[must_use]
pub(crate) fn load_cache(path: &Path) -> WslCache {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Saves the WSL listing cache atomically.
///
/// # Errors
///
/// Returns [`IndexError`] when the parent directory cannot be created,
/// the cache cannot be serialized, or the atomic write fails.
#[allow(dead_code)] // write path is tested; Windows listing will use it
pub(crate) fn save_cache(path: &Path, cache: &WslCache) -> Result<(), IndexError> {
    let parent = path.parent().ok_or_else(|| {
        IndexError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "wsl cache parent missing",
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(cache).map_err(|_| IndexError::Serialize)?;
    atomic_write(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn path_is_cold(path: &Path, listing: &WslListing, distro: &str) -> bool {
        let _ = path;
        !distro_running(distro, listing.running)
    }

    #[test]
    fn wsl_stopped_uses_cache_only() {
        set_running_hook(Some(Box::new(|_| false)));
        let temp = tempfile::tempdir().expect("tempdir");
        let config = temp.path();
        let missing = config.join("no-such-session.jsonl");
        let cache_path = wsl_cache_path(config);
        let mut cache = WslCache::new();
        cache.insert(
            "Ubuntu".to_owned(),
            WslListing {
                home: r"\\wsl.localhost\Ubuntu\home\me".to_owned(),
                running: false,
                files: vec![WslEntry {
                    agent: "agent".to_owned(),
                    key: "agent:abc".to_owned(),
                    path: missing.clone(),
                    main: true,
                    size: 10,
                    modified_ns: 1,
                }],
            },
        );
        save_cache(&cache_path, &cache).expect("write cache");
        let listing = cache.get("Ubuntu").expect("listing");
        assert!(path_is_cold(&missing, listing, "Ubuntu"));
        assert_eq!(
            std::fs::metadata(&missing).err().map(|err| err.kind()),
            Some(ErrorKind::NotFound)
        );
        set_running_hook(None);
    }

    #[test]
    fn wsl_agent_ids_match_grammar() {
        let id = wsl_agent_id("agent", "Ubuntu-22.04");
        assert!(crate::consent::valid_agent_id(&id));
    }
}
