//! Plugin boundary checks shared by `cargo xtask check-plugins` and the tests.
//!
//! The type system and the Cargo graph are the boundary. `std::process` and
//! `std::net` are denied by Clippy after macro expansion. A plugin may link
//! only what `hofund-plugin-api` and `hofund-plugin-sdk` already link. The
//! source scan is a second layer for paths, `include!`, and `#[path]`.

pub mod boundary;
pub mod registry;
