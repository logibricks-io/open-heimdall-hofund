//! The other gateway's live model list.
//!
//! One URL, `<root>/v1/models`, with the gateway key as a bearer token.
//! Pages follow `after_id`. Each row is read as the kernel's
//! OpenAI-compatible list read it, keeping the protocols the model is served
//! on (`native_endpoints`) and the API the remote asks that model on best.
//!
//! Hofund's own field is always read. A different field name, and an extra
//! list header, come from provider configuration. With neither set, no extra
//! header is sent. Image, video, decision, and price fields that wait on a
//! naming ruling are not read.
//!
//! After a non-empty list, one `GET <root>/v1/codex/models` asks for Codex's
//! own catalog rows. A failure there keeps the model list and adds no entry.

use std::collections::HashMap;

use hofund_plugin_api::HttpHandle;
use hofund_plugin_api::{
    ListKind, ModelList, ModelSpec, PREFERRED_API_FIELD, PluginError, PluginErrorKind,
    SecretString, redact_token, safe_url,
};
use hofund_plugin_sdk::classify::classify_model;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::status_hint;
use hofund_plugin_sdk::transport::parse_retry_after_headers;
use hofund_plugin_sdk::{BODY_CAP, Call, PageError, PageErrorKind, after_id, json_error_message};

/// One provider's list inputs.
pub(crate) struct Lister<'a> {
    pub(crate) http: &'a HttpHandle,
    pub(crate) url: String,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) secret: Option<&'a SecretString>,
    /// Extra JSON key read when [`PREFERRED_API_FIELD`] is absent.
    pub(crate) preferred_field: Option<&'a str>,
    /// Extra list header, name then value, sent only when both are set.
    pub(crate) ask: Option<(&'a str, &'a str)>,
}

/// Codex's `client_version` on the remote catalog request.
///
/// The same floor the Codex subscription speaks. This plugin does not depend
/// on that crate.
const CODEX_CLIENT_VERSION: &str = "0.169.1";

/// A list failure carried through [`after_id`].
struct ListFailure(PluginError);

impl From<PageError> for ListFailure {
    fn from(error: PageError) -> Self {
        let message = error.to_string();
        if error.kind() == PageErrorKind::Budget {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

impl Lister<'_> {
    pub(crate) async fn list(&self) -> Result<ModelList, PluginError> {
        let rows = after_id(&self.url, |url| async move {
            self.page(&url).await.map_err(ListFailure)
        })
        .await
        .map_err(|failure| rebuild(&failure.0, self.redact(failure.0.message())))?;
        let (models, dropped) = model_specs(&rows, self.preferred_field)?;
        if models.is_empty() {
            return Err(PluginError::decode(format!(
                "{}: no models listed",
                safe_url(&self.url)
            )));
        }
        Ok(ModelList::new(models, None)
            .with_dropped(dropped)
            .with_kind(ListKind::Live)
            .with_source_url(&self.url))
    }

    /// One list page, with the key as a bearer token unless a configured
    /// header names `authorization`.
    async fn page(&self, url: &str) -> Result<Vec<u8>, PluginError> {
        let configured = self
            .headers
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case("authorization"));
        let mut call = Call::get(url).header("accept", "application/json");
        if let Some((name, value)) = self.ask
            && !name.is_empty()
            && !self
                .headers
                .iter()
                .any(|(header, _)| header.eq_ignore_ascii_case(name))
        {
            call = call.sensitive_header(name.to_ascii_lowercase(), value);
        }
        call = self.authed(call, configured);
        let mut response = call.open_raw(self.http, 1).await?;
        let status = response.status().as_u16();
        let retry_after =
            parse_retry_after_headers(response.headers(), std::time::SystemTime::now());
        let ok = (200..300).contains(&status);
        let mut body = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) if !ok => break,
                Err(_) => {
                    return Err(PluginError::interrupted(
                        "could not read the upstream response",
                    ));
                }
            };
            body.extend_from_slice(&chunk);
            if body.len() > BODY_CAP {
                return Err(PluginError::decode("response body is too large"));
            }
        }
        // reference: internal/catalog/live.go:542
        if status == 200 {
            return Ok(body);
        }
        let detail = json_error_message(&body);
        let mut message = format!("{}: HTTP {status}", safe_url(url));
        if !detail.is_empty() {
            message.push_str(&format!(" ({detail})"));
        }
        message.push_str(&status_hint(&body));
        // `list` redacts every failure, this one included.
        let mut error = PluginError::status_error(status, message);
        if let Some(after) = retry_after {
            error = error.with_retry_after(after);
        }
        Err(error)
    }

    /// Codex catalog rows from `<root>/v1/codex/models`, keyed by model id and slug.
    ///
    /// `None` is a failed request or a body that is not JSON. `Some` is a
    /// parsed catalog, including an empty one, so a failure can keep entries
    /// already remembered and a successful catalog can replace them.
    pub(crate) async fn codex_catalog(&self) -> Option<HashMap<String, String>> {
        let url = codex_catalog_url(&self.url);
        let configured = self
            .headers
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case("authorization"));
        let call = self.authed(
            Call::get(&url).header("accept", "application/json"),
            configured,
        );
        let Ok(mut response) = call.open_raw(self.http, 1).await else {
            return None;
        };
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await {
            let Ok(chunk) = chunk else {
                return None;
            };
            body.extend_from_slice(&chunk);
            if body.len() > BODY_CAP {
                return None;
            }
        }
        if status != 200 {
            return None;
        }
        let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(&body) else {
            return None;
        };
        Some(codex_entries(&value))
    }

    fn authed(&self, mut call: Call, authorization_configured: bool) -> Call {
        if let Some(secret) = self.secret.filter(|secret| !secret_text(secret).is_empty())
            && !authorization_configured
        {
            call =
                call.sensitive_header("authorization", format!("Bearer {}", secret_text(secret)));
        }
        for (name, value) in self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
        call
    }

    fn redact(&self, text: &str) -> String {
        let mut clean = text.to_owned();
        for secret in self
            .secret
            .map(secret_text)
            .into_iter()
            .chain(self.headers.iter().map(|(_, value)| value.as_str()))
            .filter(|value| !value.is_empty())
        {
            clean = redact_token(&clean, secret);
        }
        clean
    }
}

