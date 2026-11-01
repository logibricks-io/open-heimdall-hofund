//! Network refresh with ETag revalidation and offline fallback.

use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::cache::{CacheEnvelope, CacheFile};
use crate::parse;

/// Result of one catalog fetch attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchResult {
    /// The server returned 304 and the on-disk body stays as it was.
    NotModified,
    /// A new body was stored.
    Updated,
    /// The network failed but an existing cache was kept.
    OfflineFallback,
    /// No cache existed and the fetch failed.
    Failed,
}

/// HTTP GET used by [`Syncer`]. Tests inject a local server.
pub trait CatalogFetcher: Send + Sync {
    /// Performs the GET. `if_none_match` is sent as `If-None-Match` when set.
    fn fetch(&self, url: &str, if_none_match: Option<&str>) -> Result<FetchResponse, io::Error>;
}

/// One HTTP response from [`CatalogFetcher`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchResponse {
    NotModified,
    Ok { body: Vec<u8>, etag: Option<String> },
    Status(u16),
}

pub struct Syncer {
    cache: CacheFile,
    fetcher: Arc<dyn CatalogFetcher>,
}

impl Syncer {
    #[must_use]
    pub fn new(cache: CacheFile, fetcher: Arc<dyn CatalogFetcher>) -> Self {
        Self { cache, fetcher }
    }

    /// Downloads when needed, honoring ETag and keeping the last good cache offline.
    ///
    /// # Errors
    ///
    /// Returns an error when a new body cannot be parsed or written.
    pub fn sync(&self, url: &str) -> io::Result<FetchResult> {
        let prior = self.cache.read()?;
        let etag = prior.as_ref().and_then(|envelope| envelope.etag.as_deref());
        match self.fetcher.fetch(url, etag) {
            Ok(FetchResponse::NotModified) | Ok(FetchResponse::Status(304)) => {
                Ok(FetchResult::NotModified)
            }
            Ok(FetchResponse::Status(_)) => Ok(FetchResult::Failed),
            Ok(FetchResponse::Ok { body, etag }) => {
                let catalog = parse::parse_catalog(&body).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
                self.cache.write(etag.as_deref(), &catalog)?;
                Ok(FetchResult::Updated)
            }
            Err(error) => {
                if prior.is_some() {
                    Ok(FetchResult::OfflineFallback)
                } else {
                    Err(error)
                }
            }
        }
    }

    #[must_use]
    pub fn cache(&self) -> &CacheFile {
        &self.cache
    }
}

#[must_use]
pub fn is_stale(modified: Option<SystemTime>, max_age: Duration) -> bool {
    let Some(modified) = modified else {
        return true;
    };
    modified.elapsed().map(|age| age > max_age).unwrap_or(true)
}

#[must_use]
pub fn envelope_etag(envelope: &CacheEnvelope) -> Option<&str> {
    envelope.etag.as_deref()
}
