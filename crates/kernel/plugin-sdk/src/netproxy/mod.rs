//! Outbound proxy helpers for Bun-based CLIs (Claude Code).
//!
//! Bun accepts only `http://` and `https://` proxies. SOCKS5 addresses are
//! bridged to loopback HTTP proxies, matching the reference `EnvForBunWith`
//! behavior.

mod bridge;
mod env;

pub use env::{ProxyError, env_for_bun_with, env_with, for_bun, is_socks, parse_proxy};
