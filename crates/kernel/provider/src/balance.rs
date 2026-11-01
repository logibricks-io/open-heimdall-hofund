//! Key balance and key-plan readers for compatible upstream endpoints.

use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::error::ProviderError;
use crate::http::{Credential, Endpoint};

mod expr;

const BODY_LIMIT: usize = 1 << 20;

/// Settings for a provider-owned balance endpoint.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct BalanceOptions {
    /// URL to ask instead of the endpoint's known balance URL.
    pub url: String,
    /// Field or expression to read from the reply.
    pub path: String,
    /// Account token used instead of the key.
    pub token: String,
    /// GET or POST. Empty means GET.
    pub method: String,
}

impl std::fmt::Debug for BalanceOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BalanceOptions")
            .field("url", &self.url)
            .field("path", &self.path)
            .field("token", &"[redacted]")
            .field("method", &self.method)
            .finish()
    }
}

/// One amount shown beside a balance.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BalancePart {
    /// Optional amount label.
    pub label: String,
    /// Formatted amount.
    pub text: String,
    /// Percent represented by this amount, when requested.
    pub percent: Option<f64>,
    /// Why this one amount could not be read.
    pub error: String,
}

/// A numeric wallet amount available to routing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wallet {
    /// Wallet label.
    pub name: String,
    /// Amount left.
    pub amount: f64,
    /// Currency or provider unit.
    pub unit: String,
}

/// A formatted balance and its optional detail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Balance {
    /// Display text.
    pub text: String,
    /// Custom expression parts, when a card should show them separately.
    pub parts: Vec<BalancePart>,
    /// Granted and topped-up components.
    pub split: Vec<BalancePart>,
    /// Numeric amounts for routing.
    pub wallets: Vec<Wallet>,
}

/// One allowance window returned with a balance or plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BalanceWindow {
    /// Window name.
    pub name: String,
    /// Percent used.
    pub used: f64,
    /// Amount used.
    pub amount: f64,
    /// Window cap.
    pub limit: f64,
    /// Amount unit.
    pub unit: String,
    /// Formatted count, when one is supplied.
    pub display: String,
    /// Absolute reset.
    pub resets_at: Option<SystemTime>,
    /// Window length.
    pub span: Option<Duration>,
    /// Whether exhaustion stops this key when no credits remain.
    pub blocking: bool,
}

/// A key's bought plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BalancePlan {
    /// Plan display name.
    pub name: String,
    /// Plan windows.
    pub windows: Vec<BalanceWindow>,
    /// End of the current paid term.
    pub until: Option<SystemTime>,
    /// Renewal mode, when known.
    pub renew: String,
}

/// Everything one balance endpoint tells about a key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BalanceReport {
    /// Monetary balance, absent when the endpoint has none for this key.
    pub balance: Option<Balance>,
    /// Bought plan, absent when the endpoint has none for this key.
    pub plan: Option<BalancePlan>,
    /// Key-specific windows on the balance card.
    pub windows: Vec<BalanceWindow>,
    /// Whether every key of one account sees this same balance.
    pub account_wide: bool,
}

/// A balance reader bound to one endpoint and key.
pub struct BalanceReader {
    endpoint: Endpoint,
    options: BalanceOptions,
    client: reqwest::Client,
}

#[derive(Clone)]
struct Source {
    kind: SourceKind,
    url: String,
    token: String,
}

#[derive(Clone)]
enum SourceKind {
    Custom,
}

impl BalanceReader {
    /// Builds a reader. Credentials are taken from `endpoint`; balance settings
    /// are already resolved by the caller.
    pub fn new(endpoint: Endpoint, options: BalanceOptions) -> Result<Self, ProviderError> {
        let client = match endpoint.client.clone() {
            Some(client) => client,
            None => reqwest::Client::builder()
                .connect_timeout(endpoint.timeout)
                .read_timeout(crate::http::socket_backstop(endpoint.timeout))
                .build()
                .map_err(|error| ProviderError::connect(error.to_string()))?,
        };
        Ok(Self {
            endpoint,
            options,
            client,
        })
    }

