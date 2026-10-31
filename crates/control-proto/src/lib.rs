//! Transport-independent DTOs for the versioned hofund control protocol.
//!
//! Requests reject unknown fields. Responses and events accept additive fields
//! and use unknown enum fallbacks so older clients can read newer peers.

mod agents;
mod common;
mod config;
mod decode;
mod error;
mod event_id;
mod events;
mod limits;
mod models;
mod providers;
mod quotas;
mod shutdown;
mod status;
mod token;
mod usage;
mod version;

pub use agents::*;
pub use common::*;
pub use config::*;
pub use decode::*;
pub use error::*;
pub use event_id::*;
pub use events::*;
pub use limits::*;
pub use models::*;
pub use providers::*;
pub use quotas::*;
pub use shutdown::*;
pub use status::*;
pub use token::*;
pub use usage::*;
pub use version::*;
