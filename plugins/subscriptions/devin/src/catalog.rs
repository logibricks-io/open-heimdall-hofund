//! Account model catalogs from the live RPC, CLI, and built-in snapshot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, ModelList, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::secret_text;

use crate::cli;
use crate::connect::configs;
use crate::creds::Stored;
use crate::models::{self, Family};

const CONFIGS_PATH: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
pub(crate) const FRESH: Duration = Duration::from_secs(5 * 60);
pub(crate) const RETRY: Duration = Duration::from_secs(60);
const BODY_CAP: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Catalog {
    pub(crate) families: Arc<Vec<Family>>,
    pub(crate) images: Arc<HashMap<String, bool>>,
    pub(crate) free: Arc<HashSet<String>>,
    pub(crate) prices: Arc<HashMap<String, configs::Price>>,
}

#[derive(Default)]
struct CacheEntry {
    catalog: Option<Catalog>,
    at: Duration,
    failed: Option<Duration>,
}

impl CacheEntry {
    fn fresh(&self, now: Duration) -> bool {
        self.catalog.is_some() && now.saturating_sub(self.at) < FRESH
    }

    fn retry_blocked(&self, now: Duration) -> bool {
        self.failed
            .is_some_and(|failed| now.saturating_sub(failed) < RETRY)
    }
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clears the in-process model-catalog cache for offline matrix tests.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn clear_catalog_cache_for_tests() {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

pub(crate) fn origin(stored: &Stored) -> String {
    std::env::var("WINDSURF_API_SERVER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| stored.credential.api_server.clone())
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

pub(crate) async fn list_models(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let catalog = families_for(host, account, stored).await;
    Ok(ModelList::new(
        models::listed_with_billing(
            &catalog.families,
            &catalog.images,
            &catalog.free,
            &catalog.prices,
        )?,
        None,
    ))
}

pub(crate) async fn families_for(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
) -> Catalog {
    let now = host.clock().monotonic();
    let key = account.as_str().to_owned();
    {
        let cache = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cache.get(&key)
            && entry.fresh(now)
        {
            return entry.catalog.clone().expect("fresh entry has a catalog");
        }
    }
    let blocked = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_some_and(|entry| entry.retry_blocked(now));
    if !blocked {
        match live_catalog(host, stored).await {
            Ok(catalog) => {
                cache()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        key,
                        CacheEntry {
                            catalog: Some(catalog.clone()),
                            at: now,
                            failed: None,
                        },
                    );
                return catalog;
            }
            Err(_) => {
                cache()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(key)
                    .or_default()
                    .failed = Some(now);
            }
        }
    }
    if let Some(families) = cli::families(host, stored).await {
        return Catalog {
            families: Arc::new(families),
            images: Arc::new(models::snapshot_images().clone()),
            free: Arc::new(HashSet::new()),
            prices: Arc::new(HashMap::new()),
        };
    }
    Catalog {
        families: Arc::new(snapshot_vec()),
        images: Arc::new(models::snapshot_images().clone()),
        free: Arc::new(HashSet::new()),
        prices: Arc::new(HashMap::new()),
    }
}

fn snapshot_vec() -> Vec<Family> {
    models::snapshot_families().to_vec()
}

async fn live_catalog(host: &HostContext, stored: &Stored) -> Result<Catalog, PluginError> {
    let body = configs::encode_request(secret_text(&stored.credential.key), os_name());
    let response = Call::post(format!("{}{CONFIGS_PATH}", origin(stored)))
        .header("Content-Type", "application/proto")
        .header("Connect-Protocol-Version", "1")
        .read_timeout(Duration::from_secs(15))
        .bytes(body)
        .open_raw(host.http(), 1)
        .await?;
    if !response.status().is_success() {
        return Err(PluginError::status_error(
            response.status().as_u16(),
            "GetCliModelConfigs failed",
        ));
    }
    let mut stream = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > BODY_CAP {
            return Err(PluginError::decode("GetCliModelConfigs is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let parsed = configs::parse(&bytes)
        .ok_or_else(|| PluginError::decode("GetCliModelConfigs returned no model families"))?;
    let mut images = models::snapshot_images().clone();
    images.extend(parsed.images);
    Ok(Catalog {
        families: Arc::new(parsed.families),
        images: Arc::new(images),
        free: Arc::new(parsed.free),
        prices: Arc::new(parsed.prices),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CacheEntry, FRESH, RETRY};

    #[test]
    fn cache_waits_for_fresh_expiry_and_failed_retry_delay() {
        let entry = CacheEntry {
            at: Duration::from_secs(100),
            failed: Some(Duration::from_secs(200)),
            ..CacheEntry::default()
        };
        assert!(!entry.fresh(Duration::from_secs(100 + FRESH.as_secs() - 1)));
        assert!(entry.retry_blocked(Duration::from_secs(200)));
        assert!(entry.retry_blocked(Duration::from_secs(200 + RETRY.as_secs() - 1)));
        assert!(!entry.retry_blocked(Duration::from_secs(200 + RETRY.as_secs())));
    }
}
