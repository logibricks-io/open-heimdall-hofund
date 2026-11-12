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
mod cancellation;
mod completion;
mod config;
mod duplex;
mod error;
mod forward;
mod host;
mod http_budget;
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

mod balance;
mod billed;
mod checkin;
#[cfg(not(feature = "kernel"))]
mod kernel;
mod plan;
mod quota_reset;
mod records;
mod redaction;
pub mod responses_ws;
mod subscription;
mod upstream;
mod warmup;

/// Process-host handshake. A future host compares this string with the plugin
/// manifest. Stage 1 has no compile-time consumer.
pub const API: &str = "1";

pub use agent::{
    AgentFactory, AgentIntegration, ConnectionStatus, Credential, CredentialError, CredentialStore,
    DisconnectOutcome, DynAgentFactory, FileChange, GatewayTarget, ImportCtx, ImportedProvider,
    Plan, WriteCtx,
};
pub use balance::{
    BalanceCard, BalancePart, BalanceQuery, BalanceQuota, BalanceSupport, BalanceWallet,
};
pub use billed::BilledUsage;
pub use body::{Body, BodyStream};
pub use cancellation::StoppedWaiting;
pub use checkin::{CheckinAccountStatus, CheckinOutcome, CheckinRecord, CheckinResult};
pub use completion::{Completion, CountOutcome, PROBE_CAP, ProbeReport};
pub use config::{BuildError, ConfigError, RawTable, RawValue, Schema, ValidConfig};
pub use duplex::{DuplexReader, DuplexRequest, DuplexSession, DuplexWriter, HttpVersionPin};
pub use error::{PluginError, PluginErrorKind};
pub use forward::{Forward, RawAnswer, RawBody};
pub use hofund_ir::{
    BlockStart, ContentPart, Extensions, ImagePart, ImageSource, Message, MessageStart, Request,
    Response, Role, StillWorking, Stop, StopReason, StreamEvent, TextPart, Tool, ToolArguments,
    ToolChoiceMode, ToolResultPart, too_long, words_of,
};
pub use host::{
    AccountLease, AccountsHandle, AuthResult, BRIDGE_CALLBACK_PATH_PREFIX, BindBlocked,
    BridgeCallbackGuard, BridgeCallbackHandle, BridgeCallbackHandler, BridgeCallbackReply,
    ClockHandle, DirHandle, GatewayHandle, HostContext, HttpHandle, Job, JobId, JobSpec,
    LOOPBACK_CANCEL_PATH, LongLivedChild, LoopbackBinding, LoopbackHandle, LoopbackSpec,
    ObserveHandle, PipedChild, ProcessHandle, RedirectHost, SchedulerHandle, SecretsHandle,
    SpawnRequest, Spawned, SurfaceHost,
};
pub use http_budget::{HostReadBudget, read_budget, set_read_budget};
pub use id::{
    AccountId, IdError, MemberId, MemberKey, PluginId, PluginKey, PluginKind, PresetId, ProtocolId,
    SurfaceId, TierId,
};
pub use inbound::{InboundSurface, Method, RouteSpec};
pub use plan::{PlanCard, PlanQuery, PlanQuota, PlanSupport, PlanWindow};
pub use quota_reset::{QuotaResetCard, QuotaResetCredits, QuotaResetOutcome};
pub use records::{
    Account, AccountStatus, AgentActiveProvider, AgentCredential, AgentDescription, AgentFile,
    AgentPath, AgentRemove, AgentRoute, AgentSet, AgentSignIn, AgentStatusCheck, AuthChoice,
    BinaryName, DeviceFlow, DiscoveredAgentAccount, FileKind, ListQuery, MatchNeedle, ModelField,
    ModelList, ModelSpec, ModelValue, OwnerTest, ProbeQuery, QuotaObservation, QuotaQuery,
    QuotaWindow, SignInChallenge, SignInCtx, SignInFlow, SignInFlowKind, SignInProof, Slot,
    SubscriptionSpec, Visible,
};
pub use redaction::{redact_token, safe_url};
pub use secrecy::SecretString;
pub use subscription::{DynSubscriptionFactory, Subscription, SubscriptionFactory};
pub use upstream::{
    BuildContext, CredentialStyle, DynUpstreamFactory, HeaderName, ListKind, UpstreamFactory,
    UpstreamProvider, UpstreamSpec,
};
pub use warmup::{WarmupAccountStatus, WarmupOutcome, WarmupRecord};

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
        assert_impl_all!(crate::LongLivedChild: Send, Sync);
        assert_impl_all!(crate::DirHandle: Send, Sync);
        assert_impl_all!(crate::ObserveHandle: Send, Sync);
        assert_impl_all!(crate::LoopbackHandle: Send, Sync);
        assert_impl_all!(crate::BridgeCallbackHandle: Send, Sync);
        assert_impl_all!(crate::BridgeCallbackGuard: Send);
        assert_impl_all!(crate::BridgeCallbackReply: Send, Sync);
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
