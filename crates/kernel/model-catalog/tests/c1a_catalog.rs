//! Integration tests for the models.dev catalog cache and spec lookup.

use std::sync::{Arc, Mutex};

use hofund_config::CatalogSettings;
use hofund_model_catalog::{
    CacheFile, CatalogFetcher, DevCatalog, FetchResponse, FetchResult, ModelCatalog, Syncer,
    build_index, is_stale, parse_catalog,
};
use std::time::{Duration, SystemTime};

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
    let settings = CatalogSettings::default();
    let index = build_index(&catalog, &settings);
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
    let index = build_index(&catalog, &settings.catalog);
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

const TOGGLE_FIXTURE: &str = r#"{
  "toggle-vendor": {
    "id": "toggle-vendor",
    "models": {
      "thinker": {
        "id": "thinker",
        "reasoning_options": [
          {"type": "toggle"},
          {"type": "effort", "values": ["low", "high"]}
        ],
        "limit": {"context": 8192, "output": 1024}
      }
    }
  }
}"#;

#[test]
fn c1a_9_reasoning_toggle_list_injects_none_when_provider_matches() {
    let catalog: DevCatalog = parse_catalog(TOGGLE_FIXTURE.as_bytes()).expect("parse");
    let without = CatalogSettings::default();
    let index = build_index(&catalog, &without);
    let entry = index.resolve("thinker").expect("entry");
    assert_eq!(entry.spec.reasoning_levels, ["low", "high"]);

    let with_toggle = CatalogSettings {
        reasoning_toggle_providers: vec!["toggle-vendor".to_owned()],
        ..CatalogSettings::default()
    };
    let index = build_index(&catalog, &with_toggle);
    let entry = index.resolve("thinker").expect("entry");
    assert_eq!(entry.spec.reasoning_levels, ["none", "low", "high"]);
}

const DUPLICATE_ID_FIXTURE: &str = r#"{
  "alpha": {
    "id": "alpha",
    "models": {
      "shared": {
        "id": "shared",
        "limit": {"context": 4096, "output": 100}
      }
    }
  },
  "beta": {
    "id": "beta",
    "models": {
      "shared": {
        "id": "shared",
        "limit": {"context": 8192, "output": 200}
      }
    }
  }
}"#;

#[test]
fn c1a_10_bare_id_index_is_first_provider_and_override_wins() {
    let catalog: DevCatalog = parse_catalog(DUPLICATE_ID_FIXTURE.as_bytes()).expect("parse");
    let settings = CatalogSettings::default();
    let index = build_index(&catalog, &settings);
    let entry = index.resolve("shared").expect("entry");
    assert_eq!(entry.provider_id, "alpha");
    assert_eq!(entry.spec.context_window, Some(4096));

    let settings = hofund_config::load(
        r#"
listen = "127.0.0.1:9"
[[catalog.overrides]]
model = "shared"
context = 42
"#,
    )
    .expect("config");
    let index = build_index(&catalog, &settings.catalog);
    let entry = index.resolve("shared").expect("entry");
    assert_eq!(entry.spec.context_window, Some(42));
}

#[tokio::test]
async fn c1a_11_refresh_off_means_no_background_poll() {
    let fetcher = Arc::new(MockFetcher::new(vec![]));
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = CatalogSettings {
        refresh: Duration::MAX,
        poll: Duration::MAX,
        ..CatalogSettings::default()
    };
    let catalog = ModelCatalog::open(dir.path(), settings, fetcher).expect("open");
    assert!(!catalog.stale());
    let handle = catalog.spawn_background();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("background task exits promptly when polling is off")
        .expect("background task exits");
}

#[test]
fn c1a_12_corrupt_cache_falls_back_to_fetch_without_panicking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_path = dir.path().join("catalog/models.dev.json");
    std::fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&cache_path, b"{not-json").expect("corrupt");
    let body = FIXTURE.as_bytes().to_vec();
    let fetcher = Arc::new(MockFetcher::new(vec![Ok(FetchResponse::Ok {
        body,
        etag: Some("fresh".to_owned()),
    })]));
    let settings = CatalogSettings::default();
    let catalog = ModelCatalog::open(dir.path(), settings, fetcher).expect("open");
    assert!(catalog.resolve("gpt-demo").is_none());
    assert_eq!(catalog.refresh().expect("refresh"), FetchResult::Updated);
    catalog.reload().expect("reload");
    assert!(catalog.resolve("gpt-demo").is_some());
}

#[derive(Default)]
struct EtagRecorder {
    sent: Mutex<Option<Option<String>>>,
}

impl CatalogFetcher for EtagRecorder {
    fn fetch(&self, _: &str, if_none_match: Option<&str>) -> Result<FetchResponse, std::io::Error> {
        *self.sent.lock().unwrap() = Some(if_none_match.map(str::to_owned));
        Ok(FetchResponse::NotModified)
    }
}

