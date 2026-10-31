//! Wire names shared by the Responses WebSocket edge and its clients.

/// Beta header carried by the upgrade request.
pub const BETA_HEADER: &str = "openai-beta";
/// Beta value that belongs to the socket rather than a replayed turn.
pub const SOCKET_BETA: &str = "responses_websockets";
/// Turn-state key in `client_metadata`, and its corresponding HTTP header.
pub const TURN_STATE: &str = "x-codex-turn-state";
/// Responses-lite key in `client_metadata`.
pub const LITE_METADATA: &str = "ws_request_header_x_openai_internal_codex_responses_lite";
/// Responses-lite HTTP header.
pub const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
/// Model header copied into response metadata.
pub const MODEL_HEADER: &str = "openai-model";
/// Reasoning-presence header copied into response metadata.
pub const REASONING_HEADER: &str = "x-reasoning-included";
/// Model-list validator header.
pub const MODELS_ETAG_HEADER: &str = "x-models-etag";
/// Event carrying the model-list validator.
pub const RESPONSE_METADATA_EVENT: &str = "codex.response.metadata";
/// Event carrying account rate-limit windows.
pub const RATE_LIMITS_EVENT: &str = "codex.rate_limits";
/// Prefix for client-specific account headers.
pub const CLIENT_HEADER_PREFIX: &str = "x-codex-";
/// Prefix for standard rate-limit headers.
pub const RATE_LIMIT_HEADER_PREFIX: &str = "x-ratelimit-";
/// Plan-type header.
pub const PLAN_TYPE_HEADER: &str = "x-codex-plan-type";
/// Credit-availability header.
pub const CREDITS_HAS_HEADER: &str = "x-codex-credits-has-credits";
/// Unlimited-credit header.
pub const CREDITS_UNLIMITED_HEADER: &str = "x-codex-credits-unlimited";
/// Credit-balance header.
pub const CREDITS_BALANCE_HEADER: &str = "x-codex-credits-balance";
