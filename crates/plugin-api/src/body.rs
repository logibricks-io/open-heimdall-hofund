//! HTTP bodies that do not name `hyper` or `reqwest`.

use std::fmt;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::Stream;

use crate::error::PluginError;

/// A buffered request or response body.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Body {
    bytes: Bytes,
}

impl Body {
    /// A body holding `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// An empty body.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            bytes: Bytes::new(),
        }
    }

    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Whether the body has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Body")
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Self {
        Self::from_bytes(Bytes::copy_from_slice(bytes))
    }
}

/// A streaming body. Dropping it drops the rest of the stream.
#[non_exhaustive]
pub struct BodyStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, PluginError>> + Send>>,
}

impl BodyStream {
    /// A stream of chunks.
    #[must_use]
    pub fn from_stream(
        stream: impl Stream<Item = Result<Bytes, PluginError>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }

    /// One chunk, then end.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        let chunk = bytes.into();
        Self::from_stream(futures_util::stream::once(async move { Ok(chunk) }))
    }

    /// The next chunk, if the stream has one.
    ///
    /// # Errors
    ///
    /// Returns the stream's error for this chunk.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, PluginError>> {
        futures_util::StreamExt::next(&mut self.inner).await
    }
}

impl fmt::Debug for BodyStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BodyStream")
    }
}
