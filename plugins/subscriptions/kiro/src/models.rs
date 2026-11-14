//! Live model lists and their native reasoning request schemas.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::creds::{self, Stored};
use crate::{Endpoints, refresh};

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeEffort {
    pub(crate) path: &'static str,
    pub(crate) levels: Vec<String>,
}

#[derive(Default)]
pub(crate) struct NativeModels {
    by_account_model: Mutex<HashMap<(String, String), Option<NativeEffort>>>,
}

impl NativeModels {
    pub(crate) fn native(&self, account: &AccountId, model: &str) -> Option<NativeEffort> {
        self.by_account_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(account.as_str().to_owned(), model.to_owned()))
            .cloned()
            .flatten()
    }

    fn learn(&self, account: &AccountId, models: &[Value]) {
        let mut learned = self
            .by_account_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for model in models {
            let Some(id) = model.get("modelId").and_then(Value::as_str) else {
                continue;
            };
            learned.insert(
                (account.as_str().to_owned(), id.to_owned()),
                native_of(model.get("additionalModelRequestFieldsSchema")),
            );
        }
    }
}

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
    cli_command: Option<&Path>,
    natives: &NativeModels,
) -> Result<ModelList, PluginError> {
    let stored = refresh::ensure_fresh(host, account, Arc::clone(&endpoints), cli_command).await?;
    let root = if stored.credential.method == "apikey" {
        fetch_key_pages(host, &endpoints, &stored).await?
    } else {
        fetch_management(host, &endpoints, &stored).await?
    };
    let list = parse_list(&root)?;
    if let Some(models) = root.get("models").and_then(Value::as_array) {
        natives.learn(account, models);
    }
    Ok(list)
}

