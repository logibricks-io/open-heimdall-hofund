//! Token sets from a token endpoint, and token endpoint errors.

use std::fmt;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::PluginError;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};

use super::{TokenError, token_object};

/// The longest `expires_in` honoured: one day. A longer one is read as a day.
const MAX_LIFETIME_SECS: u64 = 86_400;

/// What a token endpoint returned: the access token and, when present, the
/// refresh token, the ID token, and when the access token expires.
///
/// `Debug` shows which parts are present, never their values.
#[derive(Clone)]
pub struct TokenSet {
    access: SecretString,
    refresh: Option<SecretString>,
    id_token: Option<SecretString>,
    expires_at: Option<SystemTime>,
}

impl TokenSet {
    /// The access token.
    #[must_use]
    pub fn access(&self) -> &SecretString {
        &self.access
    }

    /// The refresh token, when the endpoint returned one.
    #[must_use]
    pub fn refresh(&self) -> Option<&SecretString> {
        self.refresh.as_ref()
    }

    /// The OpenID Connect ID token, when the endpoint returned one.
    #[must_use]
    pub fn id_token(&self) -> Option<&SecretString> {
        self.id_token.as_ref()
    }

    /// When the access token expires. `None` when the endpoint gave no
    /// `expires_in`.
    #[must_use]
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }

    /// Keeps `previous` as the refresh token when this response had none.
    ///
    /// Many endpoints only return a new refresh token when they rotate it.
    #[must_use]
    pub fn or_refresh(mut self, previous: Option<&SecretString>) -> Self {
        if self.refresh.is_none() {
            self.refresh = previous.cloned();
        }
        self
    }
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access", &"[redacted]")
            .field("refresh", &self.refresh.as_ref().map(|_| "[redacted]"))
            .field("id_token", &self.id_token.as_ref().map(|_| "[redacted]"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Reads a token endpoint body.
///
/// `expires_at` is `now + expires_in`, with `expires_in` capped at one day.
/// A zero, negative, or missing `expires_in` gives `None`.
///
/// # Errors
///
/// Returns [`TokenError`] when the body is not a JSON object or has no
/// non-empty string `access_token`. The body is not included in the error.
pub fn parse_token_set(body: &[u8], now: SystemTime) -> Result<TokenSet, TokenError> {
    let object = token_object(body)?;
    let access = non_empty(&object, "access_token")
        .ok_or(TokenError::new("token response has no access_token"))?;
    let expires_at = object
        .get("expires_in")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|seconds| *seconds > 0)
        .map(|seconds| now + Duration::from_secs(seconds.min(MAX_LIFETIME_SECS)));
    Ok(TokenSet {
        access: SecretString::from(access),
        refresh: non_empty(&object, "refresh_token").map(SecretString::from),
        id_token: non_empty(&object, "id_token").map(SecretString::from),
        expires_at,
    })
}

fn non_empty<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// The claims of a JWT, read without checking its signature.
///
/// Use it only for facts the token endpoint just handed over TLS, such as
/// the subject or email in an ID token. Never use it to decide trust.
///
/// # Errors
///
/// Returns [`TokenError`] when the token does not have three dot-separated
/// parts, or its middle part is not base64url JSON object text.
pub fn unverified_claims(token: &SecretString) -> Result<Map<String, Value>, TokenError> {
    let mut parts = token.expose_secret().split('.');
    let (Some(_), Some(claims), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::new("token is not a JWT"));
    };
    let bytes =
        base64url_decode(claims).ok_or(TokenError::new("token claims are not base64url"))?;
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(TokenError::new("token claims are not a JSON object")),
    }
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    // One leftover character cannot encode a byte.
    (bits < 6).then_some(out)
}

/// Error codes from RFC 6749 section 5.2 and RFC 8628 section 3.5. Only these
/// are copied into an error message; anything else is not.
const KNOWN_CODES: [&str; 10] = [
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
    "expired_token",
    "authorization_pending",
    "slow_down",
];

