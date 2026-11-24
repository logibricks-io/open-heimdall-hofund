//! D-264 live-row decoration from fixture catalogs.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use hofund_config::CatalogSettings;
use hofund_daemon::ModelsHost;
use hofund_model_catalog::{CacheFile, CatalogFetcher, FetchResponse, ModelCatalog, parse_catalog};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Offline;

impl CatalogFetcher for Offline {
    fn fetch(&self, _url: &str, _if_none_match: Option<&str>) -> io::Result<FetchResponse> {
        Err(io::Error::other("offline"))
    }
}

#[tokio::test]
async fn d264_live_id_rows_use_ordered_catalog_metadata_and_prices() {
    let directory = tempfile::tempdir().expect("catalog directory");
    let parsed = parse_catalog(include_bytes!(
        "../../model-catalog/tests/fixtures/d264_membership_catalog.json"
    ))
    .expect("fixture");
    CacheFile::beside_data_root(directory.path())
        .write(None, &parsed)
        .expect("seed catalog");
    let catalog = ModelCatalog::open(
        directory.path(),
        CatalogSettings::default(),
        Arc::new(Offline),
    )
    .expect("open catalog");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "latest"}, {"id": "established"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let config = hofund_config::load(&format!(
        "[[providers]]\nid = \"plan\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\ncatalog = \"acme-coding-plan, acme\"\n",
        server.uri()
    ))
    .expect("config");
    let host =
        ModelsHost::new(Duration::from_secs(2), Duration::from_secs(2)).with_model_catalog(catalog);
    let config_path = directory.path().join("hofund.toml");
    let listed = host
        .list_models(&config, &config_path, "plan")
        .await
        .expect("decorated list");

    let latest = listed
        .models
        .iter()
        .find(|model| model.id() == "latest")
        .expect("latest");
    assert_eq!(latest.image_input(), Some(true));
    assert_eq!(latest.context(), 1_000_000);
    assert_eq!(latest.max_output(), 131_072);
    assert_eq!(latest.efforts(), ["low", "high", "max"]);
    assert_eq!(latest.prices(), (None, None, None));

    let established = listed
        .models
        .iter()
        .find(|model| model.id() == "established")
        .expect("established");
    assert_eq!(established.image_input(), Some(false));
    assert_eq!(
        established.prices(),
        (Some(1_195_160), Some(4_183_060), Some(298_790))
    );
    server.verify().await;
}