/// `error` with `message`, keeping its kind, status, and wait.
fn rebuild(error: &PluginError, message: String) -> PluginError {
    let mut clean = match error.kind() {
        PluginErrorKind::Connect => PluginError::connect(message),
        PluginErrorKind::Timeout => PluginError::timeout(message),
        PluginErrorKind::Transport => PluginError::transport(message),
        PluginErrorKind::Interrupted => PluginError::interrupted(message),
        PluginErrorKind::Status => {
            PluginError::status_error(error.status().unwrap_or(502), message)
        }
        PluginErrorKind::Unsupported => PluginError::unsupported(message),
        PluginErrorKind::Auth => PluginError::auth(message),
        PluginErrorKind::Quota => PluginError::quota(message),
        PluginErrorKind::Config => PluginError::config(message),
        PluginErrorKind::Cancelled => PluginError::cancelled(message),
        PluginErrorKind::Io => PluginError::io(message),
        PluginErrorKind::Internal => PluginError::internal(message),
        _ => PluginError::decode(message),
    };
    if let Some(after) = error.retry_after() {
        clean = clean.with_retry_after(after);
    }
    clean
}

fn model_specs(
    rows: &[Value],
    extra_field: Option<&str>,
) -> Result<(Vec<ModelSpec>, Vec<String>), PluginError> {
    let mut models = Vec::new();
    let mut dropped = Vec::new();
    for row in rows {
        let id = row.as_str().or_else(|| {
            row.get("id")
                .and_then(Value::as_str)
                .or_else(|| row.get("name").and_then(Value::as_str))
        });
        if let Some(model) = model_spec(row, extra_field)? {
            models.push(model);
        } else if let Some(id) = id.filter(|id| !id.is_empty()) {
            dropped.push(id.to_owned());
        }
    }
    Ok((models, dropped))
}

