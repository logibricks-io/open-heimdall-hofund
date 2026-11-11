//! Consent-gated access to coding-agent session records.
//!
//! Agent plugins locate and parse files. This crate owns consent, path
//! containment, parser input bounds, and the shared session index.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

pub mod consent;
pub mod open;
