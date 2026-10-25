//! HTTP calls on the host's pooled client.
//!
//! This module builds requests and reads responses. It does not construct a
//! client. [`Call::send`] uses [`HttpHandle::send`](hofund_plugin_api::HttpHandle::send).
//! A status of 400 or higher becomes [`PluginErrorKind::Status`].
//! Connect and timeout failures are the only ones [`Call::send_with_retry`] repeats.

use std::fmt;

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
        }
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

    /// Sends the call once.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when a header is invalid, the host client fails,
    /// or the status is 400 or higher.
    pub async fn send(self, http: &HttpHandle) -> Result<Reply, PluginError> {
        let request = self.request()?;
        let response = http.send(request).await?;
        let (status, body) = read_body(response).await?;
        classify(status, body)
    }

    /// Sends the call up to `attempts` times, including the first try.
    ///
    /// The cap is three attempts. Only connect and timeout errors are retried.
    ///
    /// # Errors
    ///
    /// Returns the last [`PluginError`] when every attempt fails, or the first
    /// error that is not connect or timeout.
    pub async fn send_with_retry(
        self,
        http: &HttpHandle,
        attempts: u8,
    ) -> Result<Reply, PluginError> {
        let attempts = attempts.clamp(1, MAX_ATTEMPTS);
        let call = self;
        let mut attempt = 1_u8;
        loop {
            if attempt == attempts {
                return call.send(http).await;
            }
            match call.clone().send(http).await {
                Ok(reply) => return Ok(reply),
                Err(error) if retriable(error.kind()) => attempt += 1,
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
            .finish()
    }
}

fn header_value(header: &Header) -> Result<http::HeaderValue, PluginError> {
    let mut value = http::HeaderValue::from_str(&header.value)
        .map_err(|_| PluginError::auth(format!("{} is not a valid header value", header.name)))?;
    value.set_sensitive(header.sensitive);
    Ok(value)
}

fn retriable(kind: PluginErrorKind) -> bool {
    matches!(kind, PluginErrorKind::Connect | PluginErrorKind::Timeout)
}

fn classify(status: u16, body: Vec<u8>) -> Result<Reply, PluginError> {
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&body);
        let snippet = snippet.chars().take(STATUS_SNIPPET).collect::<String>();
        let message = if snippet.is_empty() {
            format!("upstream returned status {status}")
        } else {
            snippet
        };
        return Err(PluginError::status_error(status, message));
    }
    Ok(Reply { status, body })
}

async fn read_body(mut response: HttpResponse) -> Result<(u16, Vec<u8>), PluginError> {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await {
        body.extend_from_slice(&chunk?);
    }
    Ok((status, body))
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
    use hofund_plugin_api::{CredentialStyle, HeaderName, PluginErrorKind, SecretString};

    use super::{Call, classify, retriable, text_response};

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
    fn status_400_is_an_error_and_only_connect_and_timeout_retry() {
        let error = classify(401, b"nope".to_vec()).expect_err("status");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(401));
        assert_eq!(error.message(), "nope");
        let ok = classify(200, b"pong".to_vec()).expect("ok");
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.body(), b"pong");
        assert!(retriable(PluginErrorKind::Connect));
        assert!(retriable(PluginErrorKind::Timeout));
        assert!(!retriable(PluginErrorKind::Status));
        assert!(!retriable(PluginErrorKind::Auth));
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