async fn fetch_management(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<Value, PluginError> {
    let profile = creds::effective_profile_arn(&stored.credential);
    let region = creds::kiro_region(profile, &stored.credential.region);
    let base = endpoints.management.replace("{region}", &region);
    let url = format!(
        "{base}/List-Available-Models?origin=KIRO_CLI&profileArn={}",
        creds::percent_encode_query(profile)
    );
    let reply = auth_call(Call::get(url), stored)
        .header("accept", "application/json")
        .fetch(host.http())
        .await?;
    decode_reply(reply.status(), reply.body())
}

async fn fetch_key_pages(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<Value, PluginError> {
    let region = creds::kiro_region(
        creds::effective_profile_arn(&stored.credential),
        &stored.credential.region,
    );
    let base = endpoints.management.replace("{region}", &region);
    let mut models = Vec::new();
    let mut default_model = None;
    let mut next: Option<String> = None;
    for _ in 0..20 {
        let mut body = Map::new();
        body.insert("origin".to_owned(), Value::String("KIRO_CLI".to_owned()));
        if let Some(token) = &next {
            body.insert("nextToken".to_owned(), Value::String(token.clone()));
        }
        let reply = auth_call(Call::post(format!("{base}/?origin=KIRO_CLI")), stored)
            .header("content-type", "application/x-amz-json-1.0")
            .header(
                "x-amz-target",
                "KiroControlPlaneBearerService.ListAvailableModels",
            )
            .header("x-amzn-codewhisperer-optout", "false")
            .bytes(json::to_vec(&Value::Object(body)).expect("JSON object"))
            .fetch(host.http())
            .await?;
        let page = decode_reply(reply.status(), reply.body())?;
        if let Some(items) = page.get("models").and_then(Value::as_array) {
            models.extend(items.iter().cloned());
        }
        if default_model.is_none() {
            default_model = page.get("defaultModel").cloned();
        }
        next = page
            .get("nextToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    let mut root = Map::new();
    root.insert("models".to_owned(), Value::Array(models));
    if let Some(default_model) = default_model {
        root.insert("defaultModel".to_owned(), default_model);
    }
    Ok(Value::Object(root))
}

fn auth_call(call: Call, stored: &Stored) -> Call {
    let call = call.sensitive_header(
        "authorization",
        format!("Bearer {}", secret_text(&stored.credential.access)),
    );
    match stored.credential.method.as_str() {
        "apikey" => call.header("tokentype", "API_KEY"),
        "external-idp" => call.header("tokentype", "EXTERNAL_IDP"),
        _ => call,
    }
}

fn decode_reply(status: u16, body: &[u8]) -> Result<Value, PluginError> {
    match status {
        200..=299 => {
            json::from_slice(body).map_err(|_| PluginError::decode("Kiro models JSON is invalid"))
        }
        401 | 403 => Err(PluginError::auth(
            "Kiro refused the sign-in while listing models",
        )),
        status => Err(PluginError::status_error(
            status,
            "Kiro did not return a model list",
        )),
    }
}

fn parse_list(root: &Value) -> Result<ModelList, PluginError> {
    let items = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("Kiro models JSON has no models array"))?;
    let default = root
        .get("defaultModel")
        .and_then(|model| model.get("modelId"))
        .and_then(Value::as_str);
    let mut models = Vec::new();
    for item in items {
        let Some(id) = item.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let listed_name = item.get("modelName").and_then(Value::as_str);
        let name = match listed_name {
            Some(name) if !name.is_empty() && name != id => name,
            _ if id == "auto" => "Auto",
            _ => id,
        };
        let context = item
            .get("tokenLimits")
            .and_then(|limits| limits.get("maxInputTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = item
            .get("tokenLimits")
            .and_then(|limits| limits.get("maxOutputTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let images = item
            .get("supportedInputTypes")
            .and_then(Value::as_array)
            .is_some_and(|types| {
                types
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|kind| kind.eq_ignore_ascii_case("IMAGE"))
            });
        let mut spec = ModelSpec::new(id, Some(name.to_owned()))?
            .with_endpoints(vec!["/v1/messages".to_owned()])
            .with_context(context)
            .with_max_output(output)
            .with_input_modalities(if images {
                vec!["text".to_owned(), "image".to_owned()]
            } else {
                vec!["text".to_owned()]
            })
            .with_output_modalities(vec!["text".to_owned()])
            .with_image_input(Some(images));
        if thinks(id) {
            spec = spec.with_efforts(EFFORTS.iter().map(|effort| (*effort).to_owned()).collect());
        }
        models.push(spec);
    }
    if models.is_empty() {
        return Err(PluginError::decode("Kiro listed no models"));
    }
    if let Some(index) = models.iter().position(|model| Some(model.id()) == default) {
        let model = models.remove(index);
        models.insert(0, model);
    }
    Ok(ModelList::new(models, None))
}

fn native_of(schema: Option<&Value>) -> Option<NativeEffort> {
    let properties = schema?.as_object()?.get("properties")?.as_object()?;
    let adaptive = properties
        .get("thinking")?
        .get("properties")
        .and_then(|properties| properties.get("type"))
        .and_then(|kind| kind.get("enum"))
        .and_then(Value::as_array)?
        .iter()
        .any(|kind| kind.as_str() == Some("adaptive"));
    if !adaptive {
        return None;
    }
    for path in ["output_config", "reasoning"] {
        let Some(levels) = properties
            .get(path)
            .and_then(|config| config.get("properties"))
            .and_then(|properties| properties.get("effort"))
            .and_then(|effort| effort.get("enum"))
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
        else {
            continue;
        };
        if !levels.is_empty() {
            return Some(NativeEffort { path, levels });
        }
    }
    None
}

fn thinks(id: &str) -> bool {
    id == "auto" || id.to_ascii_lowercase().contains("claude")
}

#[cfg(test)]
pub(crate) fn catalog() -> Result<ModelList, PluginError> {
    let models = vec![
        ModelSpec::new("claude-sonnet-4.5", Some("Claude Sonnet 4.5".to_owned()))?
            .with_endpoints(vec!["/v1/messages".to_owned()]),
        ModelSpec::new("claude-sonnet-4.6", Some("Claude Sonnet 4.6".to_owned()))?
            .with_endpoints(vec!["/v1/messages".to_owned()]),
    ];
    Ok(ModelList::new(models, None))
}

#[cfg(test)]
mod tests {
    use super::{native_of, parse_list};
    use hofund_plugin_sdk::json::json;

    #[test]
    fn native_schema_requires_adaptive_and_a_listed_effort() {
        let schema = json!({"properties": {
            "thinking": {"properties": {"type": {"enum": ["adaptive", "disabled"]}}},
            "output_config": {"properties": {"effort": {"enum": ["low", "xhigh", "max"]}}}
        }});
        let native = native_of(Some(&schema)).expect("native");
        assert_eq!(native.path, "output_config");
        assert_eq!(native.levels, ["low", "xhigh", "max"]);
        let reasoning = json!({"properties": {
            "thinking": schema["properties"]["thinking"].clone(),
            "reasoning": schema["properties"]["output_config"].clone()
        }});
        assert_eq!(
            native_of(Some(&reasoning)).expect("native").path,
            "reasoning"
        );
        assert!(native_of(Some(&json!({"properties": {"output_config": schema["properties"]["output_config"].clone()}}))).is_none());
        assert!(native_of(None).is_none());
    }

    #[test]
    fn live_list_keeps_limits_images_efforts_and_default_order() {
        let list = parse_list(&json!({
            "defaultModel": {"modelId": "claude-opus-5.5"},
            "models": [
                {"modelId": "deepseek-3.2", "modelName": "DeepSeek"},
                {"modelId": "claude-opus-5.5", "modelName": "Claude Opus", "tokenLimits": {"maxInputTokens": 200000, "maxOutputTokens": 64000}, "supportedInputTypes": ["TEXT", "IMAGE"]}
            ]
        }))
        .expect("list");
        assert_eq!(list.models()[0].id(), "claude-opus-5.5");
        assert_eq!(list.models()[0].context(), 200000);
        assert_eq!(list.models()[0].max_output(), 64000);
        assert_eq!(list.models()[0].image_input(), Some(true));
        assert_eq!(
            list.models()[0].efforts(),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(list.models()[1].efforts().is_empty());
    }
}
