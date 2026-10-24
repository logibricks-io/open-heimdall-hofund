//! Composition root for the long-running hofund process. It reads configuration
//! and secrets, builds the gateway, and supervises the local service that the CLI
//! starts and stops. Protocol translation and vendor-specific behavior stay in the
//! crates below this one.

// Referenced so this crate's manifest dependencies are compiled, not merely declared.
use hofund_config as _;
use hofund_gateway as _;
use hofund_secrets as _;
