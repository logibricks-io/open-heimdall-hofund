//! Model list from models.dev anthropic entries and Claude Code `.claude.json` caches.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;

use crate::catalog_cache::DevCatalogCache;

use crate::creds;

/// Reads `.claude.json` model caches from the account directory.
pub(crate) struct CodeModels {
    pub(crate) at: f64,
    pub(crate) extra: BTreeMap<String, String>,
    pub(crate) entitled: BTreeMap<String, bool>,
    pub(crate) prices: BTreeMap<String, (u64, u64, u64)>,
}

pub(crate) fn list_for_account(
    host: &HostContext,
    dir: &Path,
    _quota: &[QuotaWindow],
) -> Result<ModelList, PluginError> {
    let base = anthropic_catalog(host)?;
    let code = read_code_models(&dir.join(creds::PROFILE_FILE));
    let mut have = BTreeSet::new();
    let mut models = Vec::new();
    for spec in base {
        if let Some(entitled) = code.entitled.get(spec.id())
            && !*entitled
        {
            continue;
        }
        have.insert(spec.id().to_owned());
        models.push(spec);
    }
    for (id, label) in &code.extra {
        if have.contains(id) {
            continue;
        }
        if let Some(entitled) = code.entitled.get(id)
            && !*entitled
        {
            continue;
        }
        let mut spec = ModelSpec::new(id, label.clone().into())
            .unwrap_or_else(|_| ModelSpec::new(id, None).expect("id non-empty"));
        if let Some((input, output, cache)) = code.prices.get(id) {
            spec = spec.with_prices(*input, *output, *cache);
        }
        spec = spec.with_endpoints(vec!["/v1/messages".to_owned()]);
        have.insert(id.clone());
        models.push(spec);
    }
    Ok(ModelList::new(models, None))
}

fn anthropic_catalog(host: &HostContext) -> Result<Vec<ModelSpec>, PluginError> {
    let cache = DevCatalogCache::beside_data_root(&catalog_data_root(host)?);
    let Some(catalog) = cache
        .read_catalog()
        .map_err(|error| PluginError::io(error.to_string()))?
    else {
        return Ok(Vec::new());
    };
    let Some(provider) = catalog.get("anthropic") else {
        return Ok(Vec::new());
    };
    let Some(models) = provider.get("models").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (model_key, model) in models {
        let Some(object) = model.as_object() else {
            continue;
        };
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| model_key.clone());
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let display = (!name.is_empty()).then(|| name.to_owned());
        let mut spec = ModelSpec::new(id.as_str(), display)?;
        if let Some(limit) = object.get("limit").and_then(Value::as_object) {
            if let Some(context) = limit.get("context").and_then(Value::as_u64)
                && context > 0
            {
                spec = spec.with_context(context);
            }
            if let Some(output) = limit.get("output").and_then(Value::as_u64)
                && output > 0
            {
                spec = spec.with_max_output(output);
            }
        }
        if object.get("reasoning").and_then(Value::as_bool) == Some(true) {
            let efforts = object
                .get("reasoning_options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|o| {
                    o.get("values")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                })
                .collect();
            spec = spec.with_efforts(efforts);
        }
        if let Some(cost) = object.get("cost").and_then(Value::as_object) {
            spec = spec.with_prices(
                micros(cost.get("input").and_then(Value::as_f64).unwrap_or(0.0)),
                micros(cost.get("output").and_then(Value::as_f64).unwrap_or(0.0)),
                micros(
                    cost.get("cache_read")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                ),
            );
        }
        spec = spec.with_endpoints(vec!["/v1/messages".to_owned()]);
        out.push(spec);
    }
    Ok(out)
}

/// Locates the hofund data root from this plugin's state directory layout
/// (`<data_root>/state/<kind>/<plugin_id>`). There is no catalog handle on
/// [`HostContext`]; subscription plugins read the on-disk models.dev cache
/// the daemon maintains beside that root.
fn catalog_data_root(host: &HostContext) -> Result<PathBuf, PluginError> {
    let state = host.dirs().state_dir()?;
    state
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| PluginError::io("cannot locate the hofund data directory"))
}

fn micros(value: f64) -> u64 {
    if value <= 0.0 {
        0
    } else {
        (value * 1_000_000.0).round() as u64
    }
}

