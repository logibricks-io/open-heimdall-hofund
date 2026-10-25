//! OAuth token responses and form bodies.
//!
//! This is the piece a provider needs when it trades a refresh token or a
//! signed JWT. PKCE, the loopback callback, and device-flow polling are not
//! here. Error text is a fixed sentence. The response body is never copied
//! into it.

use std::fmt;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

const DEFAULT_EXPIRES_SECS: u64 = 3600;
const MAX_EXPIRES_SECS: u64 = u64::MAX / 1_000_000_000;

/// An access token from a token endpoint. `Debug` and `Display` redact it.
pub struct AccessToken {
    token: SecretString,
    expires_in: Duration,
}

impl AccessToken {
    /// How long the token endpoint said the token stays valid.
    ///
    /// A missing, non-positive, or oversized `expires_in` becomes one hour,
    /// capped so the duration cannot overflow.
    #[must_use]
    pub const fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// The token text. Do not log, format, or store this in a `Debug` field.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.token.expose_secret()
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessToken")
            .field("token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Why a token response could not be read. The message is a fixed sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenError {
    message: &'static str,
}

impl TokenError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }

    /// The fixed sentence.
    #[must_use]
    pub const fn message(self) -> &'static str {
        self.message
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for TokenError {}

/// Reads `access_token` and `expires_in` from a token JSON body.
///
/// # Errors
///
/// Returns [`TokenError`] when the body is not a JSON object or has no
/// string `access_token`. The body is not included in the error.
pub fn parse_access_token(body: &[u8]) -> Result<AccessToken, TokenError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| TokenError::new("token response is not JSON"))?;
    let Some(object) = value.as_object() else {
        return Err(TokenError::new("token response is not a JSON object"));
    };
    let token = object
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(TokenError::new("token response has no access_token"))?;
    let expires_in = object
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map_or(DEFAULT_EXPIRES_SECS, |seconds| {
            seconds.min(MAX_EXPIRES_SECS)
        });
    Ok(AccessToken {
        token: SecretString::from(token),
        expires_in: Duration::from_secs(expires_in),
    })
}

/// `application/x-www-form-urlencoded` for `pairs`.
///
/// Encoding follows the unreserved set in RFC 6749. The result can contain
/// a refresh token or a client secret. Do not log it.
#[must_use]
pub fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", form_escape(name), form_escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A refresh-token grant. The result can contain the refresh token. Do not log it.
#[must_use]
pub fn refresh_form(client_id: &str, client_secret: &str, refresh_token: &str) -> String {
    form_body(&[
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
    ])
}

fn form_escape(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{form_body, parse_access_token, refresh_form};

    #[test]
    fn a_token_response_parses_and_redacts() {
        let body =
            br#"{"access_token":"ya29-SECRET-TOKEN","expires_in":120,"token_type":"Bearer"}"#;
        let token = parse_access_token(body).expect("token");
        assert_eq!(token.expose_secret(), "ya29-SECRET-TOKEN");
        assert_eq!(token.expires_in().as_secs(), 120);
        let rendered = format!("{token:?} {token}");
        assert!(!rendered.contains("ya29-SECRET-TOKEN"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_bad_token_response_does_not_echo_the_body() {
        let body = br"not-json SECRET-BODY-SENTINEL";
        let error = parse_access_token(body).expect_err("json");
        assert_eq!(error.to_string(), "token response is not JSON");
        assert!(!error.to_string().contains("SECRET-BODY-SENTINEL"));
        let missing = parse_access_token(br#"{"expires_in":"SECRET-NESTED"}"#).expect_err("token");
        assert_eq!(missing.to_string(), "token response has no access_token");
        assert!(!missing.to_string().contains("SECRET-NESTED"));
    }

    #[test]
    fn refresh_form_encodes_the_grant() {
        let body = refresh_form("client id", "secret/value", "refresh token");
        assert!(body.starts_with("grant_type=refresh_token&"));
        assert!(body.contains("client_id=client%20id"));
        assert!(body.contains("client_secret=secret%2Fvalue"));
        assert!(body.contains("refresh_token=refresh%20token"));
        assert_eq!(
            form_body(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")]),
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
        );
    }
}
