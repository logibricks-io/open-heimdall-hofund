//! Bidirectional HTTP on the host pool. No vendor framing lives here.

use std::time::Duration;

use bytes::Bytes;

use crate::body::BodyStream;
use crate::error::PluginError;
use crate::kernel::DuplexWriterBackend;

/// Which HTTP version the host dials for a duplex call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpVersionPin {
    /// HTTP/1.1 only. Not used for duplex today; kept for symmetry.
    Http1,
    /// HTTP/2 with ALPN on `https`, or prior knowledge on loopback `http`.
    Http2,
}

/// One duplex call before the session is open.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DuplexRequest {
    #[allow(dead_code)] // read when `kernel-internal` is enabled
    method: String,
    #[allow(dead_code)]
    uri: String,
    #[allow(dead_code)]
    headers: Vec<HeaderField>,
    pin: HttpVersionPin,
    #[allow(dead_code)]
    header_timeout: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct HeaderField {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    value: String,
    #[allow(dead_code)]
    sensitive: bool,
}

impl DuplexRequest {
    /// `POST` to absolute `uri`.
    #[must_use]
    pub fn post(uri: impl Into<String>) -> Self {
        Self::new("POST", uri)
    }

    fn new(method: &'static str, uri: impl Into<String>) -> Self {
        Self {
            method: method.to_owned(),
            uri: uri.into(),
            headers: Vec::new(),
            pin: HttpVersionPin::Http2,
            header_timeout: Duration::from_secs(15),
        }
    }

    /// Sets the HTTP version pin for this dial.
    #[must_use]
    pub fn pin(mut self, pin: HttpVersionPin) -> Self {
        self.pin = pin;
        self
    }

    /// How long to wait for response headers after the dial starts.
    #[must_use]
    pub fn header_timeout(mut self, timeout: Duration) -> Self {
        self.header_timeout = timeout;
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

#[cfg(feature = "kernel-internal")]
#[doc(hidden)]
impl DuplexRequest {
    pub(crate) fn method(&self) -> &str {
        &self.method
    }

    pub(crate) fn uri(&self) -> &str {
        &self.uri
    }

    pub(crate) fn headers(&self) -> &[HeaderField] {
        &self.headers
    }

    pub(crate) fn header_name(field: &HeaderField) -> &str {
        &field.name
    }

    pub(crate) fn header_value(field: &HeaderField) -> &str {
        &field.value
    }

    pub(crate) fn header_sensitive(field: &HeaderField) -> bool {
        field.sensitive
    }

    pub(crate) const fn version_pin(&self) -> HttpVersionPin {
        self.pin
    }

    pub(crate) const fn header_wait(&self) -> Duration {
        self.header_timeout
    }

    #[must_use]
    pub fn kernel_method(&self) -> &str {
        self.method()
    }

    #[must_use]
    pub fn kernel_uri(&self) -> &str {
        self.uri()
    }

    #[must_use]
    pub const fn kernel_version_pin(&self) -> HttpVersionPin {
        self.version_pin()
    }

    #[must_use]
    pub const fn kernel_header_wait(&self) -> Duration {
        self.header_wait()
    }

    #[must_use]
    pub fn kernel_header_triples(&self) -> Vec<(&str, &str, bool)> {
        self.headers()
            .iter()
            .map(|field| {
                (
                    Self::header_name(field),
                    Self::header_value(field),
                    Self::header_sensitive(field),
                )
            })
            .collect()
    }
}

/// An open duplex HTTP/2 (or pinned) session.
///
/// Dropping this value, or either half after [`Self::into_split`], aborts the call.
#[non_exhaustive]
pub struct DuplexSession {
    status: http::StatusCode,
    headers: http::HeaderMap,
    reader: BodyStream,
    writer: DuplexWriter,
}

impl DuplexSession {
    /// Kernel-only constructor for the host duplex pool.
    #[cfg(feature = "kernel-internal")]
    #[doc(hidden)]
    #[must_use]
    pub fn kernel_new(
        status: http::StatusCode,
        headers: http::HeaderMap,
        reader: BodyStream,
        writer: DuplexWriter,
    ) -> Self {
        Self {
            status,
            headers,
            reader,
            writer,
        }
    }

    /// The response status, available before the body is read.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.status
    }

    /// Response headers.
    #[must_use]
    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }

    /// The response body stream.
    #[must_use]
    pub fn reader(&mut self) -> &mut BodyStream {
        &mut self.reader
    }

    /// The request body writer.
    #[must_use]
    pub fn writer(&mut self) -> &mut DuplexWriter {
        &mut self.writer
    }

    /// Splits into independent reader and writer halves.
    #[must_use]
    pub fn into_split(self) -> (DuplexReader, DuplexWriter) {
        (
            DuplexReader {
                status: self.status,
                headers: self.headers,
                reader: self.reader,
            },
            self.writer,
        )
    }
}

/// Response metadata and body after [`DuplexSession::into_split`].
#[non_exhaustive]
pub struct DuplexReader {
    status: http::StatusCode,
    headers: http::HeaderMap,
    reader: BodyStream,
}

impl DuplexReader {
    /// The response status.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.status
    }

    /// Response headers.
    #[must_use]
    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }

    /// The response body stream.
    #[must_use]
    pub fn reader(&mut self) -> &mut BodyStream {
        &mut self.reader
    }
}

/// Request body half of a duplex session.
#[non_exhaustive]
pub struct DuplexWriter {
    inner: Box<dyn DuplexWriterBackend>,
}

impl DuplexWriter {
    /// Kernel-only constructor for the host duplex pool.
    #[cfg(feature = "kernel-internal")]
    #[doc(hidden)]
    #[must_use]
    pub fn kernel_new(inner: Box<dyn DuplexWriterBackend>) -> Self {
        Self { inner }
    }

    /// Queues `bytes` on the request body stream.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the session is finished or the host closed the body.
    pub async fn write(&mut self, bytes: Bytes) -> Result<(), PluginError> {
        self.inner.write(bytes).await
    }

    /// Ends the request body stream.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the host already closed the body.
    pub async fn finish(mut self) -> Result<(), PluginError> {
        self.inner.finish().await
    }
}