    /// Reads the key's balance and any plan carried by the same endpoint.
    /// `Ok(None)` means this endpoint has no balance facility or no card for
    /// this key.
    pub async fn read(&self) -> Result<Option<BalanceReport>, ProviderError> {
        if self.endpoint.api_key.is_empty() || self.options.url.trim().is_empty() {
            return Ok(None);
        }
        let Some(source) = self.source() else {
            return Ok(None);
        };
        if !source.token.is_empty() && self.custom() {
            match url_path(&self.options.url).as_str() {
                "/api/usage/token" => {
                    return Err(ProviderError::config(key_token_error(
                        &self.options.url,
                        "/api/user/self",
                        "$data.quota / 500000",
                        "access token",
                    )));
                }
                "/v1/usage" => {
                    return Err(ProviderError::config(key_token_error(
                        &self.options.url,
                        "/api/v1/user/profile",
                        "$data.balance",
                        "login token",
                    )));
                }
                _ => {}
            }
        }
        let method = if self.custom() {
            balance_method(&self.options.method)?
        } else {
            reqwest::Method::GET
        };
        let url = with_key(&source.url, &self.endpoint.api_key, true);
        let (status, body) = self.send(&source, method, &url).await?;
        if let Some(message) = balance_refusal(&body) {
            let message = user_header_hint(&message);
            return Err(if status >= 300 {
                ProviderError::status(status, message)
            } else {
                ProviderError::decode(message)
            });
        }
        if status >= 300 {
            return Err(status_error(status, &body));
        }
        self.parse(source, &body).map(Some)
    }

    fn custom(&self) -> bool {
        !self.options.url.trim().is_empty()
    }

    fn source(&self) -> Option<Source> {
        let options = self.effective_options();
        if options.url.trim().is_empty() {
            return None;
        }
        Some(Source {
            kind: SourceKind::Custom,
            url: options.url,
            token: options.token,
        })
    }

    fn effective_options(&self) -> BalanceOptions {
        let mut options = self.options.clone();
        let own_key = options
            .token
            .trim()
            .strip_prefix("Bearer ")
            .or_else(|| options.token.trim().strip_prefix("bearer "))
            .unwrap_or(options.token.trim());
        if !own_key.is_empty() && own_key == self.endpoint.api_key.trim() {
            options.token.clear();
        }
        if options.url.trim().is_empty() || !options.token.trim().is_empty() {
            return options;
        }
        if self.endpoint.headers.iter().any(|(name, value)| {
            !value.trim().is_empty()
                && (name.eq_ignore_ascii_case("authorization")
                    || name.eq_ignore_ascii_case("cookie"))
        }) {
            return options;
        }
        let Ok(mut url) = reqwest::Url::parse(options.url.trim()) else {
            return options;
        };
        if url.host_str().is_none() {
            return options;
        }
        let path = url.path().trim_end_matches('/').to_owned();
        let (account, key, field) = match path.as_str() {
            value if value.ends_with("/api/v1/user/profile") => {
                ("/api/v1/user/profile", "/v1/usage", "$data.balance")
            }
            value if value.ends_with("/api/user/self") => {
                ("/api/user/self", "/api/usage/token", "$data.quota / 500000")
            }
            _ => return options,
        };
        let prefix = path.strip_suffix(account).unwrap_or_default();
        url.set_path(&format!("{prefix}{key}"));
        url.set_query(None);
        url.set_fragment(None);
        options.url = url.to_string();
        if options.path.trim() == field {
            options.path.clear();
        }
        options
    }

