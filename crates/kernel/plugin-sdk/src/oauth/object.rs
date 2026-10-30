//! A JSON object whose values may be secret: a stored account blob, or a
//! JSON token request body.

use std::fmt;

use hofund_plugin_api::PluginError;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};

/// A JSON object of named fields, some of them secret.
///
/// A subscription plugin keeps an account's tokens as one of these inside
/// the account's [`SecretString`], and builds JSON token requests with it.
/// `Debug` lists the field names only.
#[derive(Clone, Default)]
pub struct SecretObject {
    fields: Map<String, Value>,
}

impl SecretObject {
    /// An empty object.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads an object stored with [`Self::to_secret`].
    ///
    /// # Errors
    ///
    /// Returns an `Auth` [`PluginError`] when `secret` is not a JSON object,
    /// so a garbled blob fails closed. The text is not included.
    pub fn parse(secret: &SecretString) -> Result<Self, PluginError> {
        match serde_json::from_str::<Value>(secret.expose_secret()) {
            Ok(Value::Object(fields)) => Ok(Self { fields }),
            _ => Err(PluginError::auth("stored account secret is not readable")),
        }
    }

    /// The non-empty string field `key`.
    #[must_use]
    pub fn text(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }

    /// The non-empty string field `key`, as a secret.
    #[must_use]
    pub fn secret(&self, key: &str) -> Option<SecretString> {
        self.text(key)
            .map(|text| SecretString::from(text.to_owned()))
    }

    /// The unsigned integer field `key`.
    #[must_use]
    pub fn number(&self, key: &str) -> Option<u64> {
        self.fields.get(key).and_then(Value::as_u64)
    }

    /// Whether the boolean field `key` is `true`.
    #[must_use]
    pub fn flag(&self, key: &str) -> bool {
        self.fields.get(key).and_then(Value::as_bool) == Some(true)
    }

    /// Sets the string field `key`.
    #[must_use]
    pub fn with_text(mut self, key: &str, value: &str) -> Self {
        self.fields
            .insert(key.to_owned(), Value::String(value.to_owned()));
        self
    }

    /// Sets the string field `key` to a secret.
    #[must_use]
    pub fn with_secret(self, key: &str, value: &SecretString) -> Self {
        self.with_text(key, value.expose_secret())
    }

    /// Sets the integer field `key`.
    #[must_use]
    pub fn with_number(mut self, key: &str, value: u64) -> Self {
        self.fields.insert(key.to_owned(), Value::from(value));
        self
    }

    /// Sets the boolean field `key`.
    #[must_use]
    pub fn with_flag(mut self, key: &str, value: bool) -> Self {
        self.fields.insert(key.to_owned(), Value::Bool(value));
        self
    }

    /// Removes the field `key`.
    #[must_use]
    pub fn without(mut self, key: &str) -> Self {
        self.fields.remove(key);
        self
    }

    /// The object as JSON text in a secret, for an account store.
    #[must_use]
    pub fn to_secret(&self) -> SecretString {
        SecretString::from(Value::Object(self.fields.clone()).to_string())
    }

    /// The object as a JSON request body. It can contain a token. Do not log it.
    #[must_use]
    pub fn to_body(&self) -> Vec<u8> {
        Value::Object(self.fields.clone()).to_string().into_bytes()
    }
}

impl fmt::Debug for SecretObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretObject")
            .field("fields", &self.fields.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The text of `secret`, for a plugin that hands it to a vendor tool, such
/// as a code the user pasted. Do not log, format, or store the result.
#[must_use]
pub fn secret_text(secret: &SecretString) -> &str {
    secret.expose_secret()
}

/// Whether `a` and `b` hold the same text, in time that depends only on
/// their lengths.
#[must_use]
pub fn same_secret(a: &SecretString, b: &SecretString) -> bool {
    constant_time_eq(a.expose_secret().as_bytes(), b.expose_secret().as_bytes())
}

/// Whether `a` equals `b`, in time that depends only on their lengths.
///
/// Use it for a `state` value or a token, never `==`.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{PluginErrorKind, SecretString};
    use secrecy::ExposeSecret;

    use super::{SecretObject, constant_time_eq, same_secret, secret_text};

    fn secret(text: &str) -> SecretString {
        SecretString::from(text.to_owned())
    }

    #[test]
    fn secret_text_is_the_whole_secret() {
        assert_eq!(secret_text(&secret("code#state")), "code#state");
    }

    #[test]
    fn fields_round_trip_through_the_secret() {
        let object = SecretObject::new()
            .with_text("account_id", "acct-1")
            .with_secret("refresh_token", &secret("rt-SENTINEL"))
            .with_number("v", 1)
            .with_flag("lapsed", true);
        let back = SecretObject::parse(&object.to_secret()).expect("parse");
        assert_eq!(back.text("account_id"), Some("acct-1"));
        assert_eq!(
            back.secret("refresh_token")
                .expect("refresh")
                .expose_secret(),
            "rt-SENTINEL"
        );
        assert_eq!(back.number("v"), Some(1));
        assert!(back.flag("lapsed"));
        let body = String::from_utf8(back.to_body()).expect("utf8");
        assert!(body.contains(r#""refresh_token":"rt-SENTINEL""#), "{body}");
    }

    #[test]
    fn missing_empty_and_wrong_typed_fields_read_as_absent() {
        let object = SecretObject::parse(&secret(
            r#"{"empty":"","n":"7","flag":"true","text":3,"neg":-1}"#,
        ))
        .expect("parse");
        assert_eq!(object.text("empty"), None);
        assert!(object.secret("empty").is_none());
        assert_eq!(object.text("absent"), None);
        assert_eq!(object.text("text"), None);
        assert_eq!(object.number("n"), None);
        assert_eq!(object.number("neg"), None);
        assert!(!object.flag("flag"));
        assert!(!object.flag("absent"));
        assert!(!SecretObject::new().with_flag("f", false).flag("f"));
    }

    #[test]
    fn without_removes_one_field() {
        let object = SecretObject::new()
            .with_text("a", "1")
            .with_text("b", "2")
            .without("a");
        assert_eq!(object.text("a"), None);
        assert_eq!(object.text("b"), Some("2"));
    }

    #[test]
    fn garbled_secrets_fail_closed_as_auth_without_the_text() {
        for text in ["not json SENTINEL", "[1,2]", "\"SENTINEL\"", ""] {
            let error = SecretObject::parse(&secret(text)).expect_err(text);
            assert_eq!(error.kind(), PluginErrorKind::Auth);
            assert!(!error.message().contains("SENTINEL"));
        }
    }

    #[test]
    fn debug_lists_names_only() {
        let object = SecretObject::new().with_secret("access_token", &secret("at-SENTINEL"));
        let rendered = format!("{object:?}");
        assert!(rendered.contains("access_token"), "{rendered}");
        assert!(!rendered.contains("SENTINEL"), "{rendered}");
    }

    #[test]
    fn constant_time_eq_compares_every_byte_and_the_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"xbc"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(same_secret(&secret("t-1"), &secret("t-1")));
        assert!(!same_secret(&secret("t-1"), &secret("t-2")));
    }
}
