//! Model list from Devin CLI JSON (`devin models list --format json`).

use hofund_plugin_api::{ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::{Map, Value};

pub(crate) fn parse_models(body: &[u8]) -> Result<Vec<ModelSpec>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Devin model list is not JSON"))?;
    let families = root
        .get("families")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for family in families {
        let Some(object) = family.as_object() else {
            continue;
        };
        let uid = object
            .get("family_uid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if uid.eq_ignore_ascii_case("adaptive") || uid.eq_ignore_ascii_case("fusion") {
            continue;
        }
        let label = object
            .get("family_label")
            .and_then(Value::as_str)
            .unwrap_or(uid);
        if !uid.is_empty() {
            push_family(&mut out, uid, label, object)?;
        }
        if let Some(variants) = object.get("variants").and_then(Value::as_array) {
            for variant in variants {
                let Some(variant) = variant.as_object() else {
                    continue;
                };
                let id = variant
                    .get("model_uid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                let name = variant.get("label").and_then(Value::as_str).unwrap_or(id);
                let context = variant
                    .get("max_context_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = variant
                    .get("max_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(
                    ModelSpec::new(id, Some(name.to_owned()))?
                        .with_endpoints(vec!["/v1/chat/completions".to_owned()])
                        .with_context(context)
                        .with_max_output(output),
                );
            }
        }
    }
    Ok(out)
}

fn push_family(
    out: &mut Vec<ModelSpec>,
    uid: &str,
    label: &str,
    object: &Map<String, Value>,
) -> Result<(), PluginError> {
    let context = object
        .get("variants")
        .and_then(Value::as_array)
        .and_then(|variants| variants.first())
        .and_then(Value::as_object)
        .and_then(|variant| variant.get("max_context_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = object
        .get("variants")
        .and_then(Value::as_array)
        .and_then(|variants| variants.first())
        .and_then(Value::as_object)
        .and_then(|variant| variant.get("max_output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    out.push(
        ModelSpec::new(uid, Some(label.to_owned()))?
            .with_endpoints(vec!["/v1/chat/completions".to_owned()])
            .with_context(context)
            .with_max_output(output),
    );
    Ok(())
}

pub(crate) fn list_from_fixture(body: &[u8]) -> Result<ModelList, PluginError> {
    let models = parse_models(body)?;
    Ok(ModelList::new(models, None))
}
