//! The plugin contract.
//!
//! A plugin depends on this crate and, when it needs a helper, on
//! `hofund-plugin-sdk`. It does not construct a [`HostContext`]. The
//! constructor is `hofund_plugin_host::host_context`.
//!
//! [`API`] is the process-host handshake string. Nothing in this crate reads
//! it at compile time.
//!
//! # Semver
//!
//! Public structs and enums are [`non_exhaustive`] and their fields are
//! private. A change to this surface updates `public-api.txt` in the same
//! pull request.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

mod agent;
mod body;
mod completion;
mod config;
mod error;
mod host;
mod id;
mod inbound;
/// Kernel backends. Plugins do not implement these traits.
#[doc(hidden)]
pub mod kernel;
mod records;
mod subscription;
mod upstream;

/// Process-host handshake. A future host compares this string with the plugin
/// manifest. Stage 1 has no compile-time consumer.
pub const API: &str = "1";

pub use agent::{
    AgentFactory, AgentIntegration, ConnectionStatus, DisconnectOutcome, DynAgentFactory,
    FileChange, GatewayTarget, ImportCtx, ImportedProvider, Plan, WriteCtx,
};
pub use body::{Body, BodyStream};
pub use completion::{Completion, CountOutcome, PROBE_CAP, ProbeReport};
pub use config::{BuildError, ConfigError, RawTable, RawValue, Schema, ValidConfig};
pub use error::{PluginError, PluginErrorKind};
pub use hofund_ir::{Request, Response, StreamEvent};
pub use host::{
    AccountsHandle, AuthResult, ClockHandle, DirHandle, HostContext, HttpHandle, JobId, JobSpec,
    ObserveHandle, ProcessHandle, SchedulerHandle, SecretsHandle, SpawnRequest, Spawned,
    SurfaceHost,
};
pub use id::{
    AccountId, IdError, MemberId, MemberKey, PluginId, PresetId, ProtocolId, SurfaceId, TierId,
};
pub use inbound::{InboundSurface, Method, RouteSpec};
pub use records::{
    Account, AgentDescription, AgentFile, AgentRoute, AgentSet, BinaryName, FileKind, ListQuery,
    MatchNeedle, ModelList, ModelSpec, ProbeQuery, QuotaObservation, QuotaQuery, QuotaWindow,
    SignInChallenge, SignInCtx, SignInFlow, SignInProof, Slot, StaticUrl, SubscriptionSpec,
    Visible,
};
pub use secrecy::SecretString;
pub use subscription::{DynSubscriptionFactory, Subscription, SubscriptionFactory};
pub use upstream::{
    CredentialStyle, DynUpstreamFactory, HeaderName, ListKind, UpstreamFactory, UpstreamProvider,
    UpstreamSpec,
};

#[cfg(test)]
mod tests {
    use super::API;

    #[test]
    fn handshake_is_one() {
        assert_eq!(API, "1");
    }
}