#[test]
fn c1a_13_cached_etag_is_sent_as_if_none_match() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = CacheFile::beside_data_root(dir.path());
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    cache.write(Some("v7"), &catalog).expect("seed");
    let fetcher = Arc::new(EtagRecorder::default());
    let syncer = Syncer::new(cache, fetcher.clone());
    syncer.sync("http://example.test/api.json").expect("sync");
    assert_eq!(*fetcher.sent.lock().unwrap(), Some(Some("v7".to_owned())));
}

#[test]
fn c1a_14_staleness_compares_age_against_refresh_interval() {
    let hour = Duration::from_secs(3600);
    assert!(is_stale(None, hour));
    assert!(!is_stale(Some(SystemTime::now()), hour));
    let old = SystemTime::now() - Duration::from_secs(7200);
    assert!(is_stale(Some(old), hour));
}

#[test]
fn c1a_15_not_modified_marks_cache_fresh_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = CacheFile::beside_data_root(dir.path());
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    cache.write(Some("v1"), &catalog).expect("seed");
    let old = SystemTime::now() - Duration::from_secs(7200);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&cache.path)
        .expect("open")
        .set_modified(old)
        .expect("age cache");
    assert!(is_stale(cache.modified(), Duration::from_secs(3600)));
    let fetcher = Arc::new(MockFetcher::new(vec![Ok(FetchResponse::NotModified)]));
    let syncer = Syncer::new(cache.clone(), fetcher);
    assert_eq!(
        syncer.sync("http://example.test/api.json").expect("sync"),
        FetchResult::NotModified
    );
    assert!(!is_stale(cache.modified(), Duration::from_secs(3600)));
}

#[test]
fn c1a_16_refresh_reloads_indexes_without_explicit_reload() {
    let fetcher = Arc::new(MockFetcher::new(vec![Ok(FetchResponse::Ok {
        body: FIXTURE.as_bytes().to_vec(),
        etag: None,
    })]));
    let (_dir, catalog) = temp_catalog(fetcher);
    assert!(catalog.resolve("gpt-demo").is_none());
    assert_eq!(catalog.refresh().expect("refresh"), FetchResult::Updated);
    assert!(catalog.resolve("gpt-demo").is_some());
}

#[test]
fn c1a_17_provider_prefixed_lookup_picks_that_provider_then_falls_back_to_bare_id() {
    let catalog: DevCatalog = parse_catalog(DUPLICATE_ID_FIXTURE.as_bytes()).expect("parse");
    let index = build_index(&catalog, &CatalogSettings::default());
    let beta = index.resolve("beta/shared").expect("beta entry");
    assert_eq!(beta.provider_id, "beta");
    assert_eq!(beta.spec.context_window, Some(8192));
    let routed = index.resolve("gateway/shared").expect("bare fallback");
    assert_eq!(routed.provider_id, "alpha");
}

fn window_of(limit: &str) -> Option<u32> {
    let body =
        format!(r#"{{"p": {{"id": "p", "models": {{"m": {{"id": "m", "limit": {limit}}}}}}}}}"#);
    let catalog: DevCatalog = parse_catalog(body.as_bytes()).expect("parse");
    let index = build_index(&catalog, &CatalogSettings::default());
    index.resolve("m").expect("entry").spec.context_window
}

#[test]
fn c1a_18_window_takes_smaller_of_input_and_context_and_ignores_tiny_values() {
    assert_eq!(
        window_of(r#"{"context": 50000, "input": 100000}"#),
        Some(50000)
    );
    assert_eq!(
        window_of(r#"{"context": 128000, "input": 64000}"#),
        Some(64000)
    );
    assert_eq!(window_of(r#"{"context": 1000}"#), None);
    assert_eq!(window_of(r#"{"context": 2048}"#), Some(2048));
}

const FAMILY_FIXTURE: &str = r#"{
  "fam": {
    "id": "fam",
    "models": {
      "base": {
        "id": "base",
        "family": "line",
        "reasoning": true,
        "release_date": "2025-01-01",
        "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
        "limit": {"context": 8192, "output": 1024}
      },
      "next": {
        "id": "next",
        "family": "line",
        "reasoning": true,
        "release_date": "2025-06-01",
        "limit": {"context": 8192, "output": 1024}
      }
    }
  }
}"#;

#[test]
fn c1a_19_reasoning_model_without_levels_inherits_family_levels() {
    let catalog: DevCatalog = parse_catalog(FAMILY_FIXTURE.as_bytes()).expect("parse");
    let index = build_index(&catalog, &CatalogSettings::default());
    assert_eq!(
        index.resolve("next").expect("next").spec.reasoning_levels,
        ["low", "high"]
    );
}

#[test]
fn c1a_20_price_override_replaces_catalog_price() {
    let catalog: DevCatalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    let settings = hofund_config::load(
        r#"
listen = "127.0.0.1:9"
[[catalog.overrides]]
model = "gpt-demo"
price = { input = 1.5, output = 6.0, cache_read = 0.1, cache_write = 2.0 }
"#,
    )
    .expect("config");
    let index = build_index(&catalog, &settings.catalog);
    let price = index
        .resolve("gpt-demo")
        .expect("entry")
        .price
        .clone()
        .expect("price");
    assert!((price.input - 1.5).abs() < 1e-9);
    assert!((price.output - 6.0).abs() < 1e-9);
}
