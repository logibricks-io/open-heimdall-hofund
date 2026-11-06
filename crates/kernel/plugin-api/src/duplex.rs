//! Bidirectional HTTP on the host pool. No vendor framing lives here.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;

use crate::body::BodyStream;
use crate::error::PluginError;
use crate::kernel::{DuplexGuard, DuplexWriterBackend};

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
#[derive(Clone)]
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

#[derive(Clone)]
pub(crate) struct HeaderField {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    value: String,
    #[allow(dead_code)]
    sensitive: bool,
}

impl fmt::Debug for DuplexRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .headers
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        formatter
            .debug_struct("DuplexRequest")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("header_names", &names)
            .field("pin", &self.pin)
            .field("header_timeout", &self.header_timeout)
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

struct InertDuplexGuard;

impl DuplexGuard for InertDuplexGuard {
    fn finish(&self) {}
    fn abort_if_unfinished(&self) {}
}

struct InertDuplexWriterBackend;

#[async_trait::async_trait]
impl DuplexWriterBackend for InertDuplexWriterBackend {
    async fn write(&mut self, _bytes: Bytes) -> Result<(), PluginError> {
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

fn inert_duplex_writer() -> DuplexWriter {
    DuplexWriter {
        inner: Box::new(InertDuplexWriterBackend),
        guard: Arc::new(InertDuplexGuard),
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
    guard: Arc<dyn DuplexGuard>,
}

impl Drop for DuplexSession {
    fn drop(&mut self) {
        if self.guard.session_should_abort_on_drop() {
            self.guard.abort_if_unfinished();
        }
    }
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
        guard: Arc<dyn DuplexGuard>,
    ) -> Self {
        Self {
            status,
            headers,
            reader,
            writer,
            guard,
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
    pub fn into_split(mut self) -> (DuplexReader, DuplexWriter) {
        self.guard.mark_split();
        let writer = std::mem::replace(&mut self.writer, inert_duplex_writer());
        (
            DuplexReader {
                status: self.status,
                headers: std::mem::take(&mut self.headers),
                reader: std::mem::replace(
                    &mut self.reader,
                    BodyStream::from_stream(stream::empty()),
                ),
                guard: Arc::clone(&self.guard),
            },
            writer,
        )
    }
}

/// Response metadata and body after [`DuplexSession::into_split`].
#[non_exhaustive]
pub struct DuplexReader {
    status: http::StatusCode,
    headers: http::HeaderMap,
    reader: BodyStream,
    guard: Arc<dyn DuplexGuard>,
}

impl Drop for DuplexReader {
    fn drop(&mut self) {
        self.guard.abort_if_unfinished();
    }
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
    guard: Arc<dyn DuplexGuard>,
}

impl Drop for DuplexWriter {
    fn drop(&mut self) {
        self.guard.abort_if_unfinished();
    }
}

impl DuplexWriter {
    /// Kernel-only constructor for the host duplex pool.
    #[cfg(feature = "kernel-internal")]
    #[doc(hidden)]
    #[must_use]
    pub fn kernel_new(inner: Box<dyn DuplexWriterBackend>, guard: Arc<dyn DuplexGuard>) -> Self {
        Self { inner, guard }
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
        self.guard.finish();
        self.inner.finish().await
    }
}

#[cfg(test)]
mod tests {
    use super::DuplexRequest;

    #[test]
    fn debug_omits_sensitive_header_values() {
        let request = DuplexRequest::post("https://127.0.0.1/run")
            .header("x-test", "visible")
            .sensitive_header("authorization", "secret-token-value");
        let text = format!("{request:?}");
        assert!(text.contains("authorization"));
        assert!(text.contains("x-test"));
        assert!(!text.contains("secret-token-value"));
        assert!(!text.contains("visible"));
    }
}
