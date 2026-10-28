use serde::de::DeserializeOwned;
use serde_json::error::Category;

use crate::{
    API_VERSION, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse,
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
/// shape, body version, then semantic validation. JSON errors contain only a
/// category, line and column, and a field name for unknown, missing, or
/// duplicate fields when it is a short ASCII identifier. Body values are never
/// included in the message or details.
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
    ErrorEnvelope,
    EventEnvelope,
);
