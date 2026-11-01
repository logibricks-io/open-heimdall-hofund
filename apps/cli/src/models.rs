//! `hofund models` and `hofund models refresh`.

use std::path::Path;
use std::sync::Arc;

use hofund_config::load_path;
use hofund_model_catalog::{FetchResult, ModelCatalog, ReqwestFetcher};

fn data_root(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(
            || Path::new(".").join(".hofund"),
            |parent| parent.join(".hofund"),
        )
}

/// Lists route models with catalog spec and prices, or one model when `model` is set.
pub async fn list(config_path: &Path, model: Option<&str>) -> Result<String, String> {
    let settings =
        load_path(config_path).map_err(|error| format!("{}: {error}", config_path.display()))?;
    let fetcher =
        Arc::new(ReqwestFetcher::new().map_err(|error| format!("catalog client: {error}"))?);
    let root = data_root(config_path);
    let catalog = ModelCatalog::open(&root, settings.catalog, fetcher)
        .map_err(|error| format!("catalog: {error}"))?;
    catalog
        .reload()
        .map_err(|error| format!("catalog reload: {error}"))?;
    let routes: Vec<_> = settings
        .routes
        .iter()
        .map(|route| route.model.clone())
        .collect();
    let ids: Vec<String> = match model {
        Some(name) => vec![name.to_owned()],
        None => routes,
    };
    let mut lines = Vec::new();
    for id in ids {
        let Some(entry) = catalog.resolve(&id) else {
            lines.push(format!("{id}: (no catalog entry)"));
            continue;
        };
        let mut parts = vec![format!("{id}:")];
        if let Some(window) = entry.spec.context_window {
            parts.push(format!("context={window}"));
        }
        if let Some(output) = entry.spec.max_output {
            parts.push(format!("output={output}"));
        }
        if entry.spec.vision {
            parts.push("vision=yes".to_owned());
        }
        if !entry.spec.reasoning_levels.is_empty() {
            parts.push(format!(
                "reasoning={}",
                entry.spec.reasoning_levels.join(",")
            ));
        }
        if let Some(price) = &entry.price {
            parts.push(format!(
                "price input={} output={} cache_read={} cache_write={}",
                price.input, price.output, price.cache_read, price.cache_write
            ));
        }
        lines.push(parts.join(" "));
    }
    Ok(lines.join("\n"))
}

/// Refreshes the on-disk models.dev cache for this configuration.
pub async fn refresh(config_path: &Path) -> Result<String, String> {
    let settings =
        load_path(config_path).map_err(|error| format!("{}: {error}", config_path.display()))?;
    let fetcher =
        Arc::new(ReqwestFetcher::new().map_err(|error| format!("catalog client: {error}"))?);
    let root = data_root(config_path);
    let catalog = ModelCatalog::open(&root, settings.catalog, fetcher)
        .map_err(|error| format!("catalog: {error}"))?;
    let result = catalog
        .refresh()
        .map_err(|error| format!("catalog refresh: {error}"))?;
    let message = match result {
        FetchResult::Updated => "model catalog updated",
        FetchResult::NotModified => "model catalog unchanged (304)",
        FetchResult::OfflineFallback => "model catalog fetch failed; kept previous cache",
        FetchResult::Failed => "model catalog fetch failed with no cache",
    };
    Ok(format!("{message}: {}", catalog.cache_path().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn c1a_8_cli_models_prints_spec_and_prices_from_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("hofund.toml");
        std::fs::write(
            &config,
            "listen = \"127.0.0.1:9\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"p\"]\n[[providers]]\nid = \"p\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"k\"\n",
        )
        .expect("config");
        let data = dir.path().join(".hofund");
        std::fs::create_dir_all(data.join("catalog")).expect("mkdir");
        std::fs::write(
            data.join("catalog/models.dev.json"),
            r#"{"catalog":{"openai":{"models":{"demo":{"id":"demo","limit":{"context":32000,"output":2048},"cost":{"input":1.0,"output":2.0,"cache_read":0.1,"cache_write":0.2}}}}}}"#,
        )
        .expect("cache");
        let text = list(&config, None).await.expect("list");
        assert!(text.contains("context=32000"));
        assert!(text.contains("output=2048"));
        assert!(text.contains("price input=1 output=2"));
    }
}
