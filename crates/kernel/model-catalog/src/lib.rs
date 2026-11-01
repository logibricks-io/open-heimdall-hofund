//! models.dev catalog cache, background refresh, and spec lookup.

mod cache;
mod http;
mod parse;
mod spec;
mod sync;
mod visibility;

use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub use cache::{CACHE_FILE, CacheEnvelope, CacheFile};
pub use hofund_config::CatalogSettings;
pub use http::ReqwestFetcher;
pub use parse::{
    CatalogIndex, DEFAULT_CATALOG_URL, DevCatalog, ResolvedEntry, build_index, is_deprecated,
    parse_catalog,
};
pub use spec::{LEAST_WINDOW, ModelSpec, Price};
pub use sync::{CatalogFetcher, FetchResponse, FetchResult, Syncer, envelope_etag, is_stale};
pub use visibility::{
    CatalogVisibility, RETIRED_FOR, glob_match, hop_names, listable_route_models, match_patterns,
    model_names, normalize_patterns,
};

/// Shared catalog state: disk cache, indexes, and refresh settings.
pub struct ModelCatalog {
    cache: CacheFile,
    settings: RwLock<CatalogSettings>,
    syncer: Syncer,
    index: RwLock<Option<CatalogIndex>>,
    visibility: CatalogVisibility,
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
        let index = cache::load_index(&cache, &settings)?;
        let visibility = CatalogVisibility::open(settings.clone(), data_root);
        Ok(Arc::new(Self {
            syncer: Syncer::new(cache.clone(), fetcher),
            cache,
            settings: RwLock::new(settings),
            index: RwLock::new(index),
            visibility,
        }))
    }

    /// Reloads indexes from disk after a sync or config change.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the cache cannot be read.
    pub fn reload(&self) -> io::Result<()> {
        let settings = self.settings.read().ok().map(|guard| guard.clone());
        let Some(settings) = settings else {
            return Ok(());
        };
        let index = cache::load_index(&self.cache, &settings)?;
        if let Ok(mut guard) = self.index.write() {
            *guard = index;
        }
        self.visibility.reload_runtime();
        Ok(())
    }

    /// List and routing visibility derived from settings and runtime state.
    #[must_use]
    pub fn visibility(&self) -> &CatalogVisibility {
        &self.visibility
    }

    /// Replaces catalog settings after a configuration publish or reload.
    pub fn replace_settings(&self, settings: CatalogSettings) {
        if let Ok(mut guard) = self.settings.write() {
            *guard = settings.clone();
        }
        self.visibility.replace_settings(settings);
        if let Ok(settings) = self.settings.read().map(|guard| guard.clone())
            && let Ok(index) = cache::load_index(&self.cache, &settings)
            && let Ok(mut guard) = self.index.write()
        {
            *guard = index;
        }
    }

    /// Records that a provider's wire model was retired by the vendor.
    pub fn retire(&self, provider: &str, model: &str) {
        self.visibility.retire(provider, model);
    }

    /// Clears runtime retirement after a successful answer.
    pub fn unretire(&self, provider: &str, model: &str) {
        self.visibility.unretire(provider, model);
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

    /// Settings this catalog was opened with.
    #[must_use]
    pub fn settings(&self) -> CatalogSettings {
        self.settings
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Path of the backing cache file.
    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache.path
    }

    /// Whether the on-disk cache is older than the configured refresh interval.
    #[must_use]
    pub fn stale(&self) -> bool {
        let settings = self.settings();
        if settings.refresh == Duration::MAX {
            return false;
        }
        is_stale(self.cache.modified(), settings.refresh)
    }

    /// Fetches when needed and reloads memory indexes. Never required on a request path.
    ///
    /// # Errors
    ///
    /// Returns an error when a new payload cannot be stored and no cache exists.
    pub fn refresh(&self) -> io::Result<FetchResult> {
        let settings = self.settings();
        let url = settings.fetch_url();
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
            let poll = catalog.settings().poll;
            if poll == Duration::MAX {
                return;
            }
            let mut interval = tokio::time::interval(poll);
            loop {
                interval.tick().await;
                if !catalog.stale() {
                    continue;
                }
                if let Ok(Err(error)) = catalog.refresh_detached().await {
                    tracing::warn!(?error, "model catalog refresh failed");
                }
            }
        })
    }

    /// Runs [`Self::refresh`] on its own OS thread and reports the result.
    ///
    /// The fetch blocks on the network, so it is kept off the async runtime's
    /// blocking pool: a runtime shuts down without waiting for a slow or hung
    /// fetch, and the thread ends on its own once the fetch returns.
    pub fn refresh_detached(
        self: &Arc<Self>,
    ) -> tokio::sync::oneshot::Receiver<io::Result<FetchResult>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let catalog = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("hofund-catalog-sync".to_owned())
            .spawn(move || {
                let _ = send.send(catalog.refresh());
            });
        if let Err(error) = spawned {
            tracing::warn!(?error, "model catalog refresh thread did not start");
        }
        receive
    }
}
