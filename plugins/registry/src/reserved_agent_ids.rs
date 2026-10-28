/// Agent ids reserved for the const built-in adapters.
///
/// Every built-in agent is now a compiled `agent.toml` plugin, so no id is
/// reserved; a plugin that reuses a shipped id is rejected as a collision.
pub const RESERVED_AGENT_IDS: &[&str] = &[];
