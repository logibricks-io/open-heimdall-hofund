//! RFC 8628 device authorization: ask for a code, show it, then poll.
//!
//! The plugin builds the two requests, because providers disagree on the form
//! fields. [`start`] reads the authorization response. [`poll`] repeats the
//! token request on the interval the server sets until the user approves,
//! denies, or the code expires.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use hofund_plugin_api::PluginError;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};

use super::Clock;
use super::token::{TokenSet, error_code, parse_token_set, token_error};
use crate::Reply;

/// The shortest wait between polls, whatever the server says.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// The wait when the server gives no `interval` (RFC 8628 section 3.2).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// What `slow_down` adds to the wait (RFC 8628 section 3.5).
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Failed polls in a row that end the sign-in. A failed poll is a transport
/// error, HTTP 429 or 5xx, or a body that is not a token or a standard error.
pub const MAX_MISSES: u32 = 5;

/// A device authorization response.
///
/// `Debug` hides the device code. The user code is meant to be shown.
#[derive(Clone)]
pub struct DeviceStart {
    device_code: SecretString,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Duration,
    interval: Duration,
}

impl DeviceStart {
    /// The code the token request sends back. Never show it.
    #[must_use]
    pub fn device_code(&self) -> &SecretString {
        &self.device_code
    }

    /// The code the user types.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Where the user types it.
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }

    /// A link with the code filled in, when the server gave one.
    #[must_use]
    pub fn verification_uri_complete(&self) -> Option<&str> {
        self.verification_uri_complete.as_deref()
    }

    /// How long the codes are valid.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// The first wait between polls, at least [`MIN_INTERVAL`].
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

impl fmt::Debug for DeviceStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceStart")
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// Sends the device authorization request and reads the answer.
///
/// `request` is the plugin's `Call::post(..).send(http)` future.
///
/// # Errors
///
/// Returns the transport error, [`token_error`] for a non-2xx status, or a
/// `Decode` error when the body is not a device authorization response.
pub async fn start<F>(request: F) -> Result<DeviceStart, PluginError>
where
    F: Future<Output = Result<Reply, PluginError>>,
{
    let reply = request.await?;
    if !(200..300).contains(&reply.status()) {
        return Err(token_error(reply.status(), reply.body()));
    }
    parse_start(reply.body())
}

/// Reads a device authorization response body.
///
/// `verification_url` is accepted for `verification_uri`, as some servers
/// send it. A missing `interval` is [`DEFAULT_INTERVAL`]; a smaller one than
/// [`MIN_INTERVAL`] is raised to it.
///
/// # Errors
///
/// Returns a `Decode` error naming the missing field. The body is not copied.
pub fn parse_start(body: &[u8]) -> Result<DeviceStart, PluginError> {
    let Ok(Value::Object(object)) = serde_json::from_slice(body) else {
        return Err(PluginError::decode(
            "device authorization response is not a JSON object",
        ));
    };
    let field = |name: &'static str| {
        text(&object, name).ok_or_else(|| {
            PluginError::decode(format!("device authorization response has no {name}"))
        })
    };
    let device_code = field("device_code")?;
    let user_code = field("user_code")?;
    let verification_uri = text(&object, "verification_uri")
        .or_else(|| text(&object, "verification_url"))
        .ok_or_else(|| {
            PluginError::decode("device authorization response has no verification_uri")
        })?;
    let expires_in = seconds(&object, "expires_in")
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| PluginError::decode("device authorization response has no expires_in"))?;
    let interval = seconds(&object, "interval")
        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
        .max(MIN_INTERVAL);
    Ok(DeviceStart {
        device_code: SecretString::from(device_code),
        user_code: user_code.to_owned(),
        verification_uri: verification_uri.to_owned(),
        verification_uri_complete: text(&object, "verification_uri_complete").map(str::to_owned),
        expires_in: Duration::from_secs(expires_in),
        interval,
    })
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn seconds(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

/// What one poll answer means.
enum Step {
    Token(TokenSet),
    Pending,
    SlowDown,
    Miss(PluginError),
    Fail(PluginError),
}

/// Polls the token endpoint until the user finishes.
///
/// Waits [`DeviceStart::interval`] before each request. `send` builds and
/// sends one token request; it is called once per poll.
///
/// - `authorization_pending` keeps polling.
/// - `slow_down` keeps polling and adds [`SLOW_DOWN_STEP`] to the wait.
/// - `access_denied` and `expired_token` end with `Auth`.
/// - A token ends with the [`TokenSet`], read at `clock.now()`.
/// - [`MAX_MISSES`] failed polls in a row end with the last failure. Any
///   standard answer resets the count.
/// - Once `expires_in` has passed, polling stops with `Auth`.
///
/// Elapsed time is the larger of the clock's monotonic time and the sum of
/// the waits, so a clock that does not move still ends at `expires_in`.
///
/// # Errors
///
/// As above, or [`token_error`] for any other refusal.
pub async fn poll<C, F, Fut>(
    clock: &C,
    start: &DeviceStart,
    mut send: F,
) -> Result<TokenSet, PluginError>
where
    C: Clock + ?Sized,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Reply, PluginError>>,
{
    let began = clock.monotonic();
    let mut interval = start.interval;
    let mut waited = Duration::ZERO;
    let mut misses = 0;
    loop {
        tokio::time::sleep(interval).await;
        waited += interval;
        let elapsed = clock.monotonic().saturating_sub(began).max(waited);
        if elapsed >= start.expires_in {
            return Err(PluginError::auth(
                "the device code expired before sign-in finished",
            ));
        }
        let step = match send().await {
            Ok(reply) => classify(clock, &reply),
            Err(error) => Step::Miss(error),
        };
        match step {
            Step::Token(set) => return Ok(set),
            Step::Pending => misses = 0,
            Step::SlowDown => {
                misses = 0;
                interval += SLOW_DOWN_STEP;
            }
            Step::Miss(error) => {
                misses += 1;
                if misses >= MAX_MISSES {
                    return Err(error);
                }
            }
            Step::Fail(error) => return Err(error),
        }
    }
}

fn classify<C: Clock + ?Sized>(clock: &C, reply: &Reply) -> Step {
    let status = reply.status();
    match error_code(reply.body()) {
        Some("authorization_pending") => return Step::Pending,
        Some("slow_down") => return Step::SlowDown,
        // access_denied and expired_token map to Auth in token_error.
        Some(_) => return Step::Fail(token_error(status, reply.body())),
        None => {}
    }
    if status == 429 || status >= 500 {
        return Step::Miss(token_error(status, reply.body()));
    }
    if !(200..300).contains(&status) {
        return Step::Fail(token_error(status, reply.body()));
    }
    match parse_token_set(reply.body(), clock.now()) {
        Ok(set) => Step::Token(set),
        Err(error) => Step::Miss(PluginError::decode(error.message())),
    }
}

/// The RFC 8628 token request form: grant type, client id, and device code.
///
/// It contains the device code. Do not log it.
#[must_use]
pub fn device_code_form(client_id: &str, start: &DeviceStart) -> String {
    super::form_body(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("client_id", client_id),
        ("device_code", start.device_code.expose_secret()),
    ])
}

#[cfg(test)]
mod tests;
