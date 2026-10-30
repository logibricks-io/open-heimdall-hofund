use serde::de::DeserializeOwned;
use serde_json::error::Category;

use crate::{
    API_VERSION, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentModelRequest, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse,
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    ControlError, ControlErrorKind, ErrorEnvelope, EventEnvelope, FieldIssue, HealthResponse,
    MAX_REQUEST_BODY_BYTES, MAX_RESPONSE_BODY_BYTES, ModelListResponse, PicksResponse,
    PicksSetRequest, ProviderListResponse, ProviderTestRequest, ProviderTestResponse,
    ReloadRequest, RouteModelListResponse, ShutdownRequest, ShutdownResponse, StatusResponse,
    TokenRotateRequest, TokenRotateResponse, UsageResponse,
};

mod sealed {
    pub trait Sealed {}
}

/// A versioned JSON request body accepted by [`decode_request`].
pub trait Request: DeserializeOwned + sealed::Sealed {
    /// Returns the body version.
    fn version(&self) -> u32;

    /// Performs semantic validation after shape and version checks.
    fn validate(&self) -> Result<(), ControlError> {
        Ok(())
    }
}

/// A versioned JSON response or event body accepted by [`decode_response`].
pub trait Response: DeserializeOwned + sealed::Sealed {
    /// Returns the body version.
    fn version(&self) -> u32;

    /// Performs bounded-content validation after compatibility checks.
    fn validate(&self) -> Result<(), ControlError> {
        Ok(())
    }
}

/// Decodes and validates a bounded request body.
///
/// Checks occur in this order: body byte limit, path version, strict JSON
/// shape, body version, then semantic validation. For unknown, missing, or
/// duplicate fields, a client-chosen field name of 1-64 bytes matching
/// `[A-Za-z0-9_]` is echoed in the error message and `FieldIssue.field`. Thus
/// a field name that is itself a short identifier, such as `sk_live_abc`, is
/// echoed. Longer or non-identifier keys fall back to `body`. Body values are
/// never included in the message or details.
pub fn decode_request<T: Request>(path_version: u32, body: &[u8]) -> Result<T, ControlError> {
    check_length(body, MAX_REQUEST_BODY_BYTES)?;
    check_path_version(path_version)?;
    let request =
        serde_json::from_slice::<T>(body).map_err(|error| malformed_json("request", error))?;
    check_body_version(request.version())?;
    request.validate()?;
    Ok(request)
}

/// Decodes a bounded response or event body, accepting additive unknown fields.
/// Decode errors use the same value-free representation as request errors.
pub fn decode_response<T: Response>(path_version: u32, body: &[u8]) -> Result<T, ControlError> {
    check_length(body, MAX_RESPONSE_BODY_BYTES)?;
    check_path_version(path_version)?;
    let response =
        serde_json::from_slice::<T>(body).map_err(|error| malformed_json("response", error))?;
    check_body_version(response.version())?;
    response.validate()?;
    Ok(response)
}

/// Decodes an error envelope with response compatibility rules.
pub fn decode_error_envelope(
    path_version: u32,
    body: &[u8],
) -> Result<ErrorEnvelope, ControlError> {
    decode_response(path_version, body)
}

fn malformed_json(surface: &str, error: serde_json::Error) -> ControlError {
    let raw_message = error.to_string();
    let (category, reason, field) = match error.classify() {
        Category::Syntax => ("syntax", "syntax", None),
        Category::Eof => ("eof", "eof", None),
        Category::Io => ("io", "data", None),
        Category::Data => {
            let (reason, field) = data_error_shape(&raw_message);
            ("data", reason, field)
        }
    };
    let mut message = format!("malformed {surface} JSON: {category} error");
    if let Some(field) = &field {
        message.push_str(&format!(" ({reason} `{field}`)"));
    }
    message.push_str(&format!(
        " at line {} column {}",
        error.line(),
        error.column()
    ));
    let mut control = ControlError::new(ControlErrorKind::BadRequest, &message, "decode");
    control.details.push(FieldIssue::new(
        field.as_deref().unwrap_or("body"),
        reason,
        &message,
    ));
    control
}

