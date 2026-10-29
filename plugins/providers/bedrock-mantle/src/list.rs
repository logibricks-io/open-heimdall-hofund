//! Mantle's live model list.
//!
//! The list asks `{base}/models` and, for Messages or a base without a
//! version segment, the `/v1/models` and root candidates the kernel's
//! OpenAI-compatible list asked. A candidate that lists no chat model is
//! skipped. Pages follow `after_id`.

use hofund_plugin_api::HttpHandle;
use hofund_plugin_api::{
    AuthChoice, ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginErrorKind,
    SecretString, redact_token, safe_url,
};
use hofund_plugin_sdk::classify::classify_model;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::probe::status_hint;
use hofund_plugin_sdk::transport::parse_retry_after_headers;
use hofund_plugin_sdk::{BODY_CAP, Call, PageError, PageErrorKind, after_id, json_error_message};

/// The Anthropic API version a Messages list sends.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// One provider's list inputs.
pub(crate) struct Lister<'a> {
    pub(crate) http: &'a HttpHandle,
    pub(crate) base: &'a str,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) messages: bool,
    pub(crate) secret: Option<&'a SecretString>,
}

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

/// One list URL. `v1_fallback` marks the URL that added `/v1` to the base.
struct Candidate {
    url: String,
    v1_fallback: bool,
}

