//! HTTP front door of hofund: an axum server and the middleware pipeline that
//! accepts client traffic, asks the router for an upstream, and writes the encoded
//! response or stream. Vendor names and vendor-specific branches do not belong
//! here; they live in quirk data and provider adapters.

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_router as _;
