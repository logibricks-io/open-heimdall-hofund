//! models.dev catalog cache, background refresh, and spec lookup.
#![allow(missing_docs)]

mod cache;
mod http;
mod parse;
mod spec;
mod sync;

use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub use cache::{CACHE_FILE, CacheEnvelope, CacheFile};
pub use hofund_config::CatalogSettings;
pub use http::ReqwestFetcher;
pub use parse::{
    CatalogIndex, DEFAULT_CATALOG_URL, DevCatalog, ResolvedEntry, build_index, parse_catalog,
};
pub use spec::{LEAST_WINDOW, ModelSpec, Price};
pub use sync::{CatalogFetcher, FetchResponse, FetchResult, Syncer, envelope_etag, is_stale};

/// Shared catalog state: disk cache, indexes, and refresh settings.
pub struct ModelCatalog {
    cache: CacheFile,
    settings: CatalogSettings,
    syncer: Syncer,
    index: RwLock<Option<CatalogIndex>>,
}

impl ModelCatalog {
    /// Opens or creates the cache under `data_root` and loads indexes when a cache exists.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the cache exists but cannot be read.
    pub fn open(
        data_root: &Path,
        settings: CatalogSettings,
        fetcher: Arc<dyn CatalogFetcher>,
    ) -> io::Result<Arc<Self>> {
        let cache = CacheFile::beside_data_root(data_root);
        let index = cache::load_index(&cache, &settings.overrides)?;
        Ok(Arc::new(Self {
            syncer: Syncer::new(cache.clone(), fetcher),
            cache,
            settings,
            index: RwLock::new(index),
        }))
    }

    /// Reloads indexes from disk after a sync or config change.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the cache cannot be read.
    pub fn reload(&self) -> io::Result<()> {
        let index = cache::load_index(&self.cache, &self.settings.overrides)?;
        if let Ok(mut guard) = self.index.write() {
            *guard = index;
        }
        Ok(())
    }

    /// Spec metadata for a route or catalog model id.
    #[must_use]
    pub fn spec(&self, model: &str) -> Option<ModelSpec> {
        self.resolve(model).map(|entry| entry.spec)
    }

    /// Full catalog row for a model id, including list prices when known.
    #[must_use]
    pub fn resolve(&self, model: &str) -> Option<ResolvedEntry> {
        self.index
            .read()
            .ok()
            .and_then(|guard| guard.as_ref()?.resolve(model).cloned())
    }

    #[must_use]
    pub fn settings(&self) -> &CatalogSettings {
        &self.settings
    }

    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache.path
    }

    /// Whether the on-disk cache is older than the configured refresh interval.
    #[must_use]
    pub fn stale(&self) -> bool {
        if self.settings.refresh == Duration::MAX {
            return false;
        }
        is_stale(self.cache.modified(), self.settings.refresh)
    }

    /// Fetches when needed and reloads memory indexes. Never required on a request path.
    ///
    /// # Errors
    ///
    /// Returns an error when a new payload cannot be stored and no cache exists.
    pub fn refresh(&self) -> io::Result<FetchResult> {
        let url = self.settings.fetch_url();
        let result = self.syncer.sync(url)?;
        if matches!(result, FetchResult::Updated | FetchResult::NotModified) {
            self.reload()?;
        }
        Ok(result)
    }

    /// Starts a background loop that refreshes stale caches. Returns a join handle.
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            let poll = catalog.settings.poll;
            if poll == Duration::MAX {
                return;
            }
            let mut interval = tokio::time::interval(poll);
            loop {
                interval.tick().await;
                if !catalog.stale() {
                    continue;
                }
                let refresh = Arc::clone(&catalog);
                let result = tokio::task::spawn_blocking(move || refresh.refresh()).await;
                if let Ok(Err(error)) = result {
                    tracing::warn!(?error, "model catalog refresh failed");
                }
            }
        })
    }
}
