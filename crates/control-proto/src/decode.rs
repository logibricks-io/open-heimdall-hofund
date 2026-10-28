use serde::de::DeserializeOwned;

use crate::{
    API_VERSION, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse,
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    ControlError, ControlErrorKind, ErrorEnvelope, EventEnvelope, HealthResponse,
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
/// shape, body version, then semantic validation. JSON errors name the serde
/// failure but never include the source body.
pub fn decode_request<T: Request>(path_version: u32, body: &[u8]) -> Result<T, ControlError> {
    check_length(body, MAX_REQUEST_BODY_BYTES)?;
    check_path_version(path_version)?;
    let request = serde_json::from_slice::<T>(body).map_err(|error| {
        ControlError::new(
            ControlErrorKind::BadRequest,
            format!("malformed request JSON: {error}"),
            "decode",
        )
    })?;
    check_body_version(request.version())?;
    request.validate()?;
    Ok(request)
}

/// Decodes a bounded response or event body, accepting additive unknown fields.
pub fn decode_response<T: Response>(path_version: u32, body: &[u8]) -> Result<T, ControlError> {
    check_length(body, MAX_RESPONSE_BODY_BYTES)?;
    check_path_version(path_version)?;
    let response = serde_json::from_slice::<T>(body).map_err(|error| {
        ControlError::new(
            ControlErrorKind::BadRequest,
            format!("malformed response JSON: {error}"),
            "decode",
        )
    })?;
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
request!(ReloadRequest);
request!(ProviderTestRequest, validate);
request!(PicksSetRequest, validate);
request!(AgentPlanRequest, validate);
request!(AgentConnectRequest);
request!(AgentDisconnectRequest);
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