/// Codes that mean the user has to sign in again.
const AUTH_CODES: [&str; 5] = [
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "access_denied",
    "expired_token",
];

/// The standard `error` code in a token endpoint body, if it is one of the
/// codes this module knows.
pub(crate) fn error_code(body: &[u8]) -> Option<&'static str> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let code = value.get("error")?.as_str()?;
    KNOWN_CODES.into_iter().find(|known| *known == code)
}

/// A [`PluginError`] for a token endpoint that refused a request.
///
/// `invalid_grant`, `invalid_client`, `unauthorized_client`,
/// `access_denied`, `expired_token`, HTTP 400, and HTTP 401 give `Auth`: the
/// user has to sign in again. HTTP 429 gives `Quota`. Anything else is a
/// status error. The message names the status and, when it is a standard
/// code, the code. `error_description` and the rest of the body are never
/// copied.
#[must_use]
pub fn token_error(status: u16, body: &[u8]) -> PluginError {
    let code = error_code(body);
    let message = match code {
        Some(code) => format!("token endpoint refused the request: HTTP {status}, {code}"),
        None => format!("token endpoint refused the request: HTTP {status}"),
    };
    if code.is_some_and(|code| AUTH_CODES.contains(&code)) || matches!(status, 400 | 401) {
        PluginError::auth(message)
    } else if status == 429 {
        PluginError::quota_status(status, message)
    } else {
        PluginError::status_error(status, message)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use hofund_plugin_api::PluginErrorKind;
    use secrecy::{ExposeSecret, SecretString};

    use super::{parse_token_set, token_error, unverified_claims};

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)
    }

    #[test]
    fn token_set_reads_every_part() {
        let set = parse_token_set(
            br#"{"access_token":"acc","refresh_token":"ref","id_token":"idt","expires_in":3600,"token_type":"Bearer"}"#,
            at(),
        )
        .expect("token set");
        assert_eq!(set.access().expose_secret(), "acc");
        assert_eq!(set.refresh().map(ExposeSecret::expose_secret), Some("ref"));
        assert_eq!(set.id_token().map(ExposeSecret::expose_secret), Some("idt"));
        assert_eq!(set.expires_at(), Some(at() + Duration::from_secs(3600)));
    }

    #[test]
    fn token_set_expiry_rules() {
        let read = |body: &str| {
            parse_token_set(body.as_bytes(), at())
                .expect("set")
                .expires_at()
        };
        assert_eq!(read(r#"{"access_token":"a"}"#), None);
        assert_eq!(read(r#"{"access_token":"a","expires_in":0}"#), None);
        assert_eq!(read(r#"{"access_token":"a","expires_in":-5}"#), None);
        assert_eq!(
            read(r#"{"access_token":"a","expires_in":"60"}"#),
            Some(at() + Duration::from_secs(60))
        );
        assert_eq!(
            read(r#"{"access_token":"a","expires_in":99999999}"#),
            Some(at() + Duration::from_secs(86_400))
        );
    }

    #[test]
    fn token_set_rejects_missing_access_without_echo() {
        for body in [
            r#"{"refresh_token":"sentinel-secret"}"#,
            r#"{"access_token":"","refresh_token":"sentinel-secret"}"#,
            "sentinel-secret",
            r#"["sentinel-secret"]"#,
        ] {
            let error = parse_token_set(body.as_bytes(), at()).expect_err(body);
            assert!(!error.to_string().contains("sentinel"), "{error}");
        }
    }

    #[test]
    fn token_set_debug_and_refresh_fallback() {
        let set = parse_token_set(br#"{"access_token":"sentinel-a"}"#, at()).expect("set");
        assert_eq!(set.refresh().map(ExposeSecret::expose_secret), None);
        let kept = set.or_refresh(Some(&SecretString::from("sentinel-r")));
        assert_eq!(
            kept.refresh().map(ExposeSecret::expose_secret),
            Some("sentinel-r")
        );
        let debug = format!("{kept:?}");
        assert!(!debug.contains("sentinel"), "{debug}");
        assert!(debug.contains("refresh: Some"), "{debug}");
        let rotated = parse_token_set(br#"{"access_token":"a","refresh_token":"new"}"#, at())
            .expect("set")
            .or_refresh(Some(&SecretString::from("old")));
        assert_eq!(
            rotated.refresh().map(ExposeSecret::expose_secret),
            Some("new")
        );
    }

    #[test]
    fn unverified_claims_reads_the_middle_part() {
        // {"sub":"u1","email":"me@example.com"} without padding, then with it.
        let token = SecretString::from(
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1MSIsImVtYWlsIjoibWVAZXhhbXBsZS5jb20ifQ.sig",
        );
        let claims = unverified_claims(&token).expect("claims");
        assert_eq!(claims["sub"], "u1");
        assert_eq!(claims["email"], "me@example.com");
        // {"a":1} is eyJhIjoxfQ, with padding eyJhIjoxfQ==.
        let padded = SecretString::from("h.eyJhIjoxfQ==.s");
        assert_eq!(unverified_claims(&padded).expect("claims")["a"], 1);
        // '-' and '_' are 62 and 63: the bytes 0xfb 0xff encode as -_8.
        assert_eq!(super::base64url_decode("-_8"), Some(vec![0xfb, 0xff]));
        // Five characters leave six bits over, which cannot be a byte.
        assert_eq!(super::base64url_decode("eyJhI"), None);
        assert_eq!(super::base64url_decode("eyJh"), Some(b"{\"a".to_vec()));
    }

    #[test]
    fn unverified_claims_rejects_without_echo() {
        for raw in [
            "sentinel",
            "a.b",
            "a.b.c.d",
            "h.c2VudGluZWw.s",
            "h.!!!.s",
            "h.eyJhIjoxfQx.s",
            "h.WyJzZW50aW5lbCJd.s",
        ] {
            let error = unverified_claims(&SecretString::from(raw)).expect_err(raw);
            assert!(!error.to_string().contains("sentinel"), "{error}");
        }
    }

    #[test]
    fn token_error_maps_invalid_grant_to_auth() {
        let auth = [
            (400, r#"{"error":"invalid_grant"}"#),
            (500, r#"{"error":"invalid_grant"}"#),
            (500, r#"{"error":"invalid_client"}"#),
            (403, r#"{"error":"unauthorized_client"}"#),
            (403, r#"{"error":"access_denied"}"#),
            (403, r#"{"error":"expired_token"}"#),
            (400, "not json"),
            (401, ""),
        ];
        for (status, body) in auth {
            let error = token_error(status, body.as_bytes());
            assert_eq!(error.kind(), PluginErrorKind::Auth, "{status} {body}");
        }
        let quota = token_error(429, b"{}");
        assert_eq!(quota.kind(), PluginErrorKind::Quota);
        assert_eq!(quota.status(), Some(429));
        let server = token_error(503, br#"{"error":"temporarily_unavailable"}"#);
        assert_eq!(server.kind(), PluginErrorKind::Status);
        assert_eq!(server.status(), Some(503));
        let forbidden = token_error(403, br#"{"error":"invalid_scope"}"#);
        assert_eq!(forbidden.kind(), PluginErrorKind::Status);
        assert_eq!(
            forbidden.message(),
            "token endpoint refused the request: HTTP 403, invalid_scope"
        );
    }

    #[test]
    fn token_error_never_echoes_description() {
        let body = br#"{"error":"invalid_grant","error_description":"sentinel token abc","error_uri":"https://sentinel.example"}"#;
        let error = token_error(400, body);
        assert_eq!(
            error.message(),
            "token endpoint refused the request: HTTP 400, invalid_grant"
        );
        let unknown = token_error(400, br#"{"error":"sentinel_code"}"#);
        assert_eq!(
            unknown.message(),
            "token endpoint refused the request: HTTP 400"
        );
        assert!(!format!("{unknown:?}").contains("sentinel"));
    }
}