pub(crate) fn read_code_models(path: &Path) -> CodeModels {
    let mut best = CodeModels {
        at: 0.0,
        extra: BTreeMap::new(),
        entitled: BTreeMap::new(),
        prices: BTreeMap::new(),
    };
    if let Some(parsed) = parse_code_models_file(path)
        && parsed.at >= best.at
    {
        best = parsed;
    }
    best
}

fn parse_code_models_file(path: &Path) -> Option<CodeModels> {
    // The model cache is only read, never followed: a symlinked file is
    // ignored. Credentials keep their own reading rules.
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let text = crate::creds::read_capped(path)?;
    parse_code_models(&text)
}

pub(crate) fn parse_code_models(text: &str) -> Option<CodeModels> {
    let root: Value = hofund_plugin_sdk::json::from_str(text).ok()?;
    let object = root.as_object()?;
    let mut out = CodeModels {
        at: object
            .get("additionalModelOptionsAnsweredAt")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        extra: BTreeMap::new(),
        entitled: BTreeMap::new(),
        prices: BTreeMap::new(),
    };
    if let Some(options) = object
        .get("additionalModelOptionsCache")
        .and_then(Value::as_array)
    {
        for entry in options {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            if entry.get("disabled").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(id) = entry.get("value").and_then(Value::as_str).and_then(api_id) else {
                continue;
            };
            let label = entry
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or(id.clone());
            out.extra.entry(id).or_insert(label);
        }
    }
    if let Some(access) = object.get("modelAccessCache").and_then(Value::as_array) {
        for entry in access {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let Some(id) = entry
                .get("apiName")
                .and_then(Value::as_str)
                .and_then(api_id)
            else {
                continue;
            };
            if let Some(entitled) = entry.get("entitled").and_then(Value::as_bool) {
                out.entitled.insert(id.to_owned(), entitled);
                if entitled {
                    out.extra.entry(id.to_owned()).or_default();
                }
            }
        }
    }
    if let Some(costs) = object
        .get("additionalModelCostsCache")
        .and_then(Value::as_object)
    {
        for (raw_id, cost) in costs {
            let Some(id) = api_id(raw_id.as_str()) else {
                continue;
            };
            let Some(cost) = cost.as_object() else {
                continue;
            };
            let input = cost
                .get("inputTokens")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let output = cost
                .get("outputTokens")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if input <= 0.0 || output <= 0.0 {
                continue;
            }
            let cache_read = cost
                .get("promptCacheReadTokens")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            out.prices.insert(
                id.to_owned(),
                (micros(input), micros(output), micros(cache_read)),
            );
        }
    }
    Some(out)
}