impl Lister<'_> {
    pub(crate) async fn list(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let mut errors = Vec::new();
        for candidate in list_urls(self.base, self.messages) {
            let rows = after_id(&candidate.url, |url| async move {
                self.page(&url, query).await.map_err(ListFailure)
            })
            .await
            .map_err(|failure| failure.0);
            match rows {
                Ok(rows) => {
                    let (models, dropped) = model_specs(&rows)?;
                    if models.is_empty() {
                        errors.push(PluginError::decode(format!(
                            "{}: no models listed",
                            safe_url(&candidate.url)
                        )));
                        continue;
                    }
                    return Ok(ModelList::new(models, None)
                        .with_dropped(dropped)
                        .with_kind(ListKind::Live)
                        .with_v1_fallback(candidate.v1_fallback)
                        .with_source_url(&candidate.url));
                }
                Err(error) => errors.push(self.redact_error(&error)),
            }
        }
        Err(join_errors(errors))
    }

    /// One list page. Messages sends the key in `x-api-key` alone, as
    /// Bedrock asks; Chat and Responses send the choice the caller learned,
    /// or both headers.
    async fn page(&self, url: &str, query: &ListQuery) -> Result<Vec<u8>, PluginError> {
        let configured = |name: &str| {
            self.headers
                .iter()
                .any(|(header, _)| header.eq_ignore_ascii_case(name))
        };
        let mut call = Call::get(url).header("accept", "application/json");
        if self.messages && !configured("anthropic-version") {
            call = call.header("anthropic-version", ANTHROPIC_VERSION);
        }
        if let Some(secret) = self.secret.filter(|secret| !secret_text(secret).is_empty()) {
            let choice = if self.messages {
                AuthChoice::XApiKey
            } else {
                query.auth_choice().unwrap_or(AuthChoice::Both)
            };
            let token = secret_text(secret);
            if matches!(choice, AuthChoice::Both | AuthChoice::XApiKey) && !configured("x-api-key")
            {
                call = call.sensitive_header("x-api-key", token);
            }
            if matches!(choice, AuthChoice::Both | AuthChoice::Bearer)
                && !configured("authorization")
            {
                call = call.sensitive_header("authorization", format!("Bearer {token}"));
            }
        }
        for (name, value) in self.headers {
            call = call.sensitive_header(name.to_ascii_lowercase(), value.clone());
        }
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
        let mut error = PluginError::status_error(status, self.redact(&message));
        if let Some(after) = retry_after {
            error = error.with_retry_after(after);
        }
        Err(error)
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

    fn redact_error(&self, error: &PluginError) -> PluginError {
        rebuild(error, self.redact(error.message()))
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

/// Joins the candidates' messages. The last error supplies the kind,
/// status, and wait.
fn join_errors(errors: Vec<PluginError>) -> PluginError {
    let Some(last) = errors.last() else {
        return PluginError::decode("model list failed");
    };
    let message = errors
        .iter()
        .map(PluginError::message)
        .collect::<Vec<_>>()
        .join("; ");
    rebuild(last, message)
}

// reference: internal/catalog/live.go:351-396
fn list_urls(base: &str, messages: bool) -> Vec<Candidate> {
    let base = base.trim().trim_end_matches('/');
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut add = |url: String, v1_fallback: bool| {
        if !candidates.iter().any(|candidate| candidate.url == url) {
            candidates.push(Candidate { url, v1_fallback });
        }
    };
    add(format!("{base}/models"), false);
    if messages || !versioned(base) {
        add(format!("{base}/v1/models"), true);
        let mut root = base.to_owned();
        // reference: internal/catalog/live.go:366-371
        for suffix in [
            "/anthropic",
            "/apps/anthropic",
            "/api/anthropic",
            "/v1",
            "/api",
            "/api/v1",
        ] {
            if let Some(stripped) = root.strip_suffix(suffix) {
                root = stripped.to_owned();
            }
        }
        add(format!("{root}/v1/models"), false);
        add(format!("{root}/models"), false);
    }
    candidates
}

/// Whether a path segment is a version, `v` and a digit.
fn versioned(base: &str) -> bool {
    hofund_plugin_sdk::url::Url::parse(base).is_ok_and(|url| {
        url.path_segments().is_some_and(|mut segments| {
            segments.any(|segment| {
                let mut chars = segment.chars();
                matches!(chars.next(), Some('v' | 'V'))
                    && chars.next().is_some_and(|digit| digit.is_ascii_digit())
            })
        })
    })
}

fn model_specs(rows: &[Value]) -> Result<(Vec<ModelSpec>, Vec<String>), PluginError> {
    let mut models = Vec::new();
    let mut dropped = Vec::new();
    for row in rows {
        let id = row.as_str().or_else(|| {
            row.get("id")
                .and_then(Value::as_str)
                .or_else(|| row.get("name").and_then(Value::as_str))
        });
        if let Some(model) = model_spec(row)? {
            models.push(model);
        } else if let Some(id) = id.filter(|id| !id.is_empty()) {
            dropped.push(id.to_owned());
        }
    }
    Ok((models, dropped))
}

/// One OpenAI-compatible model row, as the kernel's live list read it.
fn model_spec(row: &Value) -> Result<Option<ModelSpec>, PluginError> {
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
    Ok(Some(model.with_endpoints(endpoints)))
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

#[cfg(test)]
mod tests {
    use super::{list_urls, versioned};

    #[test]
    fn a_versioned_base_asks_its_own_list_only() {
        let urls = list_urls("https://bedrock-mantle.us-east-1.api.aws/v1/", false);
        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0].url,
            "https://bedrock-mantle.us-east-1.api.aws/v1/models"
        );
        assert!(!urls[0].v1_fallback);
    }

    #[test]
    fn messages_and_an_unversioned_base_try_the_root_candidates() {
        let urls: Vec<(String, bool)> =
            list_urls("https://bedrock-mantle.us-east-1.api.aws/anthropic", true)
                .into_iter()
                .map(|candidate| (candidate.url, candidate.v1_fallback))
                .collect();
        assert_eq!(
            urls,
            [
                (
                    "https://bedrock-mantle.us-east-1.api.aws/anthropic/models".to_owned(),
                    false
                ),
                (
                    "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/models".to_owned(),
                    true
                ),
                (
                    "https://bedrock-mantle.us-east-1.api.aws/v1/models".to_owned(),
                    false
                ),
                (
                    "https://bedrock-mantle.us-east-1.api.aws/models".to_owned(),
                    false
                ),
            ]
        );
        let urls = list_urls("https://relay.example/mantle", false);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[1].url, "https://relay.example/mantle/v1/models");
        assert!(urls[1].v1_fallback);
        let urls = list_urls("https://relay.example/mantle/api", false);
        assert_eq!(urls.len(), 4);
        assert_eq!(urls[2].url, "https://relay.example/mantle/v1/models");
        assert!(versioned("https://h/V2/x"));
        assert!(!versioned("https://h/version"));
        assert!(!versioned("not a url"));
    }
}
