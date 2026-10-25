//! Sample inbound plugin.
//!
//! The gateway mounts `GET /hofund/echo`. The registry rejects a conflicting
//! prefix at build time.

#![deny(missing_docs)]

use hofund_plugin_api::{InboundSurface, Method, PluginError, RouteSpec, SurfaceHost, SurfaceId};
use hofund_plugin_sdk::{Contribution, HttpRequest, HttpResponse, text_response};

/// Registers the echo surface.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().surface(Echo)
}

static ID: SurfaceId = SurfaceId::from_static("echo");
static ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Get, "/hofund/echo")];

/// The echo surface.
#[derive(Clone, Copy, Debug)]
pub struct Echo;

#[hofund_plugin_sdk::async_trait]
impl InboundSurface for Echo {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        route: &RouteSpec,
        request: HttpRequest,
        host: &SurfaceHost,
    ) -> Result<HttpResponse, PluginError> {
        let _ = (route, request, host);
        Ok(text_response(200, "echo"))
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{InboundSurface, Method};

    use super::Echo;

    #[test]
    fn the_route_is_get_hofund_echo() {
        let echo = Echo;
        assert_eq!(echo.id().as_str(), "echo");
        assert_eq!(echo.routes().len(), 1);
        assert_eq!(echo.routes()[0].prefix(), "/hofund/echo");
        assert_eq!(echo.routes()[0].method(), Method::Get);
    }
}