    async fn send(
        &self,
        source: &Source,
        method: reqwest::Method,
        url: &str,
    ) -> Result<(u16, Vec<u8>), ProviderError> {
        let mut headers = HeaderMap::new();
        if source.token.is_empty() {
            crate::http::insert_credential(
                &mut headers,
                &self.endpoint.api_key,
                self.endpoint.credential,
                Credential::Bearer,
            )?;
        }
        if source.token.is_empty() || self.custom() {
            for (name, value) in &self.endpoint.headers {
                let name = HeaderName::try_from(name.as_str())
                    .map_err(|_| ProviderError::config("invalid balance header name"))?;
                let mut value =
                    HeaderValue::try_from(with_key(value, &self.endpoint.api_key, false))
                        .map_err(|_| ProviderError::config("invalid balance header value"))?;
                value.set_sensitive(true);
                headers.insert(name, value);
            }
        }
        if !source.token.is_empty() {
            let mut value = HeaderValue::try_from(balance_authorization(&source.token))
                .map_err(|_| ProviderError::config("balance token is not valid header text"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self
            .client
            .request(method, url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| {
                ProviderError::connect(redact_error(&error.to_string(), &self.endpoint.api_key))
            })?;
        let status = response.status().as_u16();
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                ProviderError::interrupted(redact_error(&error.to_string(), &self.endpoint.api_key))
            })?;
            let left = BODY_LIMIT.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(left)]);
            if body.len() == BODY_LIMIT {
                break;
            }
        }
        Ok((status, body))
    }

    fn parse(&self, source: Source, body: &[u8]) -> Result<BalanceReport, ProviderError> {
        let _ = source.kind;
        self.parse_custom(body)
    }

    fn parse_custom(&self, body: &[u8]) -> Result<BalanceReport, ProviderError> {
        let options = self.effective_options();
        let windows = if url_path(&options.url) == "/v1/usage" {
            panel_windows(body)
        } else {
            Vec::new()
        };
        let account_wide = !options.token.is_empty()
            || (url_path(&options.url) == "/v1/usage"
                && json(body)
                    .ok()
                    .and_then(|reply| reply.get("mode").and_then(Value::as_str).map(str::to_owned))
                    .as_deref()
                    == Some("unrestricted"));
        let path = balance_path(&options);
        match expr::read(body, &path) {
            Ok((text, mut parts, wallets)) => {
                if !(parts.len() > 1 || parts.first().is_some_and(|part| part.percent.is_some())) {
                    parts.clear();
                }
                Ok(BalanceReport {
                    balance: Some(Balance {
                        text,
                        parts,
                        split: Vec::new(),
                        wallets,
                    }),
                    plan: None,
                    windows,
                    account_wide,
                })
            }
            Err(error) if !windows.is_empty() && options.path.trim().is_empty() => {
                let _ = error;
                Ok(BalanceReport {
                    balance: None,
                    plan: None,
                    windows,
                    account_wide: false,
                })
            }
            Err(error) => Err(error),
        }
    }
}

fn panel_windows(body: &[u8]) -> Vec<BalanceWindow> {
    let Ok(reply) = json(body) else {
        return Vec::new();
    };
    let Some(limits) = reply.get("rate_limits").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, name, seconds) in [
        ("5h", "5 hours", 5 * 3600),
        ("1d", "1 day", 24 * 3600),
        ("7d", "7 days", 7 * 24 * 3600),
    ] {
        for limit in limits {
            if limit.get("window").and_then(Value::as_str).map(str::trim) != Some(id) {
                continue;
            }
            let Some(cap) = limit
                .get("limit")
                .and_then(Value::as_f64)
                .filter(|cap| *cap > 0.0)
            else {
                continue;
            };
            let used = limit
                .get("used")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0);
            out.push(BalanceWindow {
                name: name.to_owned(),
                used: (100.0 * used / cap).min(100.0),
                amount: used,
                limit: cap,
                unit: "USD".to_owned(),
                display: String::new(),
                resets_at: limit.get("reset_at").and_then(timestamp),
                span: Some(Duration::from_secs(seconds)),
                blocking: true,
            });
            break;
        }
    }
    out
}

fn balance_path(options: &BalanceOptions) -> String {
    if !options.path.trim().is_empty() {
        return options.path.clone();
    }
    match url_path(&options.url) {
        path if path == "/api/user/self" => "$data.quota / 500000".to_owned(),
        path if path == "/api/usage/token" => "$data.total_available / 500000".to_owned(),
        path if path == "/api/v1/user/profile" => "$data.balance".to_owned(),
        path if path == "/v1/usage" => "$remaining".to_owned(),
        path if path.ends_with("/dashboard/billing/credit_grants") => "$total_available".to_owned(),
        _ => options.path.clone(),
    }
}

fn balance_method(method: &str) -> Result<reqwest::Method, ProviderError> {
    match method.trim().to_uppercase().as_str() {
        "" | "GET" => Ok(reqwest::Method::GET),
        "POST" => Ok(reqwest::Method::POST),
        _ => Err(ProviderError::config(format!(
            "the balance method {method:?} is neither GET nor POST"
        ))),
    }
}

