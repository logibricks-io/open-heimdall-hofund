//! HTTP bodies that do not name `hyper` or `reqwest`.

use std::fmt;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

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

    /// The chunks `next` yields from `state`, until it returns `None`.
    ///
    /// For a plugin that turns one stream into another without naming a
    /// stream crate: `next` takes the state and returns the next chunk with
    /// the state to continue from. Once it returns `None`, the stream stays
    /// ended.
    #[must_use]
    pub fn unfold<T, F, Fut>(state: T, next: F) -> Self
    where
        T: Send + 'static,
        F: FnMut(T) -> Fut + Send + 'static,
        Fut: Future<Output = Option<(Result<Vec<u8>, PluginError>, T)>> + Send + 'static,
    {
        let stream = futures_util::stream::unfold(state, next)
            .map(|chunk| chunk.map(Bytes::from))
            .fuse();
        Self::from_stream(stream)
    }

    /// One chunk, then end.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        let chunk = bytes.into();
        Self::from_stream(futures_util::stream::once(async move { Ok(chunk) }))
    }

    /// `chunk`, then whatever `rest` had not yet yielded.
    ///
    /// The router uses this after peeking the first raw chunk so a failure
    /// before that chunk can fail over, and a success still delivers the chunk.
    #[must_use]
    pub fn resume(chunk: Bytes, rest: Self) -> Self {
        let head = futures_util::stream::once(async move { Ok(chunk) });
        Self {
            inner: Box::pin(head.chain(rest.inner)),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unfold_yields_each_chunk_then_ends() {
        let mut stream = BodyStream::unfold(0_u8, |count| async move {
            match count {
                0 | 1 => Some((Ok(vec![b'a' + count]), count + 1)),
                2 => Some((Err(PluginError::internal("broken")), 3)),
                _ => None,
            }
        });
        assert_eq!(
            stream.next_chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"a")
        );
        assert_eq!(
            stream.next_chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"b")
        );
        let error = stream.next_chunk().await.unwrap().unwrap_err();
        assert_eq!(error.message(), "broken");
        assert!(stream.next_chunk().await.is_none());
        assert!(stream.next_chunk().await.is_none());
    }
}
