//! Resolves and stores upstream credentials such as API keys and account tokens
//! outside ordinary configuration files. The daemon asks this crate for secret
//! material and passes it downward. This crate does not open network connections
//! and does not speak an LLM protocol.