fn balance_authorization(token: &str) -> String {
    let token = token.trim();
    if token.contains(' ') {
        return token.to_owned();
    }
    if token.starts_with("eyJ") && token.matches('.').count() == 2 {
        return format!("Bearer {token}");
    }
    token.to_owned()
}

fn key_token_error(raw: &str, account_path: &str, field: &str, token: &str) -> String {
    let account = reqwest::Url::parse(raw.trim())
        .ok()
        .filter(|url| url.host_str().is_some())
        .map(|url| {
            format!(
                "{}://{}{account_path}",
                url.scheme(),
                url.host_str().unwrap_or_default()
            )
        })
        .unwrap_or_else(|| account_path.to_owned());
    format!(
        "the Balance URL {} takes the API key, not the {token}: set it to {account} (Balance field {field}) for the account's balance, or remove the token for the key's own",
        raw.trim()
    )
}

fn with_key(text: &str, key: &str, in_url: bool) -> String {
    let replacement = if in_url {
        percent_encode(key)
    } else {
        key.to_owned()
    };
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let Some(end) = candidate.find('}') else {
            out.push_str(candidate);
            return out;
        };
        let token = candidate[..=end].trim_start_matches('$');
        let normalized = token
            .trim_matches(|character| matches!(character, '{' | '}' | ' '))
            .to_lowercase();
        if matches!(
            normalized.as_str(),
            "key" | "apikey" | "api_key" | "api-key" | "token"
        ) {
            out.push_str(&replacement);
        } else {
            out.push_str(&candidate[..=end]);
        }
        rest = &candidate[end + 1..];
    }
    out.push_str(rest);
    out
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn redact_error(error: &str, key: &str) -> String {
    if key.is_empty() {
        return error.to_owned();
    }
    let mask = if key.chars().count() >= 12 {
        let suffix: String = key.chars().skip(key.chars().count() - 4).collect();
        format!("…{suffix}")
    } else {
        "…".to_owned()
    };
    error
        .replace(key, &mask)
        .replace(&percent_encode(key), &mask)
}

fn url_path(raw: &str) -> String {
    reqwest::Url::parse(raw.trim())
        .map(|url| url.path().trim_end_matches('/').to_owned())
        .unwrap_or_default()
}

fn balance_refusal(body: &[u8]) -> Option<String> {
    let reply = json(body).ok()?;
    if reply.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    reply
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

fn user_header_hint(message: &str) -> String {
    if !message.to_lowercase().contains("new-api-user") {
        return message.to_owned();
    }
    format!(
        "add the header New-Api-User = your user ID (shown in the site's personal settings) to this provider's Headers; the relay said: {message}"
    )
}

fn status_error(status: u16, body: &[u8]) -> ProviderError {
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or_default();
    let prefix = format!("{status} {reason}").trim().to_owned();
    let message = String::from_utf8_lossy(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !message.starts_with('{') {
        return ProviderError::status(status, prefix);
    }
    let mut chars = message.chars();
    let shortened = chars.by_ref().take(200).collect::<String>();
    let suffix = if chars.next().is_some() { "…" } else { "" };
    ProviderError::status(status, format!("{prefix}: {shortened}{suffix}"))
}

fn json(body: &[u8]) -> Result<Value, ProviderError> {
    serde_json::from_slice(body).map_err(|error| ProviderError::decode(error.to_string()))
}

fn timestamp(value: &Value) -> Option<SystemTime> {
    match value {
        Value::String(text) => {
            if let Some(at) = hofund_plugin_sdk::time::parse_rfc3339(text) {
                return Some(at);
            }
            text.parse::<f64>().ok().and_then(epoch_time)
        }
        Value::Number(number) => number.as_f64().and_then(epoch_time),
        _ => None,
    }
}

fn epoch_time(value: f64) -> Option<SystemTime> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = if value < 1e12 { value } else { value / 1000.0 };
    if seconds > u64::MAX as f64 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(seconds))
}

fn money(sign: &str, value: f64) -> String {
    format!("{sign}{value:.2}")
}

fn currency_unit(sign: &str) -> &str {
    match sign {
        "$" => "USD",
        "¥" => "CNY",
        "€" => "EUR",
        "£" => "GBP",
        _ => "",
    }
}

#[cfg(test)]
mod tests;
