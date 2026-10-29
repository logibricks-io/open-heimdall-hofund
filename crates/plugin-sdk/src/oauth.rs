//! OAuth token responses, form bodies, device sign-in, and token caching.
//!
//! [`parse_access_token`] and [`refresh_form`] cover a provider that trades a
//! refresh token or a signed JWT. [`Pkce`], [`new_state`], and
//! [`authorize_url`] build a browser sign-in and [`code_exchange_form`]
//! trades its code; the kernel's loopback listener
//! checks the callback. [`device`] runs an RFC 8628 device sign-in.
//! [`TokenSet`], [`TokenCache`], [`token_error`], and [`account_id_from`] are
//! what a subscription plugin keeps per account. [`SecretObject`] stores an
//! account's tokens as JSON in its secret and builds JSON token requests,
//! [`same_secret`] and [`constant_time_eq`] compare tokens and `state`, and
//! [`detached`] runs a refresh that outlives its caller. Error text is a fixed
//! sentence or a standard error code. The response body is never copied into
//! it. Vertex keeps its own cache for now.

use std::fmt;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

mod account;
mod cache;
mod clock;
mod detached;
pub mod device;
mod object;
mod pkce;
mod token;

pub use account::account_id_from;
pub use cache::{NEGATIVE_TTL, TokenCache};
pub use clock::Clock;
pub use detached::detached;
pub use object::{SecretObject, constant_time_eq, same_secret};
pub use pkce::{Pkce, authorize_url, code_exchange_form, new_state};
pub use token::{TokenSet, parse_token_set, token_error, unverified_claims};

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
    let object = token_object(body)?;
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

/// An IAM Credentials access token and its optional absolute expiry.
/// `Debug` and `Display` redact the token.
pub struct IamAccessToken {
    access: AccessToken,
    expire_time: Option<String>,
}

impl IamAccessToken {
    /// The access token, with the one-hour fallback lifetime.
    #[must_use]
    pub const fn access_token(&self) -> &AccessToken {
        &self.access
    }

    /// The IAM `expireTime` value, when the response supplied a string.
    #[must_use]
    pub fn expire_time(&self) -> Option<&str> {
        self.expire_time.as_deref()
    }
}

impl fmt::Debug for IamAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IamAccessToken")
            .field("access", &self.access)
            .field("expire_time", &self.expire_time)
            .finish()
    }
}

impl fmt::Display for IamAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Reads `accessToken` and `expireTime` from an IAM Credentials
/// `generateAccessToken` JSON body with one parse.
///
/// IAM uses an RFC 3339 `expireTime`, not OAuth's `expires_in`. The returned
/// token has the same redaction and one-hour default lifetime as an OAuth token.
///
/// # Errors
///
/// Returns [`TokenError`] when the body is not a JSON object or has no non-empty
/// string `accessToken`. The body is not included in the error.
pub fn parse_iam_access_token(body: &[u8]) -> Result<IamAccessToken, TokenError> {
    let object = token_object(body)?;
    let token = object
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(TokenError::new(
            "IAM Credentials token response has no accessToken",
        ))?;
    let expire_time = object
        .get("expireTime")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(IamAccessToken {
        access: AccessToken {
            token: SecretString::from(token),
            expires_in: Duration::from_secs(DEFAULT_EXPIRES_SECS),
        },
        expire_time,
    })
}

fn token_object(body: &[u8]) -> Result<serde_json::Map<String, Value>, TokenError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| TokenError::new("token response is not JSON"))?;
    let Value::Object(object) = value else {
        return Err(TokenError::new("token response is not a JSON object"));
    };
    Ok(object)
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
    use super::{form_body, parse_access_token, parse_iam_access_token, refresh_form};

    #[test]
    fn an_iam_token_response_parses_and_redacts() {
        let token = parse_iam_access_token(
            br#"{"accessToken":"iam-SECRET-TOKEN","expireTime":"2026-10-25T08:00:00Z"}"#,
        )
        .expect("token");
        assert_eq!(token.access_token().expose_secret(), "iam-SECRET-TOKEN");
        assert_eq!(token.access_token().expires_in().as_secs(), 3600);
        assert_eq!(token.expire_time(), Some("2026-10-25T08:00:00Z"));
        let rendered = format!("{token:?} {token}");
        assert!(!rendered.contains("iam-SECRET-TOKEN"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn an_iam_token_response_requires_a_token() {
        let error = parse_iam_access_token(br#"{"expireTime":"2026-10-25T08:00:00Z"}"#)
            .expect_err("missing");
        assert_eq!(
            error.to_string(),
            "IAM Credentials token response has no accessToken"
        );
    }

    #[test]
    fn an_iam_token_response_rejects_an_empty_token() {
        let error = parse_iam_access_token(br#"{"accessToken":""}"#).expect_err("empty");
        assert_eq!(
            error.to_string(),
            "IAM Credentials token response has no accessToken"
        );
    }

    #[test]
    fn a_bad_iam_token_response_does_not_echo_the_body() {
        let error = parse_iam_access_token(b"not-json SECRET-IAM-BODY").expect_err("json");
        assert_eq!(error.to_string(), "token response is not JSON");
        assert!(!error.to_string().contains("SECRET-IAM-BODY"));
    }

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
