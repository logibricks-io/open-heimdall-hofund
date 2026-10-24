//! Routing groups, selection strategies, and failover across providers. A decision
//! is expressed with provider identity, health, and policy. This crate does not
//! name vendors and does not consult quirk data; those facts stay in `quirks` and
//! the provider adapters.

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_provider as _;