/// One OpenAI-compatible model row, as the kernel's live list read it.
fn model_spec(row: &Value, extra_field: Option<&str>) -> Result<Option<ModelSpec>, PluginError> {
    if let Value::String(id) = row {
        if classify_model(id, "").is_none() {
            return Ok(None);
        }
        return ModelSpec::new(id, Some(id.to_owned())).map(Some);
    }
    let Some(row) = row.as_object() else {
        return Ok(None);
    };
    let Some(id) = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| row.get("name").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
    let Some(tags) = classify_model(id, kind) else {
        return Ok(None);
    };
    // reference: internal/catalog/live.go:589-603
    let name = row
        .get("display_name")
        .and_then(Value::as_str)
        .or_else(|| {
            row.get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.contains(": "))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_owned();
    let mut model = ModelSpec::new(id, Some(name))?
        .with_draws(tags.draws())
        .with_films(tags.films())
        .with_decides(tags.decides())
        .with_context(positive(row.get("context_length")))
        .with_max_output(positive(row.get("max_output_tokens")))
        .with_efforts(named_values(
            row.get("supported_reasoning_levels"),
            "effort",
        ))
        .with_service_tiers(named_values(row.get("service_tiers"), "id"));
    if let Some(search) = row.get("web_search").and_then(Value::as_str) {
        model = model.with_web_search(search);
    }
    if let Some(modalities) = row.get("modalities").and_then(Value::as_object) {
        let input = string_array(modalities.get("input"));
        let output = string_array(modalities.get("output"));
        if modalities.get("input").and_then(Value::as_array).is_some() {
            model = model.with_image_input(Some(input.iter().any(|item| item == "image")));
        }
        model = model
            .with_input_modalities(input)
            .with_output_modalities(output);
    }
    let native = string_array(row.get("native_endpoints"));
    let supported = string_array(row.get("supported_endpoints"));
    let endpoints = if !native.is_empty() {
        native
    } else if !supported.is_empty() {
        supported
    } else {
        string_array(row.get("type_target"))
    };
    if let Some(raw) = preferred_raw(row, extra_field) {
        model = model.with_preferred_api(raw);
    }
    Ok(Some(model.with_endpoints(endpoints)))
}

/// Hofund's field wins. The configured name is read only when that field is absent.
fn preferred_raw<'a>(row: &'a Map<String, Value>, extra_field: Option<&str>) -> Option<&'a str> {
    if let Some(raw) = row.get(PREFERRED_API_FIELD).and_then(Value::as_str) {
        return Some(raw);
    }
    let extra = extra_field.filter(|name| !name.is_empty() && *name != PREFERRED_API_FIELD)?;
    row.get(extra).and_then(Value::as_str)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn positive(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= u64::MAX as f64)
        .map_or(0, |value| value as u64)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// `<list-url without /models>/codex/models?client_version=...`
fn codex_catalog_url(list_url: &str) -> String {
    let path = list_url.split('?').next().unwrap_or(list_url);
    let root = path.trim_end_matches('/').trim_end_matches("/models");
    format!("{root}/codex/models?client_version={CODEX_CLIENT_VERSION}")
}

/// Rows that are Codex catalog entries, keyed by id and by slug.
fn codex_entries(value: &Value) -> HashMap<String, String> {
    let rows = value
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| value.get("data").and_then(Value::as_array));
    let Some(rows) = rows else {
        return HashMap::new();
    };
    let mut entries = HashMap::new();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        if !codex_entry(object) {
            continue;
        }
        let Ok(text) = hofund_plugin_sdk::json::to_string(row) else {
            continue;
        };
        let slug = object.get("slug").and_then(Value::as_str).unwrap_or("");
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(slug);
        if !id.is_empty() {
            entries.insert(id.to_owned(), text.clone());
        }
        if !slug.is_empty() {
            entries.insert(slug.to_owned(), text);
        }
    }
    entries
}

/// A Codex entry has a slug, reasoning levels, a shell, and base instructions.
fn codex_entry(row: &hofund_plugin_sdk::json::Map<String, Value>) -> bool {
    let slug = row.get("slug").and_then(Value::as_str).unwrap_or("");
    let levels = row
        .get("supported_reasoning_levels")
        .and_then(Value::as_array);
    let shell = row.get("shell_type").and_then(Value::as_str).unwrap_or("");
    let instructions = row
        .get("base_instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    !slug.is_empty()
        && levels.is_some_and(|levels| !levels.is_empty())
        && !shell.is_empty()
        && !instructions.is_empty()
}

fn named_values(value: Option<&Value>, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    for value in value.and_then(Value::as_array).into_iter().flatten() {
        let named = value
            .as_str()
            .or_else(|| value.get(key).and_then(Value::as_str));
        if let Some(named) = named.filter(|named| !named.is_empty())
            && !values.iter().any(|present| present == named)
        {
            values.push(named.to_owned());
        }
    }
    values
}
