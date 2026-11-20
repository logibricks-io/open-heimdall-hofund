//! One outbound WebSocket on the host client. No vendor framing lives here.

use std::fmt;
use std::time::Duration;

use crate::error::PluginError;
use crate::kernel::WebSocketIo;

/// One text or binary frame.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebSocketMessage {
    /// A UTF-8 text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
}

/// A handshake before the session is open.
#[derive(Clone)]
#[non_exhaustive]
pub struct WebSocketRequest {
    uri: String,
    headers: Vec<HeaderField>,
    idle: Option<Duration>,
    age: Option<Duration>,
}

#[derive(Clone)]
struct HeaderField {
    name: String,
    /// Read by the host when the `kernel` feature is on.
    #[cfg_attr(not(feature = "kernel"), allow(dead_code))]
    value: String,
    sensitive: bool,
}

impl fmt::Debug for WebSocketRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .headers
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        formatter
            .debug_struct("WebSocketRequest")
            .field("uri", &self.uri)
            .field("header_names", &names)
            .field("idle", &self.idle)
            .field("age", &self.age)
            .finish()
    }
}

impl fmt::Debug for HeaderField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HeaderField")
            .field("name", &self.name)
            .field("sensitive", &self.sensitive)
            .finish_non_exhaustive()
    }
}

impl WebSocketRequest {
    /// A handshake to absolute `uri` (`ws`, `wss`, `http`, or `https`).
    #[must_use]
    pub fn get(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            headers: Vec::new(),
            idle: None,
            age: None,
        }
    }

    /// Closes the socket after `idle` with no send or recv, and `age` after the handshake.
    ///
    /// The host clock arms both deadlines. A later send or recv starts `idle`
    /// again. `age` stays fixed at the handshake.
    #[must_use]
    pub fn lifetime(mut self, idle: Duration, age: Duration) -> Self {
        self.idle = Some(idle);
        self.age = Some(age);
        self
    }

    /// Adds a header. The value is not sensitive.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(HeaderField {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        });
        self
    }

    /// Adds a header and marks the value sensitive.
    #[must_use]
    pub fn sensitive_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(HeaderField {
            name: name.into(),
            value: value.into(),
            sensitive: true,
        });
        self
    }
}

#[cfg(feature = "kernel")]
#[doc(hidden)]
impl WebSocketRequest {
    /// The handshake URI. The host reads this. A plugin does not.
    #[must_use]
    pub fn kernel_uri(&self) -> &str {
        &self.uri
    }

    /// Handshake headers as `(name, value, sensitive)`. The host reads this.
    #[must_use]
    pub fn kernel_headers(&self) -> Vec<(&str, &str, bool)> {
        self.headers
            .iter()
            .map(|field| (field.name.as_str(), field.value.as_str(), field.sensitive))
            .collect()
    }

    /// Idle budget from the last send or recv. The host reads this.
    #[must_use]
    pub fn kernel_idle(&self) -> Option<Duration> {
        self.idle
    }

    /// Age budget from the handshake. The host reads this.
    #[must_use]
    pub fn kernel_age(&self) -> Option<Duration> {
        self.age
    }
}

/// An open WebSocket. Dropping it closes the connection.
#[non_exhaustive]
pub struct WebSocketSession {
    io: Box<dyn WebSocketIo>,
}

impl WebSocketSession {
    /// Kernel-only constructor for the host client.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    pub fn kernel_new(io: Box<dyn WebSocketIo>) -> Self {
        Self { io }
    }

    /// Sends one text frame.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the connection is closed or the write fails.
    pub async fn send_text(&mut self, text: String) -> Result<(), PluginError> {
        self.io.send_text(text).await
    }

    /// The next data frame, or `None` when the peer closes.
    ///
    /// Control frames are handled inside the host client.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the read fails.
    pub async fn recv(&mut self) -> Result<Option<WebSocketMessage>, PluginError> {
        self.io.recv().await
    }

    /// Sends a close frame.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the close frame cannot be written. The
    /// connection is dropped either way.
    pub async fn close(&mut self) -> Result<(), PluginError> {
        self.io.close().await
    }
}

impl fmt::Debug for WebSocketSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketSession")
            .finish_non_exhaustive()
    }
}
