//! HTTP calls on the host's pooled client.
//!
//! This module builds requests and reads responses. It does not construct a
//! client. [`Call::send`] uses [`HttpHandle::send`](hofund_plugin_api::HttpHandle::send).
//! A status of 400 or higher becomes [`PluginErrorKind::Status`].
//!
//! `HttpHandle::send` streams a 2xx body, but [`Call`] still reads the whole
//! body. The Chat, Responses, Messages, Generative, and Converse transports move
//! into the SDK later, starting with Chat and Responses.
//!
//! [`Call::send_with_retry`] retries at most three times. [`PluginErrorKind::Connect`]
//! (the request was not sent) is retried for every method. [`PluginErrorKind::Timeout`]
//! [`PluginErrorKind::Transport`], and [`PluginErrorKind::Interrupted`] are
//! retried only for an idempotent method, or when [`Call::idempotent`] is set.
//! The wait between attempts is full-jitter
//! exponential backoff, and dropping the future cancels it.

use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use hofund_plugin_api::{
    Body, BodyStream, CredentialStyle, HttpHandle, PluginError, PluginErrorKind, SecretString,
};
use secrecy::ExposeSecret;

/// Request type plugins use so they do not name the `http` crate.
pub type HttpRequest = http::Request<Body>;

/// Response type plugins use so they do not name the `http` crate.
pub type HttpResponse = http::Response<BodyStream>;

const MAX_ATTEMPTS: u8 = 3;
const STATUS_SNIPPET: usize = 256;
const BACKOFF_BASE: Duration = Duration::from_millis(20);
const BACKOFF_CAP: Duration = Duration::from_millis(200);

/// A buffered HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The response body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Consumes the reply and returns the body.
    #[must_use]
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
}

/// One outbound call. Header values are omitted from [`Debug`].
#[derive(Clone)]
pub struct Call {
    method: &'static str,
    uri: String,
    headers: Vec<Header>,
    body: Vec<u8>,
    idempotent: bool,
}

#[derive(Clone)]
struct Header {
    name: String,
    value: String,
    sensitive: bool,
}

impl Call {
    /// `POST` `uri`. `uri` is absolute.
    #[must_use]
    pub fn post(uri: impl Into<String>) -> Self {
        Self::new("POST", uri)
    }

    /// `GET` `uri`. `uri` is absolute.
    #[must_use]
    pub fn get(uri: impl Into<String>) -> Self {
        Self::new("GET", uri)
    }

