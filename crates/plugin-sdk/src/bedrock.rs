//! Bedrock's Messages wire, on the runtime and on Mantle, takes a subset of
//! Anthropic's betas and no `metadata`. These fit the header and body before
//! sending.
//!
//! Reference: `internal/gateway/betas.go:23-35` and `105-139`,
//! `internal/gateway/gateway.go:4045`, `4413-4420`, and `5256-5259`.

use hofund_plugin_api::Request;
use serde_json::Value;

/// Fits each `anthropic-beta` header value and drops one left empty.
#[must_use]
pub fn fit_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut fitted = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            let betas = fit_betas(std::iter::once(value.as_str()));
            if !betas.is_empty() {
                fitted.push((name.clone(), betas.join(",")));
            }
        } else {
            fitted.push((name.clone(), value.clone()));
        }
    }
    fitted
}

/// Drops `metadata` and fits the body's `anthropic_beta` list, removing it
/// when nothing is left.
pub fn fit_request(request: &mut Request) {
    request.extensions.remove("metadata");
    if let Some(Value::Array(values)) = request.extensions.get("anthropic_beta") {
        let asked: Vec<String> = values.iter().map(beta_text).collect();
        let fitted = fit_betas(asked.iter().map(String::as_str));
        if fitted == asked {
            return;
        }
        if fitted.is_empty() {
            request.extensions.remove("anthropic_beta");
        } else {
            request.extensions.insert(
                "anthropic_beta",
                Value::Array(fitted.into_iter().map(Value::String).collect()),
            );
        }
    }
}

fn beta_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Splits on commas, trims, renames the advanced tool use beta, and drops
/// empty, unsupported, and repeated values.
fn fit_betas<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut fitted = Vec::new();
    for value in values {
        for value in value.split(',') {
            let value = value.trim();
            let value = match value {
                "advanced-tool-use-2025-11-20" => "tool-search-tool-2025-10-19",
                other => other,
            };
            if value.is_empty()
                || matches!(
                    value,
                    "prompt-caching-scope-2026-01-05" | "redact-thinking-2026-02-12"
                )
                || fitted.iter().any(|present| present == value)
            {
                continue;
            }
            fitted.push(value.to_owned());
        }
    }
    fitted
}

#[cfg(test)]
mod tests;
