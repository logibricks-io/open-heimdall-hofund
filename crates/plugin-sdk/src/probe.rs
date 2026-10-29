//! Helpers for a provider probe: one minimal request that proves a model
//! answers.
//!
//! A probe reads at most [`ERROR_CAP`] bytes of a refusal and [`STREAM_CAP`]
//! bytes of a stream. [`probe_failure`] turns a refusal into the status error
//! the builtin adapters report, with [`status_hint`] advice.

use std::time::Duration;

use hofund_plugin_api::{PluginError, ProbeQuery, ProbeReport, redact_token, safe_url};
use serde_json::Value;

use crate::HttpResponse;
use crate::transport::parse_retry_after_headers;

/// How long a whole probe may take.
pub const PROBE_WAIT: Duration = Duration::from_secs(20);
/// The most of a 2xx streamed probe body that is read.
pub const STREAM_CAP: usize = 1024 * 1024;
/// The most of any other probe body that is read.
pub const ERROR_CAP: usize = 4096;

/// A probe response read up to its cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbePage {
    /// The HTTP status.
    pub status: u16,
    /// The wait the response asked for.
    pub retry_after: Option<Duration>,
    /// The body, cut at the cap.
    pub body: Vec<u8>,
}

/// Reads `response` up to [`STREAM_CAP`] for a 2xx stream and [`ERROR_CAP`]
/// otherwise, stopping at the cap. A failed read of a status outside 2xx
/// keeps what arrived.
///
/// # Errors
///
/// Returns an interrupted error when a 2xx body fails mid-read.
pub async fn read_probe(mut response: HttpResponse, stream: bool) -> Result<ProbePage, PluginError> {
    let status = response.status().as_u16();
    let retry_after = parse_retry_after_headers(response.headers(), std::time::SystemTime::now());
    let ok = (200..300).contains(&status);
    let cap = if ok && stream { STREAM_CAP } else { ERROR_CAP };
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
        let remaining = cap.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() >= cap {
            break;
        }
    }
    Ok(ProbePage {
        status,
        retry_after,
        body,
    })
}

/// The report for a probe page: the body for a 2xx reply, the first output
/// event of a stream, or the status error for anything else. Every non-empty
/// value in `secrets` is redacted from an error.
///
/// # Errors
///
/// Returns the [`probe_failure`] for a status outside 2xx, or the
/// [`stream_report`] error.
pub fn probe_report(
    url: &str,
    page: ProbePage,
    stream: bool,
    secrets: &[&str],
) -> Result<ProbeReport, PluginError> {
    if !(200..300).contains(&page.status) {
        return Err(probe_failure(url, &page, secrets));
    }
    if stream {
        // `stream_report` fails only with a decode error.
        return stream_report(&page.body)
            .map_err(|error| PluginError::decode(redact(error.message(), secrets)));
    }
    Ok(ProbeReport::new(String::from_utf8_lossy(&page.body)))
}

/// The status error for a refused probe: `{url}: HTTP {status} ({detail})`
/// plus any [`status_hint`], with the page's status and wait. The URL loses
/// its credentials and query, and every non-empty value in `secrets` is
/// redacted.
#[must_use]
pub fn probe_failure(url: &str, page: &ProbePage, secrets: &[&str]) -> PluginError {
    let detail = probe_error_message(&page.body);
    let mut message = format!("{}: HTTP {}", safe_url(url), page.status);
    if !detail.is_empty() {
        message.push_str(&format!(" ({detail})"));
    }
    message.push_str(&status_hint(&page.body));
    let error = PluginError::status_error(page.status, redact(&message, secrets));
    match page.retry_after {
        Some(after) => error.with_retry_after(after),
        None => error,
    }
}

/// The message in a probe's error body: the first non-empty JSON
/// `error.message`, `message`, `msg`, `detail`, or `errors[0].message`, or a
/// string `error`, or the whitespace-collapsed body. HTML is dropped. The
/// result is cut to 300 characters.
#[must_use]
pub fn probe_error_message(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        for pointer in [
            "/error/message",
            "/message",
            "/msg",
            "/detail",
            "/errors/0/message",
        ] {
            if let Some(message) = value.pointer(pointer).and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                return cut_runes(message.trim(), 300);
            }
        }
        if let Some(message) = value.get("error").and_then(Value::as_str) {
            return cut_runes(message.trim(), 300);
        }
    }
    let text = String::from_utf8_lossy(body);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() || compact.starts_with('<') {
        String::new()
    } else {
        cut_runes(&compact, 300)
    }
}

fn cut_runes(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let mut cut = text.chars().take(cap).collect::<String>();
    cut.push('…');
    cut
}

const BLOCK_ADVICE: &str =
    "the provider's network edge blocked this address; wait, or try another network or proxy";
const VERIFY_ADVICE: &str = "this Google account needs verification";

