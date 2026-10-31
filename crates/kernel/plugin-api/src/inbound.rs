//! Inbound HTTP surfaces. The plugin does not bind a socket.

use std::time::Duration;

use async_trait::async_trait;

use crate::body::{Body, BodyStream};
use crate::error::PluginError;
use crate::host::SurfaceHost;
use crate::id::SurfaceId;

/// An HTTP method plus a path prefix. Generated routes use [`RouteSpec::from_static`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RouteSpec {
    method: Method,
    prefix: &'static str,
}

/// An HTTP method a surface declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// PATCH.
    Patch,
    /// DELETE.
    Delete,
}

impl RouteSpec {
    /// A route. `prefix` starts with `/` and has no `?`. Generated code calls this.
    ///
    /// # Panics
    ///
    /// Panics when `prefix` is not a path prefix.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(method: Method, prefix: &'static str) -> Self {
        assert!(
            is_prefix(prefix),
            "route prefix must start with / and contain no query"
        );
        Self { method, prefix }
    }

    /// The method.
    #[must_use]
    pub const fn method(&self) -> Method {
        self.method
    }

    /// The path prefix.
    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        self.prefix
    }
}

const fn is_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes[0] != b'/' {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'?' || byte == b'#' || byte.is_ascii_whitespace() {
            return false;
        }
        index += 1;
    }
    true
}

/// A surface the gateway mounts. The plugin does not listen.
#[async_trait]
pub trait InboundSurface: Send + Sync + 'static {
    /// Surface id. Agent `requires` names this.
    fn id(&self) -> &SurfaceId;

    /// Routes the gateway mounts. Prefixes are checked for conflicts at build time.
    fn routes(&self) -> &'static [RouteSpec];

    /// Exact paths on which this surface accepts a Responses WebSocket.
    fn responses_websocket_paths(&self) -> &'static [&'static str] {
        &[]
    }

    /// Handles a request the gateway has already accepted.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the surface rejects the request.
    async fn handle(
        &self,
        route: &RouteSpec,
        request: http::Request<Body>,
        host: &SurfaceHost,
    ) -> Result<http::Response<BodyStream>, PluginError>;

    /// Whether the surface can take a request. The default is success.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the surface should be left unmounted.
    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Stops background work before `deadline`. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when shutdown fails.
    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }
}
