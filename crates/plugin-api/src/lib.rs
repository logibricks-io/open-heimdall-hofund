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
//! Public structs and enums are `#[non_exhaustive]` and their fields are
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
///
/// The module is public only with the `kernel` feature. `hofund-plugin-host`
/// is the crate that enables it. A workspace build unifies features, so
/// `xtask` also rejects any use of this module outside the host stack.
#[cfg(feature = "kernel")]
#[doc(hidden)]
pub mod kernel;

#[cfg(not(feature = "kernel"))]
mod kernel;
mod records;
mod redaction;
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
pub use hofund_ir::{ContentPart, Request, Response, Role, StreamEvent};
pub use host::{
    AccountLease, AccountsHandle, AuthResult, BindBlocked, ClockHandle, DirHandle, HostContext,
    HttpHandle, Job, JobId, JobSpec, LOOPBACK_CANCEL_PATH, LoopbackBinding, LoopbackHandle,
    LoopbackSpec, ObserveHandle, PipedChild, ProcessHandle, RedirectHost, SchedulerHandle,
    SecretsHandle, SpawnRequest, Spawned, SurfaceHost,
};
pub use id::{
    AccountId, IdError, MemberId, MemberKey, PluginId, PluginKey, PluginKind, PresetId, ProtocolId,
    SurfaceId, TierId,
};
pub use inbound::{InboundSurface, Method, RouteSpec};
pub use records::{
    Account, AccountStatus, AgentDescription, AgentFile, AgentRoute, AgentSet, AuthChoice,
    BinaryName, DeviceFlow, FileKind, ListQuery, MatchNeedle, ModelList, ModelSpec, ProbeQuery,
    QuotaObservation, QuotaQuery, QuotaWindow, SignInChallenge, SignInCtx, SignInFlow,
    SignInFlowKind, SignInProof, Slot, SubscriptionSpec, Visible,
};
pub use redaction::{redact_token, safe_url};
pub use secrecy::SecretString;
pub use subscription::{DynSubscriptionFactory, Subscription, SubscriptionFactory};
pub use upstream::{
    BuildContext, CredentialStyle, DynUpstreamFactory, HeaderName, ListKind, UpstreamFactory,
    UpstreamProvider, UpstreamSpec,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use static_assertions::assert_impl_all;

    use super::API;

    #[test]
    fn handshake_is_one() {
        assert_eq!(API, "1");
    }

    #[test]
    fn handles_and_errors_are_send_and_sync() {
        assert_impl_all!(crate::HostContext: Send, Sync);
        assert_impl_all!(crate::HttpHandle: Send, Sync);
        assert_impl_all!(crate::SecretsHandle: Send, Sync);
        assert_impl_all!(crate::AccountsHandle: Send, Sync);
        assert_impl_all!(crate::ClockHandle: Send, Sync);
        assert_impl_all!(crate::SchedulerHandle: Send, Sync);
        assert_impl_all!(crate::ProcessHandle: Send, Sync);
        assert_impl_all!(crate::DirHandle: Send, Sync);
        assert_impl_all!(crate::ObserveHandle: Send, Sync);
        assert_impl_all!(crate::LoopbackHandle: Send, Sync);
        assert_impl_all!(crate::SurfaceHost: Send, Sync);
        assert_impl_all!(crate::PluginError: Send, Sync);
        assert_impl_all!(Arc<dyn crate::DynUpstreamFactory>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::DynSubscriptionFactory>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::DynAgentFactory>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::UpstreamProvider>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::Subscription>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::AgentIntegration>: Send, Sync);
        assert_impl_all!(Arc<dyn crate::InboundSurface>: Send, Sync);
    }
}