/// Advice appended to a probe's status error: a blocked network edge, a
/// Google account that needs verification (with its https link when the
/// body gives one), or a Go plan without API access. Empty otherwise; a
/// non-empty hint starts with ` — `.
#[must_use]
pub fn status_hint(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    if lower.contains("request has been blocked")
        || lower.contains("errors.aliyun.com")
        || lower.contains(&BLOCK_ADVICE.to_ascii_lowercase())
    {
        return format!(" — {BLOCK_ADVICE}");
    }
    // reference: internal/provider/test.go:690-727
    if let Some(link) = verification(body, &text, &lower) {
        return link.map_or_else(
            || format!(" — {VERIFY_ADVICE} before API access can work"),
            |link| {
                format!(
                    " — {VERIFY_ADVICE}: open {link} in a browser, verify the account, then try again"
                )
            },
        );
    }
    if lower.contains("go plan doesn") && lower.contains("include api access") {
        return " — this Go plan does not include Provider API access; use an API-enabled plan or a provider API key".to_owned();
    }
    String::new()
}

fn verification(body: &[u8], text: &str, lower: &str) -> Option<Option<String>> {
    let value: Option<Value> = serde_json::from_slice(body).ok();
    let details = value
        .as_ref()
        .and_then(|value| value.pointer("/error/details"))
        .and_then(Value::as_array);
    let mut required = false;
    let mut help = None;
    let mut validation = None;
    for detail in details.into_iter().flatten() {
        for link in detail
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if help.is_none() {
                help = link.get("url").and_then(Value::as_str).map(str::to_owned);
            }
        }
        if detail.get("reason").and_then(Value::as_str) == Some("VALIDATION_REQUIRED") {
            required = true;
            if validation.is_none() {
                validation = detail
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| {
                        metadata
                            .get("validation_url")
                            .or_else(|| metadata.get("validationUrl"))
                    })
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    }
    let words = lower.contains("verify your account")
        || lower.contains("account verification required")
        || lower.contains(&VERIFY_ADVICE.to_ascii_lowercase());
    if !required && !words {
        return None;
    }
    let link = if required { validation.or(help) } else { None }
        .or_else(|| own_advice_link(text))
        .and_then(|link| {
            let url = url::Url::parse(&link).ok()?;
            (url.scheme() == "https" && url.host_str().is_some()).then_some(link)
        });
    Some(link)
}

fn own_advice_link(text: &str) -> Option<String> {
    let start = text.find(VERIFY_ADVICE)? + VERIFY_ADVICE.len();
    let rest = text.get(start..)?.strip_prefix(": open ")?;
    let end = rest
        .find(|character: char| character.is_whitespace() || character == '"' || character == '\\')
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_owned(), |clean, secret| redact_token(&clean, secret))
}

/// Copies the probe's extra body keys over the top-level keys of `body`.
///
/// # Errors
///
/// Returns a decode error when the extra body is not a JSON object.
pub fn merge_extra(body: &mut Value, query: &ProbeQuery) -> Result<(), PluginError> {
    let Some(extra) = query.extra_body() else {
        return Ok(());
    };
    let extra: Value = serde_json::from_slice(extra)
        .map_err(|_| PluginError::decode("probe extra body is not JSON"))?;
    let Some(extra) = extra.as_object() else {
        return Err(PluginError::decode("probe extra body is not a JSON object"));
    };
    let Some(object) = body.as_object_mut() else {
        return Err(PluginError::decode("probe body is not a JSON object"));
    };
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    Ok(())
}

// reference: internal/provider/test.go:580-631
/// The first event of a streamed probe that carries output, or an empty
/// report when none does.
///
/// # Errors
///
/// Returns a decode error when an event before it reports an error.
pub fn stream_report(body: &[u8]) -> Result<ProbeReport, PluginError> {
    for line in String::from_utf8_lossy(body).lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "error" || event.get("error").is_some_and(|error| !error.is_null()) {
            return Err(PluginError::decode(format!(
                "error in the stream: {}",
                probe_error_message(data.as_bytes())
            )));
        }
        if kind == "response.failed" {
            let detail = event.pointer("/response/error").map_or_else(
                || "response failed".to_owned(),
                |error| probe_error_message(error.to_string().as_bytes()),
            );
            return Err(PluginError::decode(detail));
        }
        if matches!(
            kind,
            "content_block_start" | "content_block_delta" | "response.output_item.added"
        ) || (kind.starts_with("response.") && kind.ends_with(".delta"))
        {
            return Ok(ProbeReport::new(data));
        }
        if event
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .and_then(Value::as_object)
                        .is_some_and(|delta| {
                            ["content", "reasoning", "reasoning_content", "tool_calls"]
                                .iter()
                                .any(|key| {
                                    delta.get(*key).is_some_and(|value| {
                                        !value.is_null() && value.as_str() != Some("")
                                    })
                                })
                        })
                })
            })
            || event
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        ["text", "thought", "functionCall"]
                            .iter()
                            .any(|key| part.get(*key).is_some_and(|value| !value.is_null()))
                    })
                })
        {
            return Ok(ProbeReport::new(data));
        }
    }
    Ok(ProbeReport::new(""))
}

#[cfg(test)]
mod tests;
