//! Z Code provider-config discovery and per-model capabilities.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{HostContext, ModelSpec, PluginError};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::{Call, url};

use crate::session::Stored;
use crate::{APP_VERSION, Endpoints, api, start};

pub(crate) struct Loaded {
    pub config: Value,
    pub source_url: Option<String>,
}

pub(crate) async fn load(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Option<Loaded> {
    let device_file = crate::device::device_path(host).ok()?;
    let remote = remote(host, endpoints, &device_file).await.ok();
    let local = local(stored);
    match (remote, local) {
        (Some(remote), Some(local)) => {
            if revision(&local) > revision(&remote.config) {
                Some(Loaded {
                    config: local,
                    source_url: None,
                })
            } else {
                Some(remote)
            }
        }
        (Some(remote), None) => Some(remote),
        (None, Some(local)) => Some(Loaded {
            config: local,
            source_url: None,
        }),
        (None, None) => None,
    }
}

async fn remote(
    host: &HostContext,
    endpoints: &Endpoints,
    device_file: &Path,
) -> Result<Loaded, PluginError> {
    let url = format!(
        "{}/api/v1/client/configs?app_version={APP_VERSION}&platform={}",
        endpoints.portal.trim_end_matches('/'),
        start::platform()
    );
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        None,
        &[],
        device_file,
    ))
    .await?;
    let source = data
        .get("configs")
        .and_then(|configs| configs.get("builtin_provider_config_json"))
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .ok_or_else(|| PluginError::decode("Z Code configs name no provider config"))?
        .to_owned();
    let parsed = url::Url::parse(&source)
        .map_err(|_| PluginError::decode("Z Code provider config URL is invalid"))?;
    if parsed.scheme() != "https" {
        return Err(PluginError::decode(
            "Z Code provider config URL is not HTTPS",
        ));
    }
    let reply = Call::get(source.clone())
        .header("user-agent", format!("ZCode/{APP_VERSION}"))
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Z Code provider config could not be read",
        ));
    }
    let config = json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Z Code provider config is not JSON"))?;
    Ok(Loaded {
        config,
        source_url: Some(source),
    })
}

fn local(stored: &Stored) -> Option<Value> {
    let mut files = Vec::new();
    if let Some(credentials) = &stored.auth.agent_credentials
        && let Some(v2) = credentials.parent()
    {
        collect_configs(&v2.join("runtime").join("provider"), 0, &mut files);
    }
    #[cfg(target_os = "macos")]
    files.push(PathBuf::from(
        "/Applications/ZCode.app/Contents/Resources/config/provider/zcode-builtin.json",
    ));
    let mut best = None;
    for file in files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let Ok(config) = json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|current| revision(&config) > revision(current))
        {
            best = Some(config);
        }
    }
    best
}

fn collect_configs(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if depth < 3 && kind.is_dir() {
            collect_configs(&path, depth + 1, files);
        } else if depth == 3
            && kind.is_file()
            && path
                .file_name()
                .is_some_and(|name| name == "zcode-builtin.json")
        {
            files.push(path);
        }
    }
}

fn revision(config: &Value) -> u64 {
    config.get("revision").and_then(Value::as_u64).unwrap_or(0)
}

pub(crate) fn models(config: &Value, plan: &str) -> Result<Vec<ModelSpec>, PluginError> {
    let Some(config) = config.get("config") else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for rule in config
        .get("providerConfigRules")
        .and_then(|rules| rules.get("providerRules"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|rule| text(rule, "providerId") == plan)
    {
        for id in rule
            .get("builtinModelIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            push_id(&mut ids, id);
        }
    }
    let mut off = Vec::new();
    for rule in config
        .get("modelConfigRules")
        .and_then(|rules| rules.get("builtinProviderModelRules"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|rule| text(rule, "providerId") == plan)
    {
        let id = text(rule, "modelId");
        if rule
            .get("config")
            .and_then(|config| config.get("enabled"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            off.push(id.to_ascii_lowercase());
        } else {
            push_id(&mut ids, id);
        }
    }
    let mut out = Vec::new();
    for id in ids {
        if off.contains(&id.to_ascii_lowercase()) {
            continue;
        }
        let mut model =
            ModelSpec::new(&id, None)?.with_endpoints(vec!["anthropic-messages".to_owned()]);
        for rule in config
            .get("modelConfigRules")
            .and_then(|rules| rules.get("modelRules"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let pattern = text(rule, "modelMatch");
            let Ok(regex) = regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
                .case_insensitive(true)
                .build()
            else {
                continue;
            };
            if !regex.is_match(&id) {
                continue;
            }
            let rule_config = rule.get("config").cloned().unwrap_or(Value::Null);
            if let Some(context) = rule_config
                .get("properties")
                .and_then(|properties| properties.get("contextWindow"))
                .and_then(Value::as_u64)
                .filter(|context| *context > 0)
            {
                model = model.with_context(context);
            }
            if let Some(output) = rule_config
                .get("optionSpecs")
                .and_then(|specs| specs.get("maxOutputTokens"))
                .and_then(|spec| spec.get("max"))
                .and_then(Value::as_u64)
                .filter(|output| *output > 0)
            {
                model = model.with_max_output(output);
            }
            if let Some(image) = rule_config
                .get("properties")
                .and_then(|properties| properties.get("inputFormat"))
                .and_then(|format| format.get("supportsImage"))
                .and_then(Value::as_bool)
            {
                model = model
                    .with_image_input(Some(image))
                    .with_input_modalities(if image {
                        vec!["text".to_owned(), "image".to_owned()]
                    } else {
                        vec!["text".to_owned()]
                    })
                    .with_output_modalities(vec!["text".to_owned()]);
            }
            if let Some(efforts) = rule_config
                .get("optionSpecs")
                .and_then(|specs| specs.get("reasoningLevel"))
                .and_then(|spec| spec.get("values"))
                .and_then(Value::as_array)
            {
                let mut mapped = Vec::new();
                for effort in efforts.iter().filter_map(Value::as_str) {
                    let effort = match effort {
                        "disabled" => "none",
                        "enabled" => "high",
                        other => other,
                    };
                    if !mapped.iter().any(|existing| existing == effort) {
                        mapped.push(effort.to_owned());
                    }
                }
                model = model.with_efforts(mapped);
            }
        }
        out.push(model);
    }
    Ok(out)
}

pub(crate) fn about(config: &Value, id: &str) -> Option<ModelSpec> {
    [
        "account:zai-start-plan",
        "account:zai-individual-coding-plan",
        "account:bigmodel-individual-coding-plan",
    ]
    .into_iter()
    .flat_map(|plan| models(config, plan).unwrap_or_default())
    .find(|model| model.id().eq_ignore_ascii_case(id))
}

fn push_id(ids: &mut Vec<String>, id: &str) {
    if !ids.iter().any(|existing| existing.eq_ignore_ascii_case(id)) {
        ids.push(id.to_owned());
    }
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
