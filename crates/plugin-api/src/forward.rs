//! Calls the kernel routes by model and forwards to a provider as they are.
//!
//! Embeddings, reranking and audio transcription are not chats. The kernel
//! does not translate them. It picks a member the way it does for a chat,
//! asks the surface for the body with that member's own model id, and hands
//! the bytes to the provider.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;

use crate::PluginError;

/// The longest path a [`Forward`] may name.
const PATH_CAP: usize = 128;

/// A body as it goes to one upstream, with its `Content-Type`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawBody {
    content_type: String,
    bytes: Bytes,
}

impl RawBody {
    /// A body of `content_type`.
    #[must_use]
    pub fn new(content_type: impl Into<String>, bytes: impl Into<Bytes>) -> Self {
        Self {
            content_type: content_type.into(),
            bytes: bytes.into(),
        }
    }

    /// The `Content-Type` the body is sent with.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The body.
    #[must_use]
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

type BodyFor = Arc<dyn Fn(&str) -> Result<RawBody, PluginError> + Send + Sync>;

/// A call the kernel routes by model and forwards to the chosen provider.
///
/// `path` is relative to the provider's OpenAI-style base URL, such as
/// `/embeddings`. The body is built once per tried member, for the model id
/// that member's provider knows.
#[derive(Clone)]
#[non_exhaustive]
pub struct Forward {
    model: String,
    path: String,
    body: BodyFor,
}

impl Forward {
    /// A call for the route `model` on `path`, whose body `body` builds for a
    /// provider's model id.
    ///
    /// # Errors
    ///
    /// Returns a `400` status error when `model` is empty, or when `path` is
    /// not an absolute path of ASCII letters, digits, `-`, `_`, `.` and `/`
    /// with no empty or dot segment.
    pub fn new(
        model: impl Into<String>,
        path: impl Into<String>,
        body: impl Fn(&str) -> Result<RawBody, PluginError> + Send + Sync + 'static,
    ) -> Result<Self, PluginError> {
        let model = model.into();
        let path = path.into();
        if model.is_empty() {
            return Err(PluginError::status_error(400, "model is required"));
        }
        if !valid_path(&path) {
            return Err(PluginError::status_error(400, "forward path is not valid"));
        }
        Ok(Self {
            model,
            path,
            body: Arc::new(body),
        })
    }

    /// The route the client named.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The path under the provider's base URL.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The body for the provider's model id `model`.
    ///
    /// # Errors
    ///
    /// Returns what the surface's body builder returns.
    pub fn body_for(&self, model: &str) -> Result<RawBody, PluginError> {
        (self.body)(model)
    }
}

impl fmt::Debug for Forward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forward")
            .field("model", &self.model)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn valid_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    path.len() <= PATH_CAP
        && rest.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

/// A provider's successful answer to a forwarded call.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawAnswer {
    status: u16,
    content_type: Option<String>,
    body: Bytes,
}

impl RawAnswer {
    /// An answer with `status`, the provider's `Content-Type` when it sent
    /// one, and `body`.
    #[must_use]
    pub fn new(status: u16, content_type: Option<String>, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }

    /// The provider's status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The provider's `Content-Type`, when it sent one.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The body, as the provider sent it.
    #[must_use]
    pub const fn body(&self) -> &Bytes {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use super::{Forward, PATH_CAP, RawAnswer, RawBody};

    #[expect(
        clippy::unnecessary_wraps,
        reason = "a forward body builder returns a Result"
    )]
    fn echo(model: &str) -> Result<RawBody, crate::PluginError> {
        Ok(RawBody::new("application/json", model.to_owned()))
    }

    #[test]
    fn paths_are_plain_absolute_segments() {
        for good in [
            "/embeddings",
            "/rerank",
            "/audio/transcriptions",
            "/v1.2/a_b-c",
        ] {
            assert!(Forward::new("m", good, echo).is_ok(), "{good}");
        }
        for bad in [
            "",
            "embeddings",
            "/",
            "//x",
            "/a//b",
            "/a/",
            "/../x",
            "/a/./b",
            "/a?b=1",
            "/a#b",
            "/a b",
            "/a%2e",
            "/\u{e9}",
        ] {
            let error = Forward::new("m", bad, echo).unwrap_err();
            assert_eq!(error.status(), Some(400), "{bad:?}");
        }
        let longest = format!("/{}", "a".repeat(PATH_CAP - 1));
        assert!(Forward::new("m", longest.clone(), echo).is_ok());
        assert!(Forward::new("m", format!("{longest}a"), echo).is_err());
    }

    #[test]
    fn model_is_required_and_bodies_follow_the_member_model() {
        let error = Forward::new("", "/embeddings", echo).unwrap_err();
        assert_eq!(error.status(), Some(400));
        assert_eq!(error.message(), "model is required");
        let call = Forward::new("route", "/embeddings", echo).unwrap();
        assert_eq!(call.model(), "route");
        assert_eq!(call.path(), "/embeddings");
        let body = call.body_for("vendor-id").unwrap();
        assert_eq!(body.content_type(), "application/json");
        assert_eq!(body.bytes().as_ref(), b"vendor-id");
        let shown = format!("{call:?}");
        assert!(
            shown.contains("route") && shown.contains("/embeddings"),
            "{shown}"
        );
    }

    #[test]
    fn answers_keep_what_the_provider_sent() {
        let answer = RawAnswer::new(201, Some("audio/x".into()), b"x".to_vec());
        assert_eq!(answer.status(), 201);
        assert_eq!(answer.content_type(), Some("audio/x"));
        assert_eq!(answer.body().as_ref(), b"x");
        assert_eq!(RawAnswer::new(200, None, Vec::new()).content_type(), None);
    }
}
