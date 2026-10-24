//! Adapters for coding-agent configuration files, including Claude Code, Codex,
//! Gemini CLI, and OpenCode. An adapter plans edits that point an agent at a model
//! served by hofund, then applies that plan to the agent's own files. This crate
//! depends only on `config` and must never depend on the gateway.

// Referenced so this crate's manifest dependency is compiled, not merely declared.
use hofund_config as _;
