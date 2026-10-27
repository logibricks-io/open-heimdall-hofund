//! HTTP calls on the host's pooled client.
//!
//! This module builds requests and reads responses. It does not construct a
//! client. [`Call::send`] uses [`HttpHandle::send`](hofund_plugin_api::HttpHandle::send).
//! A status of 400 or higher becomes [`PluginErrorKind::Status`].
//!
//! The Chat, Responses, Messages, Generative, and Converse transports move here,
//! with streaming through `HttpHandle::send`, in the first row E PR that moves
//! an adapter into its own plugin crate (E3, Vertex).
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
        .map_err(|_| PluginError::auth(format!("{} is not a valid header value", header.name)))?;
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
        let error = PluginError::status_error(status, message);
        return Err(match retry_after {
            Some(after) => error.with_retry_after(after),
            None => error,
        });
    }
    Ok(Reply { status, body })
}

async fn read_body(
    mut response: HttpResponse,
) -> Result<(u16, Option<Duration>, Vec<u8>), PluginError> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, std::time::SystemTime::now()));
    let mut body = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await {
        body.extend_from_slice(&chunk?);
    }
    Ok((status, retry_after, body))
}

fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            let at = httpdate::parse_http_date(value).ok()?;
            at.duration_since(now).ok()
        })
        .filter(|after| !after.is_zero())
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

    use super::{Call, backoff, classify, parse_retry_after, retries, text_response};

    fn secret(value: &str) -> SecretString {
        SecretString::from(value)
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
}