    fn new(method: &'static str, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            headers: Vec::new(),
            body: Vec::new(),
            idempotent: false,
        }
    }

    /// Marks whether a non-idempotent method may be retried after the body was sent.
    ///
    /// `POST` and `PATCH` retry only [`PluginErrorKind::Connect`] unless this
    /// is `true`. Set it when the upstream treats the call as idempotent, for
    /// example because the request carries an idempotency key. `GET`, `HEAD`,
    /// `OPTIONS`, `PUT`, and `DELETE` already retry timeout, transport, and
    /// interrupted-body failures. The default is `false`.
    #[must_use]
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// Adds a header. The value is not marked sensitive.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        });
        self
    }

    /// Adds a header and marks the value sensitive.
    #[must_use]
    pub fn sensitive_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
            sensitive: true,
        });
        self
    }

    /// `Authorization: Bearer`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the token is not a valid header value.
    pub fn bearer(self, token: &SecretString) -> Result<Self, PluginError> {
        self.credential(CredentialStyle::Bearer, token)
    }

    /// Places `secret` using `style`. The header name is lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the secret is not a valid header value, or
    /// when `style` is not one of the known variants.
    pub fn credential(
        self,
        style: CredentialStyle,
        secret: &SecretString,
    ) -> Result<Self, PluginError> {
        let token = secret.expose_secret();
        match style {
            CredentialStyle::Bearer => {
                Ok(self.sensitive_header("authorization", format!("Bearer {token}")))
            }
            CredentialStyle::ApiKey => Ok(self.sensitive_header("api-key", token)),
            CredentialStyle::XApiKey => Ok(self.sensitive_header("x-api-key", token)),
            CredentialStyle::MessagesBoth => Ok(self
                .sensitive_header("x-api-key", token)
                .sensitive_header("authorization", format!("Bearer {token}"))),
            CredentialStyle::NamedHeader(name) => Ok(self.sensitive_header(name.as_str(), token)),
            _ => Err(PluginError::unsupported(
                "credential style is not supported",
            )),
        }
    }

    /// Sets the body bytes.
    #[must_use]
    pub fn bytes(mut self, body: impl AsRef<[u8]>) -> Self {
        self.body = body.as_ref().to_vec();
        self
    }

    /// Sets a JSON body and `content-type: application/json` when that header
    /// is not already present.
    #[must_use]
    pub fn json(self, body: impl AsRef<[u8]>) -> Self {
        let call = if self
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("content-type"))
        {
            self
        } else {
            self.header("content-type", "application/json")
        };
        call.bytes(body)
    }

    /// Sends the call once.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when a header is invalid, the host client fails,
    /// or the status is 400 or higher.
    pub async fn send(self, http: &HttpHandle) -> Result<Reply, PluginError> {
        let request = self.request()?;
        let response = http.send(request).await?;
        let (status, retry_after, body) = read_body(response).await?;
        classify(status, retry_after, body)
    }

    /// Sends the call up to `attempts` times, including the first try.
    ///
    /// The cap is three attempts. [`PluginErrorKind::Connect`] is retried for
    /// every method, because the request was not sent. [`PluginErrorKind::Timeout`],
    /// [`PluginErrorKind::Transport`], and [`PluginErrorKind::Interrupted`] are
    /// retried for `GET`, `HEAD`, `OPTIONS`, `PUT`, and `DELETE`, and for a call
    /// marked [`Call::idempotent`]. `POST` and `PATCH` are not retried on those
    /// three kinds: the body may already have been processed.
    ///
    /// Between attempts the call waits a full-jitter exponential backoff,
    /// starting at 20ms and capped at 200ms. The wait is a normal sleep.
    /// Dropping this future cancels the sleep and does not start another attempt.
    ///
    /// # Errors
    ///
    /// Returns the last [`PluginError`] when every attempt fails, or the first
    /// error that this call must not retry.
    pub async fn send_with_retry(
        self,
        http: &HttpHandle,
        attempts: u8,
    ) -> Result<Reply, PluginError> {
        let attempts = attempts.clamp(1, MAX_ATTEMPTS);
        let mut attempt = 1_u8;
        loop {
            match self.clone().send(http).await {
                Ok(reply) => return Ok(reply),
                Err(error)
                    if attempt < attempts
                        && retries(self.method, self.idempotent, error.kind()) =>
                {
                    let failed = u32::from(attempt);
                    attempt += 1;
                    tokio::time::sleep(backoff(failed)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn request(self) -> Result<HttpRequest, PluginError> {
        let mut builder = http::Request::builder().method(self.method).uri(&self.uri);
        for header in &self.headers {
            builder = builder.header(&header.name, header_value(header)?);
        }
        builder
            .body(Body::from_bytes(self.body))
            .map_err(|error| PluginError::internal(error.to_string()))
    }
}

impl fmt::Debug for Call {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .headers
            .iter()
            .map(|header| header.name.as_str())
            .collect();
        formatter
            .debug_struct("Call")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("header_names", &names)
            .field("body_len", &self.body.len())
            .field("idempotent", &self.idempotent)
            .finish()
    }
}

fn header_value(header: &Header) -> Result<http::HeaderValue, PluginError> {
    let mut value = http::HeaderValue::from_str(&header.value)
        .map_err(|_| PluginError::config(format!("{} is not a valid header value", header.name)))?;
    value.set_sensitive(header.sensitive);
    Ok(value)
}

fn retries(method: &str, idempotent: bool, kind: PluginErrorKind) -> bool {
    // A status proves the upstream received the request. Only idempotent calls
    // may repeat an interrupted body read on the same member. The reference
    // records when the body was written; a reply after that belongs to the vendor.
    // reference: internal/gateway/gateway.go:2753-2754
    match kind {
        PluginErrorKind::Connect => true,
        PluginErrorKind::Timeout | PluginErrorKind::Transport | PluginErrorKind::Interrupted => {
            idempotent || method_is_idempotent(method)
        }
        _ => false,
    }
}

fn method_is_idempotent(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE")
}

/// Full jitter: uniform in `0..cap`. `failed_attempts` is 1 after the first try.
fn backoff(failed_attempts: u32) -> Duration {
    let shift = failed_attempts.saturating_sub(1).min(4);
    let factor = 1_u32 << shift;
    let grown = BACKOFF_BASE.saturating_mul(factor);
    full_jitter(grown.min(BACKOFF_CAP))
}

fn full_jitter(cap: Duration) -> Duration {
    let cap_nanos = u64::try_from(cap.as_nanos()).unwrap_or(u64::MAX);
    if cap_nanos == 0 {
        return Duration::ZERO;
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(cap_nanos);
    Duration::from_nanos(hasher.finish() % cap_nanos)
}

fn classify(
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
) -> Result<Reply, PluginError> {
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&body);
        let snippet = snippet.chars().take(STATUS_SNIPPET).collect::<String>();
        let message = if snippet.is_empty() {
            format!("upstream returned status {status}")
        } else {
            snippet
        };
        let error = if is_quota_error(status, &body) {
            PluginError::quota_status(status, message)
        } else {
            PluginError::status_error(status, message)
        };
        return Err(match retry_after {
            Some(after) => error.with_retry_after(after),
            None => error,
        });
    }
    Ok(Reply { status, body })
}

/// Whether an upstream status and response body report exhausted quota.
///
/// The rules follow the reference's failure classification, in its order: a
/// 402 naming a usage limit and no money is quota; a credit or balance
/// refusal (including `insufficient_quota`) is not; a 429 naming its reset
/// time is quota; a 429 naming a per-minute limit, a short wait, or a plain
/// rate limit is not; otherwise a 5xx quota code, a sub-500 status naming a
/// used-up limit, or a 429 naming a planned allowance is quota.
#[must_use]
pub fn is_quota_error(status: u16, body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let insufficient_quota = text.contains("insufficient_quota");
    if status == 402 {
        return used_up_words(&text) && !credit_words(&text) && !insufficient_quota;
    }
    if credit_words(&text) && (status != 429 || broke_words(&text)) || insufficient_quota {
        return false;
    }
    if status == 429 {
        if resets_at(&text) {
            return true;
        }
        if minute_limit(&text) || !credit_words(&text) && back_soon(&text) {
            return false;
        }
        if rate_words(&text) && !planned_words(&without_urls(&text)) {
            return false;
        }
        return used_up_words(&text) || planned_words(&text);
    }
    if status >= 500 {
        quota_codes(&text)
    } else {
        used_up_words(&text)
    }
}

fn used_up_words(text: &str) -> bool {
    text.contains("quota")
        || text.contains("out of budget")
        || text.contains("budget exceeded")
        || text.contains("budget exhausted")
        || optional_separator(text, "usage", "limit")
        || optional_separator(text, "limit", "reached")
        || contains_then(text, "hit your ", "limit")
        || limit_resets(text)
        || contains_then(text, "exceeded", "plan")
        || contains_then(text, "exceeded", "limit")
}

fn planned_words(text: &str) -> bool {
    text.contains("quota")
        || optional_separator(text, "usage", "limit")
        || contains_then(text, "hit your ", "limit")
        || limit_resets(text)
        || ["day", "week", "month"]
            .into_iter()
            .any(|unit| optional_separator(text, "per", unit))
        || ["daily", "weekly", "monthly"]
            .into_iter()
            .any(|word| text.contains(word))
}

fn rate_words(text: &str) -> bool {
    optional_separator(text, "rate", "limit")
        || optional_separator(text, "frequency", "limit")
        || text.contains("too many requests")
        || minute_words(text)
}

fn minute_limit(text: &str) -> bool {
    let text = &without_urls(text);
    minute_words(text)
        && !long_words(text)
        && !broke_words(text)
        && !text.contains("insufficient_quota")
}

fn minute_words(text: &str) -> bool {
    ["second", "sec", "minute", "min"]
        .into_iter()
        .any(|unit| optional_separator(text, "per", unit))
        || has_word(text, &["rpm", "tpm", "rps", "tps"])
        || ["token", "call", "request"]
            .into_iter()
            .any(|kind| text.contains(&format!("{kind} rate limit")))
}

fn long_words(text: &str) -> bool {
    optional_separator(text, "usage", "limit")
        || contains_then(text, "hit your ", "limit")
        || limit_resets(text)
        || ["hour", "day", "week", "month"]
            .into_iter()
            .any(|unit| optional_separator(text, "per", unit))
        || ["hour", "hourly", "daily", "weekly", "monthly"]
            .into_iter()
            .any(|word| text.contains(word))
        || has_word(text, &["rpd", "tpd", "rph", "tph"])
}

fn credit_words(text: &str) -> bool {
    broke_words(text)
        || [
            "balance",
            "credit",
            "billing",
            "payment",
            "arrear",
            "overdue",
            "suspended",
        ]
        .into_iter()
        .any(|word| text.contains(word))
}

fn broke_words(text: &str) -> bool {
    ["balance", "credit", "fund"]
        .into_iter()
        .any(|word| optional_separator(text, "insufficient", word))
        || [
            "credit exhausted",
            "credits exhausted",
            "credit are exhausted",
            "credits are exhausted",
        ]
        .into_iter()
        .any(|phrase| text.contains(phrase))
}

fn quota_codes(text: &str) -> bool {
    text.contains("usage_limit_reached")
        || text.contains("resource_exhausted")
        || text.match_indices("limit reached|").any(|(index, _)| {
            let suffix = &text[index + "limit reached|".len()..];
            let mut characters = suffix.chars();
            let digits = characters.by_ref().take(10).collect::<String>();
            digits.len() == 10
                && digits.chars().all(|character| character.is_ascii_digit())
                && characters
                    .next()
                    .is_none_or(|character| !character.is_ascii_digit())
        })
}

fn resets_at(text: &str) -> bool {
    (text.contains("reset at") || text.contains("resets at"))
        && text.contains(|character: char| character.is_ascii_digit())
        && (text.contains("utc") || text.contains("gmt"))
}

fn back_soon(text: &str) -> bool {
    ["retry in", "try again in", "resets in"]
        .into_iter()
        .filter_map(|phrase| text.find(phrase).map(|index| &text[index + phrase.len()..]))
        .any(|tail| {
            !tail.contains("hour")
                && !tail.contains("day")
                && !tail.contains("week")
                && !tail.contains("month")
                && !tail.split_whitespace().take(8).any(|token| {
                    let unit = token.trim_start_matches(|character: char| {
                        character.is_ascii_digit() || character == '.'
                    });
                    matches!(
                        unit.trim_matches(|character: char| !character.is_alphabetic()),
                        "h" | "hr" | "hrs" | "d" | "w"
                    )
                })
        })
}

fn optional_separator(text: &str, left: &str, right: &str) -> bool {
    [format!("{left}{right}"), format!("{left} {right}")]
        .into_iter()
        .any(|needle| text.contains(&needle))
        || text.match_indices(left).any(|(index, _)| {
            let after = &text[index + left.len()..];
            let mut characters = after.char_indices();
            let Some((_, separator)) = characters.next() else {
                return false;
            };
            let Some((start, _)) = characters.next() else {
                return false;
            };
            !separator.is_alphanumeric() && after[start..].starts_with(right)
        })
}

fn contains_then(text: &str, first: &str, second: &str) -> bool {
    text.match_indices(first)
        .any(|(index, _)| text[index + first.len()..].contains(second))
}

fn limit_resets(text: &str) -> bool {
    text.match_indices("limit").any(|(index, _)| {
        text[index + "limit".len()..]
            .chars()
            .take(24)
            .collect::<String>()
            .contains("resets")
    })
}

fn has_word(text: &str, words: &[&str]) -> bool {
    text.split(|character: char| !character.is_alphanumeric())
        .any(|word| words.contains(&word))
}

fn without_urls(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = [rest.find("http://"), rest.find("https://")]
        .into_iter()
        .flatten()
        .min()
    {
        output.push_str(&rest[..index]);
        rest = &rest[index..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        output.push(' ');
        rest = &rest[end..];
    }
    output.push_str(rest);
    output
}

async fn read_body(
    mut response: HttpResponse,
) -> Result<(u16, Option<Duration>, Vec<u8>), PluginError> {
    let status = response.status().as_u16();
    let retry_after = parse_retry_after_headers(response.headers(), std::time::SystemTime::now());
    let mut body = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await {
        body.extend_from_slice(&chunk?);
    }
    Ok((status, retry_after, body))
}

/// Parses Retry-After and rate-limit reset headers relative to `now`.
/// A positive Retry-After value takes precedence. Otherwise the longest
/// positive `*ratelimit*reset*` header wins: an RFC 3339 time, a duration such
/// as `1.5s` or `1m30s`, or a Unix timestamp in seconds. Values are not
/// clamped here; the router clamps every hint to its policy maximum.
#[must_use]
pub fn parse_retry_after_headers(
    headers: &http::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    if let Some(after) = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now))
    {
        return Some(after);
    }
    let mut longest = None;
    for name in headers.keys() {
        let name = name.as_str();
        if !name.contains("ratelimit") || !name.contains("reset") {
            continue;
        }
        let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        let after = parse_rfc3339(value, now)
            .or_else(|| parse_duration(value))
            .or_else(|| parse_unix_reset(value, now));
        if after.is_some_and(|after| longest.is_none_or(|longest| after > longest)) {
            longest = after;
        }
    }
    longest
}

/// Parses a Retry-After delta-seconds value or HTTP-date relative to `now`.
/// Fractional seconds are accepted. Zero and past durations are ignored.
#[must_use]
pub fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    parse_seconds(value).or_else(|| {
        let at = httpdate::parse_http_date(value).ok()?;
        positive(at.duration_since(now).ok()?)
    })
}

