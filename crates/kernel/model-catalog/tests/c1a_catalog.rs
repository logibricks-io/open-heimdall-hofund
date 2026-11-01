//! Integration tests for the models.dev catalog cache and spec lookup.

use std::sync::{Arc, Mutex};

use hofund_config::{CatalogOverrides, CatalogSettings};
use hofund_model_catalog::{
    CacheFile, CatalogFetcher, DevCatalog, FetchResponse, FetchResult, ModelCatalog, Syncer,
    build_index, parse_catalog,
};

struct MockFetcher {
    responses: Mutex<Vec<Result<FetchResponse, std::io::Error>>>,
}

impl MockFetcher {
    fn new(responses: Vec<Result<FetchResponse, std::io::Error>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl CatalogFetcher for MockFetcher {
    fn fetch(
        &self,
        _url: &str,
        _if_none_match: Option<&str>,
    ) -> Result<FetchResponse, std::io::Error> {
        match self.responses.lock().unwrap().pop() {
            Some(result) => result,
            None => Ok(FetchResponse::Status(500)),
        }
    }
}

const FIXTURE: &str = r#"{
  "acme": {
    "id": "acme",
    "models": {
      "gpt-demo": {
        "id": "gpt-demo",
        "name": "GPT Demo",
        "reasoning": true,
        "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
        "modalities": {"input": ["text", "image/jpeg"], "output": ["text"]},
        "limit": {"context": 128000, "output": 4096},
        "cost": {"input": 2.5, "output": 10.0, "cache_read": 0.25, "cache_write": 3.0}
      }
    }
  }
}"#;

fn temp_catalog(fetcher: Arc<dyn CatalogFetcher>) -> (tempfile::TempDir, Arc<ModelCatalog>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = CatalogSettings::default();
    let catalog = ModelCatalog::open(dir.path(), settings, fetcher).expect("open catalog");
    (dir, catalog)
}

#[test]
fn c1a_1_parse_fixture_shapes_limits_prices_and_modalities() {
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    let index = build_index(&catalog, &CatalogOverrides::default());
    let entry = index.resolve("gpt-demo").expect("entry");
    assert_eq!(entry.spec.context_window, Some(128_000));
    assert_eq!(entry.spec.max_output, Some(4096));
    assert!(entry.spec.vision);
    assert_eq!(entry.spec.reasoning_levels, ["low", "high"]);
    let price = entry.price.as_ref().expect("price");
    assert!((price.input - 2.5).abs() < f64::EPSILON);
}

#[test]
fn c1a_2_etag_304_reuses_existing_cache_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = CacheFile::beside_data_root(dir.path());
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    cache.write(Some("v1"), &catalog).expect("seed cache");
    let fetcher = Arc::new(MockFetcher::new(vec![Ok(FetchResponse::NotModified)]));
    let syncer = Syncer::new(cache.clone(), fetcher);
    assert_eq!(
        syncer.sync("http://example.test/api.json").expect("sync"),
        FetchResult::NotModified
    );
    let reread = cache.read().expect("read").expect("envelope");
    assert_eq!(reread.etag.as_deref(), Some("v1"));
}

#[test]
fn c1a_3_offline_fallback_keeps_last_cache_on_fetch_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = CacheFile::beside_data_root(dir.path());
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    cache.write(None, &catalog).expect("seed");
    let fetcher = Arc::new(MockFetcher::new(vec![Err(std::io::Error::other(
        "offline",
    ))]));
    let syncer = Syncer::new(cache, fetcher);
    assert_eq!(
        syncer.sync("http://example.test/api.json").expect("sync"),
        FetchResult::OfflineFallback
    );
}

#[derive(Default)]
struct RecordingFetcher {
    url: Mutex<Option<String>>,
}

impl CatalogFetcher for RecordingFetcher {
    fn fetch(&self, url: &str, _: Option<&str>) -> Result<FetchResponse, std::io::Error> {
        *self.url.lock().unwrap() = Some(url.to_owned());
        Err(std::io::Error::other("stop"))
    }
}

impl RecordingFetcher {
    fn last_url(&self) -> String {
        self.url.lock().unwrap().clone().unwrap_or_default()
    }
}

#[test]
fn c1a_4_mirror_url_is_used_for_sync_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = CacheFile::beside_data_root(dir.path());
    let fetcher = Arc::new(RecordingFetcher::default());
    let syncer = Syncer::new(cache, fetcher.clone());
    let _ = syncer.sync("http://mirror.test/custom.json");
    assert_eq!(fetcher.last_url(), "http://mirror.test/custom.json");
}

#[test]
fn c1a_5_override_wins_over_fetched_catalog() {
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    let settings = hofund_config::load(
        r#"
listen = "127.0.0.1:9"
[[catalog.overrides]]
model = "gpt-demo"
context = 2000
output = 512
vision = false
reasoning_levels = ["max"]
"#,
    )
    .expect("config");
    let index = build_index(&catalog, &settings.catalog.overrides);
    let entry = index.resolve("gpt-demo").expect("entry");
    assert_eq!(entry.spec.context_window, Some(2000));
    assert_eq!(entry.spec.max_output, Some(512));
    assert!(!entry.spec.vision);
    assert_eq!(entry.spec.reasoning_levels, ["max"]);
}

#[test]
fn c1a_6_spec_lookup_returns_none_without_cache() {
    let fetcher = Arc::new(MockFetcher::new(vec![]));
    let (_dir, catalog) = temp_catalog(fetcher);
    assert!(catalog.spec("missing").is_none());
}