fn api_id(value: &str) -> Option<String> {
    let mut value = value.trim().to_ascii_lowercase();
    if let Some(open) = value.find('[')
        && value.ends_with(']')
        && open > 0
    {
        value.truncate(open);
    }
    if !value.starts_with("claude-") || value.contains([' ', '/']) {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r5_7_code_models_entitlement_filters() {
        let text = r#"{
            "additionalModelOptionsAnsweredAt": 2,
            "additionalModelOptionsCache": [{"value":"claude-mythos-5-1","label":"Mythos"}],
            "modelAccessCache": [
                {"apiName":"claude-sonnet-4-5","entitled":true},
                {"apiName":"claude-opus-4-5","entitled":false}
            ]
        }"#;
        let parsed = parse_code_models(text).expect("parsed");
        assert!(parsed.entitled.get("claude-opus-4-5") == Some(&false));
        assert!(parsed.extra.contains_key("claude-mythos-5-1"));
    }

    #[test]
    fn r7_1_invalid_model_option_is_skipped_not_fatal() {
        let text = r#"{
            "additionalModelOptionsAnsweredAt": 2,
            "additionalModelOptionsCache": [
                {"value":null,"label":"Default"},
                {"value":"default","label":"Default"},
                {"value":"claude-mythos-5-1","label":"Mythos"}
            ],
            "modelAccessCache": [
                {"apiName":"claude-sonnet-4-5","entitled":true},
                {"apiName":"claude-opus-4-5","entitled":false}
            ]
        }"#;
        let parsed = parse_code_models(text).expect("parsed");
        assert!(parsed.extra.contains_key("claude-mythos-5-1"));
        assert!(parsed.extra.contains_key("claude-sonnet-4-5"));
        assert!(!parsed.extra.contains_key("default"));
        assert_eq!(parsed.entitled.get("claude-opus-4-5"), Some(&false));
    }

    #[test]
    fn r8_7_disabled_model_option_is_skipped() {
        let text = r#"{
            "additionalModelOptionsAnsweredAt": 2,
            "additionalModelOptionsCache": [
                {"value":"claude-mythos-5-1","label":"Mythos","disabled":true},
                {"value":"claude-haiku-5-1","label":"Haiku","disabled":false}
            ],
            "modelAccessCache": []
        }"#;
        let parsed = parse_code_models(text).expect("parsed");
        assert!(!parsed.extra.contains_key("claude-mythos-5-1"));
        assert!(parsed.extra.contains_key("claude-haiku-5-1"));
    }

    #[test]
    fn r8_8_micros_rounds_and_floors_at_zero() {
        assert_eq!(micros(3.0), 3_000_000);
        assert_eq!(micros(0.000_001_7), 2);
        assert_eq!(micros(0.000_001_2), 1);
        assert_eq!(micros(0.0), 0);
        assert_eq!(micros(-2.5), 0);
    }

    #[test]
    fn r5_8_api_id_strips_window_suffix() {
        assert_eq!(
            api_id("claude-mythos-5-1[1m]"),
            Some("claude-mythos-5-1".to_owned())
        );
        assert_eq!(api_id("default"), None);
    }

    #[test]
    fn r5_27_catalog_data_root_from_state_layout() {
        use hofund_kit_subscription::TestHost;
        use hofund_plugin_api::{PluginId, PluginKind};

        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let root = catalog_data_root(host.context()).expect("root");
        assert_eq!(root, dir.path());
        let cache = DevCatalogCache::beside_data_root(&root);
        assert_eq!(cache.path, dir.path().join("catalog/models.dev.json"));
    }

    #[test]
    fn r5_39_anthropic_models_from_cache_envelope() {
        use std::fs;

        use hofund_kit_subscription::TestHost;
        use hofund_plugin_api::{PluginId, PluginKind};

        let dir = tempfile::tempdir().expect("dir");
        fs::create_dir_all(dir.path().join("catalog")).expect("catalog dir");
        let envelope = r#"{"catalog":{"anthropic":{"id":"anthropic","models":{"claude-sonnet-4-5":{"name":"Sonnet 4.5","limit":{"context":200000}},"claude-haiku-map-key":{"name":"Haiku","limit":{"context":1000}}}}}}"#;
        fs::write(dir.path().join("catalog/models.dev.json"), envelope).expect("cache");
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let account = dir.path().join("account");
        fs::create_dir_all(&account).expect("account");
        fs::write(
            account.join(creds::PROFILE_FILE),
            r#"{"additionalModelOptionsCache":[{"value":"claude-mythos-5-1","label":"Mythos"}]}"#,
        )
        .expect("profile");
        let list = list_for_account(host.context(), &account, &[]).expect("list");
        let ids: Vec<_> = list.models().iter().map(|m| m.id().to_owned()).collect();
        assert!(ids.contains(&"claude-sonnet-4-5".to_owned()));
        assert!(ids.contains(&"claude-haiku-map-key".to_owned()));
        assert!(ids.contains(&"claude-mythos-5-1".to_owned()));
    }

    #[test]
    fn r6_4_symlink_profile_ignored_for_code_models() {
        use std::fs;

        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("real.json");
        fs::write(
            &target,
            r#"{"additionalModelOptionsCache":[{"value":"claude-mythos-5-1","label":"M"}]}"#,
        )
        .expect("target");
        let profile = dir.path().join(creds::PROFILE_FILE);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &profile).expect("symlink");
        #[cfg(not(unix))]
        return;

        assert!(parse_code_models_file(&profile).is_none());
        assert!(read_code_models(&profile).extra.is_empty());
    }

    #[test]
    fn r6_5_oversize_profile_ignored_for_code_models() {
        use std::fs;

        let dir = tempfile::tempdir().expect("dir");
        let big = format!(
            r#"{{"additionalModelOptionsCache":[{{"value":"{}"}}]}}"#,
            "x".repeat((1024 * 1024 + 64) as usize)
        );
        fs::write(dir.path().join(creds::PROFILE_FILE), big).expect("write");
        assert!(parse_code_models_file(&dir.path().join(creds::PROFILE_FILE)).is_none());
    }
}