fn parse_seconds(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return positive(Duration::from_secs(seconds));
    }
    let seconds = value.parse::<f64>().ok()?;
    nanos_duration(seconds * 1_000_000_000.0)
}

fn parse_duration(value: &str) -> Option<Duration> {
    let rest = value.strip_prefix('+').unwrap_or(value);
    if rest.starts_with('-') {
        return None;
    }
    let mut rest = rest;
    let mut nanos = 0.0;
    let mut terms = 0;
    while !rest.is_empty() {
        let number_end = rest
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_digit() || *character == '.')
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        let number = rest[..number_end].parse::<f64>().ok()?;
        rest = &rest[number_end..];
        let (unit, factor) = [
            ("ns", 1.0),
            ("us", 1_000.0),
            ("µs", 1_000.0),
            ("μs", 1_000.0),
            ("ms", 1_000_000.0),
            ("s", 1_000_000_000.0),
            ("m", 60_000_000_000.0),
            ("h", 3_600_000_000_000.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        nanos += number * factor;
        rest = &rest[unit.len()..];
        terms += 1;
    }
    (terms > 0).then(|| nanos_duration(nanos)).flatten()
}

fn parse_unix_reset(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    let seconds = value.parse::<u64>().ok()?;
    if seconds <= 1_000_000_000 {
        return None;
    }
    let at = std::time::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    positive(at.duration_since(now).ok()?)
}

fn parse_rfc3339(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    if !value.is_ascii() || value.len() < 20 || &value[4..5] != "-" || &value[7..8] != "-" {
        return None;
    }
    if !matches!(&value[10..11], "T" | "t") || &value[13..14] != ":" || &value[16..17] != ":" {
        return None;
    }
    let year = fixed_number(value, 0, 4)?;
    let month = fixed_number(value, 5, 7)?;
    let day = fixed_number(value, 8, 10)?;
    let hour = fixed_number(value, 11, 13)?;
    let minute = fixed_number(value, 14, 16)?;
    let second = fixed_number(value, 17, 19)?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > i64::from(days_in_month(year, month))
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut index = 19;
    let mut fraction = 0_i128;
    if matches!(value.as_bytes().get(index), Some(b'.' | b',')) {
        index += 1;
        let start = index;
        while value.as_bytes().get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
        for character in value[start..index].chars().take(9) {
            fraction = fraction * 10 + i128::from(character as u8 - b'0');
        }
        for _ in index - start..9 {
            fraction *= 10;
        }
    }
    let offset = match value.as_bytes().get(index) {
        Some(b'Z' | b'z') if index + 1 == value.len() => 0,
        Some(b'+' | b'-') if index + 6 == value.len() => {
            let hours = fixed_number(value, index + 1, index + 3)?;
            let minutes = fixed_number(value, index + 4, index + 6)?;
            if &value[index + 3..index + 4] != ":" || hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = hours * 3_600 + minutes * 60;
            if value.as_bytes()[index] == b'-' {
                -seconds
            } else {
                seconds
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let timestamp = i128::from(days * 86_400 + hour * 3_600 + minute * 60 + second - offset);
    let target = timestamp * 1_000_000_000 + fraction;
    let now = i128::try_from(now.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()).ok()?;
    let difference = target - now;
    if difference <= 0 {
        return None;
    }
    let seconds = u64::try_from(difference / 1_000_000_000).unwrap_or(u64::MAX);
    let nanos = u32::try_from(difference % 1_000_000_000).unwrap_or(0);
    Some(Duration::new(seconds, nanos))
}

fn fixed_number(value: &str, start: usize, end: usize) -> Option<i64> {
    value.get(start..end)?.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn nanos_duration(nanos: f64) -> Option<Duration> {
    if !nanos.is_finite() || nanos < 1.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(nanos / 1_000_000_000.0).unwrap_or(Duration::MAX))
}

fn positive(after: Duration) -> Option<Duration> {
    (!after.is_zero()).then_some(after)
}

/// A text response. Plugins return this from [`InboundSurface::handle`](hofund_plugin_api::InboundSurface::handle).
#[must_use]
pub fn text_response(status: u16, body: impl AsRef<[u8]>) -> HttpResponse {
    let code =
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = http::Response::new(BodyStream::from_bytes(body.as_ref().to_vec()));
    *response.status_mut() = code;
    response
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_plugin_api::{CredentialStyle, HeaderName, PluginErrorKind, SecretString};

    use super::{
        Call, backoff, classify, is_quota_error, parse_retry_after, parse_retry_after_headers,
        retries, text_response,
    };

    fn secret(value: &str) -> SecretString {
        SecretString::from(value)
    }

    #[test]
    fn an_invalid_local_header_value_is_a_config_error() {
        let error = Call::get("https://example.test")
            .header("x-local-setting", "invalid\nvalue")
            .request()
            .expect_err("invalid header");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert_eq!(
            error.message(),
            "x-local-setting is not a valid header value"
        );
    }

    #[test]
    fn bearer_marks_authorization_and_hides_the_value_in_debug() {
        let call = Call::post("https://example.test/v1")
            .credential(CredentialStyle::Bearer, &secret("top-secret"))
            .expect("bearer");
        let rendered = format!("{call:?}");
        assert!(!rendered.contains("top-secret"));
        assert!(rendered.contains("authorization"));
        let request = call.request().expect("request");
        let header = request.headers().get("authorization").expect("header");
        assert!(header.is_sensitive());
        assert_eq!(header.as_bytes(), b"Bearer top-secret");
    }

    #[test]
    fn each_credential_style_uses_the_documented_header() {
        let styles = [
            (CredentialStyle::ApiKey, "api-key", "k"),
            (CredentialStyle::XApiKey, "x-api-key", "k"),
            (
                CredentialStyle::NamedHeader(HeaderName::new("x-acme-key")),
                "x-acme-key",
                "k",
            ),
        ];
        for (style, name, value) in styles {
            let call = Call::get("https://example.test")
                .credential(style, &secret(value))
                .expect("credential");
            let request = call.request().expect("request");
            let header = request.headers().get(name).expect(name);
            assert!(header.is_sensitive());
            assert_eq!(header.as_bytes(), value.as_bytes());
        }
        let both = Call::post("https://example.test")
            .credential(CredentialStyle::MessagesBoth, &secret("k"))
            .expect("both");
        let request = both.request().expect("request");
        assert!(request.headers().get("x-api-key").unwrap().is_sensitive());
        assert!(
            request
                .headers()
                .get("authorization")
                .unwrap()
                .is_sensitive()
        );
    }

    #[test]
    fn retry_after_is_unbounded_and_attached_to_status_errors() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = httpdate::fmt_http_date(now + Duration::from_secs(120));
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after(&date, now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("7200", now),
            Some(Duration::from_secs(7200))
        );
        assert_eq!(
            parse_retry_after(&u64::MAX.to_string(), now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(parse_retry_after("invalid", now), None);
        assert_eq!(parse_retry_after("0", now), None);
        assert_eq!(parse_retry_after(&httpdate::fmt_http_date(now), now), None);
        let error =
            classify(429, Some(Duration::from_secs(120)), b"slow".to_vec()).expect_err("status");
        assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
        let missing = classify(429, None, b"slow".to_vec()).expect_err("status");
        assert_eq!(missing.retry_after(), None);
    }

    #[test]
    fn status_400_is_an_error_and_retry_follows_the_method() {
        let error = classify(401, None, b"nope".to_vec()).expect_err("status");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(401));
        assert_eq!(error.message(), "nope");
        let ok = classify(200, None, b"pong".to_vec()).expect("ok");
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.body(), b"pong");
        for method in ["GET", "HEAD", "OPTIONS", "PUT", "DELETE"] {
            assert!(retries(method, false, PluginErrorKind::Connect));
            assert!(retries(method, false, PluginErrorKind::Timeout));
            assert!(retries(method, false, PluginErrorKind::Transport));
            assert!(retries(method, false, PluginErrorKind::Interrupted));
        }
        for method in ["POST", "PATCH"] {
            assert!(retries(method, false, PluginErrorKind::Connect));
            assert!(!retries(method, false, PluginErrorKind::Timeout));
            assert!(!retries(method, false, PluginErrorKind::Transport));
            assert!(!retries(method, false, PluginErrorKind::Interrupted));
            assert!(retries(method, true, PluginErrorKind::Timeout));
            assert!(retries(method, true, PluginErrorKind::Transport));
            assert!(retries(method, true, PluginErrorKind::Interrupted));
        }
        assert!(!retries("POST", true, PluginErrorKind::Status));
        assert!(!retries("GET", true, PluginErrorKind::Auth));
        let marked = Call::post("https://example.test").idempotent(true);
        assert!(marked.idempotent);
        let plain = Call::post("https://example.test");
        assert!(!plain.idempotent);
        assert!(!format!("{plain:?}").contains("secret"));
        let json = Call::post("https://example.test").json(br#"{"model":"demo"}"#);
        let rendered = format!("{json:?}");
        assert!(rendered.contains("content-type"), "{rendered}");
        assert!(rendered.contains("body_len"), "{rendered}");
        let kept = Call::post("https://example.test")
            .header("Content-Type", "application/json")
            .json(br"{}");
        let names = format!("{kept:?}");
        assert_eq!(names.matches("content-type").count(), 0);
        assert_eq!(names.matches("Content-Type").count(), 1, "{names}");
    }

    #[test]
    fn backoff_stays_inside_the_cap_and_is_not_constant() {
        let mut samples = Vec::new();
        for _ in 0..24 {
            let delay = backoff(1);
            assert!(delay <= super::BACKOFF_BASE);
            samples.push(delay);
        }
        assert!(
            samples.iter().any(|delay| *delay != samples[0]),
            "full jitter"
        );
        assert!(backoff(2) <= super::BACKOFF_BASE.saturating_mul(2));
        assert!(backoff(8) <= super::BACKOFF_CAP);
    }

    #[tokio::test]
    async fn text_response_sets_status_and_body() {
        let mut response = text_response(201, "echo");
        assert_eq!(response.status().as_u16(), 201);
        let chunk = response
            .body_mut()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("bytes");
        assert_eq!(&chunk[..], b"echo");
    }

    #[test]
    fn quota_classifier_follows_each_reference_rule() {
        // 402 naming a usage limit and no money.
        assert!(is_quota_error(
            402,
            b"You've reached your weekly standard usage limit (resets in 3 days)."
        ));
        // 429 naming the time it resets.
        assert!(is_quota_error(
            429,
            b"Limit exhausted, resets at 2026-01-02 03:04:05 UTC+8."
        ));
        // 5xx quota codes.
        assert!(is_quota_error(503, b"{\"code\":\"usage_limit_reached\"}"));
        assert!(is_quota_error(500, b"RESOURCE_EXHAUSTED"));
        assert!(is_quota_error(500, b"limit reached|1767225600"));
        // Sub-500 statuses naming a used-up limit.
        assert!(is_quota_error(403, b"Quota exceeded for this project."));
        assert!(is_quota_error(400, b"You have hit your monthly limit"));
        // 429 naming a planned allowance.
        assert!(is_quota_error(429, b"Daily request quota used up"));
        assert!(is_quota_error(429, b"You exceeded your plan's usage limit"));
    }

    #[test]
    fn quota_classifier_keeps_rate_limits_credit_and_plain_errors_out() {
        assert!(!is_quota_error(429, b"Too many requests"));
        assert!(!is_quota_error(
            429,
            b"Rate limit reached: 60 requests per minute"
        ));
        assert!(!is_quota_error(
            429,
            b"Usage limit hit, try again in 20 seconds"
        ));
        assert!(!is_quota_error(
            429,
            b"rate limit exceeded, see https://example.test/quota-docs"
        ));
        // The reference reads these as out of credit, not quota.
        assert!(!is_quota_error(
            429,
            b"{\"error\":{\"type\":\"insufficient_quota\"}}"
        ));
        assert!(!is_quota_error(403, b"Insufficient balance for this quota"));
        assert!(!is_quota_error(402, b"Payment required"));
        assert!(!is_quota_error(500, b"quota service unavailable"));
        assert!(!is_quota_error(400, b"invalid request"));
    }

    #[test]
    fn classify_reports_quota_bodies_with_their_status() {
        let error = classify(429, None, b"Daily quota exceeded".to_vec()).expect_err("quota");
        assert_eq!(error.kind(), PluginErrorKind::Quota);
        assert_eq!(error.status(), Some(429));
        let error = classify(429, None, b"Too many requests".to_vec()).expect_err("rate");
        assert_eq!(error.kind(), PluginErrorKind::Status);
    }

    #[test]
    fn retry_after_keeps_fractional_seconds() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after("1.5", now),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            parse_retry_after("0.25", now),
            Some(Duration::from_millis(250))
        );
        assert_eq!(parse_retry_after("-1", now), None);
        assert_eq!(parse_retry_after("NaN", now), None);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().expect("header value"));
        }
        map
    }

    #[test]
    fn rate_limit_reset_headers_parse_each_reference_format() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after_headers(&headers(&[("x-ratelimit-reset-requests", "1m30.5s")]), now),
            Some(Duration::from_millis(90_500))
        );
        assert_eq!(
            parse_retry_after_headers(&headers(&[("x-ratelimit-reset-tokens", "250ms")]), now),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse_retry_after_headers(
                &headers(&[(
                    "anthropic-ratelimit-requests-reset",
                    "2023-11-14T22:13:27.5Z"
                )]),
                now
            ),
            Some(Duration::from_millis(7_500))
        );
        assert_eq!(
            parse_retry_after_headers(
                &headers(&[(
                    "anthropic-ratelimit-tokens-reset",
                    "2023-11-14T23:13:27+01:00"
                )]),
                now
            ),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            parse_retry_after_headers(&headers(&[("x-ratelimit-reset", "1700000042")]), now),
            Some(Duration::from_secs(42))
        );
        // A small integer is neither a duration nor a Unix time.
        assert_eq!(
            parse_retry_after_headers(&headers(&[("x-ratelimit-reset", "42")]), now),
            None
        );
        // Past times and unrelated headers are ignored.
        assert_eq!(
            parse_retry_after_headers(
                &headers(&[
                    ("x-ratelimit-reset", "1699999999"),
                    ("x-request-reset", "5s"),
                ]),
                now
            ),
            None
        );
    }

    #[test]
    fn retry_after_takes_precedence_and_the_longest_reset_header_wins() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after_headers(
                &headers(&[("retry-after", "2"), ("x-ratelimit-reset-requests", "9s")]),
                now
            ),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            parse_retry_after_headers(
                &headers(&[
                    ("retry-after", "0"),
                    ("x-ratelimit-reset-requests", "3s"),
                    ("x-ratelimit-reset-tokens", "9s"),
                ]),
                now
            ),
            Some(Duration::from_secs(9))
        );
    }
}