fn data_error_shape(message: &str) -> (&'static str, Option<String>) {
    for (prefix, reason) in [
        ("unknown field `", "unknown_field"),
        ("missing field `", "missing_field"),
        ("duplicate field `", "duplicate_field"),
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let field = rest
                .split('`')
                .next()
                .filter(|field| valid_error_field(field))
                .map(str::to_owned);
            return (reason, field);
        }
    }
    for (prefix, reason) in [
        ("invalid type:", "invalid_type"),
        ("invalid value:", "invalid_value"),
        ("unknown variant `", "unknown_variant"),
        ("invalid length", "invalid_length"),
    ] {
        if message.starts_with(prefix) {
            return (reason, None);
        }
    }
    ("data", None)
}

fn valid_error_field(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= 64
        && field
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn check_length(body: &[u8], limit: usize) -> Result<(), ControlError> {
    if body.len() > limit {
        return Err(ControlError::new(
            ControlErrorKind::BadRequest,
            format!("body exceeds the {limit}-byte limit"),
            "decode",
        ));
    }
    Ok(())
}

fn check_path_version(path_version: u32) -> Result<(), ControlError> {
    if path_version == API_VERSION {
        return Ok(());
    }
    Err(ControlError::new(
        ControlErrorKind::UnsupportedVersion,
        format!("unsupported path version {path_version}; expected {API_VERSION}"),
        "decode",
    ))
}

fn check_body_version(version: u32) -> Result<(), ControlError> {
    if version == API_VERSION {
        return Ok(());
    }
    Err(ControlError::new(
        ControlErrorKind::UnsupportedVersion,
        format!("unsupported body version {version}; expected {API_VERSION}"),
        "decode",
    ))
}

macro_rules! request {
    ($name:ident) => {
        impl sealed::Sealed for $name {}
        impl Request for $name {
            fn version(&self) -> u32 {
                self.version
            }
        }
    };
    ($name:ident, validate) => {
        impl sealed::Sealed for $name {}
        impl Request for $name {
            fn version(&self) -> u32 {
                self.version
            }

            fn validate(&self) -> Result<(), ControlError> {
                self.validate()
            }
        }
    };
}

macro_rules! response {
    ($($name:ident),+ $(,)?) => {
        $(
            impl sealed::Sealed for $name {}
            impl Response for $name {
                fn version(&self) -> u32 {
                    self.version
                }
            }
        )+
    };
}

request!(ConfigValidateRequest, validate);
request!(ConfigApplyRequest, validate);
request!(ReloadRequest, validate);
request!(ProviderTestRequest, validate);
request!(PicksSetRequest, validate);
request!(AgentPlanRequest, validate);
request!(AgentConnectRequest, validate);
request!(AgentDisconnectRequest, validate);
request!(AgentModelRequest, validate);
request!(TokenRotateRequest);
request!(ShutdownRequest);

impl sealed::Sealed for ConfigResponse {}
impl Response for ConfigResponse {
    fn version(&self) -> u32 {
        self.version
    }

    fn validate(&self) -> Result<(), ControlError> {
        self.validate()
    }
}

impl sealed::Sealed for ErrorEnvelope {}
impl Response for ErrorEnvelope {
    fn version(&self) -> u32 {
        self.version
    }

    fn validate(&self) -> Result<(), ControlError> {
        self.validate()
    }
}

impl sealed::Sealed for EventEnvelope {}
impl Response for EventEnvelope {
    fn version(&self) -> u32 {
        self.version
    }

    fn validate(&self) -> Result<(), ControlError> {
        self.validate()
    }
}

response!(
    HealthResponse,
    StatusResponse,
    ConfigValidationResponse,
    ProviderListResponse,
    ProviderTestResponse,
    ModelListResponse,
    PicksResponse,
    RouteModelListResponse,
    AgentListResponse,
    AgentStatusResponse,
    AgentPlanResponse,
    AgentChangeResponse,
    UsageResponse,
    TokenRotateResponse,
    ShutdownResponse,
);

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EnumRequest {
        version: u32,
        #[allow(dead_code)]
        action: EnumAction,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum EnumAction {
        Known,
    }

    impl sealed::Sealed for EnumRequest {}
    impl Request for EnumRequest {
        fn version(&self) -> u32 {
            self.version
        }
    }

    #[test]
    fn decode_unknown_field_names_obey_identifier_cap_and_fallback() {
        for (length, named) in [(64, true), (65, false)] {
            let key = "k".repeat(length);
            let body = format!(r#"{{"version":1,"{key}":true}}"#);
            let error = decode_request::<ShutdownRequest>(API_VERSION, body.as_bytes())
                .expect_err("unknown field");
            assert_decode_issue(
                &error,
                "unknown_field",
                if named { &key } else { "body" },
                &key,
                named,
            );
            if !named {
                assert_error_text_absent(&error, &key);
            }
        }

        let error = decode_request::<ShutdownRequest>(API_VERSION, br#"{"version":1,"a-b":true}"#)
            .expect_err("non-identifier field");
        assert_decode_issue(&error, "unknown_field", "body", "a-b", false);
        assert_error_text_absent(&error, "a-b");
    }

    #[test]
    fn decode_unknown_variant_is_not_reclassified_or_echoed() {
        let variant = "future_secret_variant";
        let body = format!(r#"{{"version":1,"action":"{variant}"}}"#);
        let error = decode_request::<EnumRequest>(API_VERSION, body.as_bytes())
            .expect_err("unknown variant");
        assert_decode_issue(&error, "unknown_variant", "body", variant, false);
        assert_error_text_absent(&error, variant);
    }

    #[test]
    fn decode_distinguishes_eof_and_syntax_errors() {
        let error = decode_request::<ShutdownRequest>(API_VERSION, br#"{"version":1"#)
            .expect_err("truncated request");
        assert_decode_issue(&error, "eof", "body", "body", false);
        assert!(error.message.contains("eof error"));

        let error = decode_request::<ShutdownRequest>(API_VERSION, br#"{"version":1,}"#)
            .expect_err("syntax error");
        assert_decode_issue(&error, "syntax", "body", "body", false);
        assert!(error.message.contains("syntax error"));
    }

    #[test]
    fn decode_data_error_reasons_and_named_fields_are_exact() {
        let error =
            decode_request::<ShutdownRequest>(API_VERSION, br#"{"version":"not-a-version"}"#)
                .expect_err("invalid type");
        assert_decode_issue(&error, "invalid_type", "body", "body", false);
        assert_error_text_absent(&error, "not-a-version");

        let error =
            decode_request::<ShutdownRequest>(API_VERSION, br#"{}"#).expect_err("missing version");
        assert_decode_issue(&error, "missing_field", "version", "version", true);

        let error = decode_request::<ShutdownRequest>(API_VERSION, br#"{"version":1,"version":1}"#)
            .expect_err("duplicate version");
        assert_decode_issue(&error, "duplicate_field", "version", "version", true);
    }

    fn assert_decode_issue(
        error: &ControlError,
        reason: &str,
        detail_field: &str,
        message_field: &str,
        named: bool,
    ) {
        assert_eq!(error.kind, ControlErrorKind::BadRequest);
        assert_eq!(error.details.len(), 1);
        assert_eq!(error.details[0].reason, reason);
        assert_eq!(error.details[0].field, detail_field);
        assert_eq!(
            error.message.contains(message_field),
            named,
            "unexpected field-name handling in {}",
            error.message
        );
    }

    fn assert_error_text_absent(error: &ControlError, text: &str) {
        assert!(!error.message.contains(text), "{}", error.message);
        for issue in &error.details {
            assert!(!issue.field.contains(text));
            assert!(!issue.reason.contains(text));
            assert!(!issue.message.contains(text));
        }
    }
}
