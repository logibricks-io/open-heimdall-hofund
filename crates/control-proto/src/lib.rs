//! Transport-independent DTOs for the versioned hofund control protocol.
//!
//! Requests reject unknown fields. Responses and events accept additive fields
//! and use unknown enum fallbacks so older clients can read newer peers.

mod agents;
mod checkins;
mod common;
mod config;
mod decode;
mod error;
mod event_id;
mod events;
mod gateway_keys;
mod groups;
mod limits;
mod models;
mod presets;
mod profiles;
mod provider_keys;
mod providers;
mod quotas;
mod route_view;
mod shutdown;
mod spend_caps;
mod status;
mod token;
mod usage;
mod version;
mod warmups;

pub use agents::*;
pub use checkins::*;
pub use common::*;
pub use config::*;
pub use decode::*;
pub use error::*;
pub use event_id::*;
pub use events::*;
pub use gateway_keys::*;
pub use groups::*;
pub use limits::*;
pub use models::*;
pub use presets::*;
pub use profiles::*;
pub use provider_keys::*;
pub use providers::*;
pub use quotas::*;
pub use route_view::*;
pub use shutdown::*;
pub use spend_caps::*;
pub use status::*;
pub use token::*;
pub use usage::*;
pub use version::*;
pub use warmups::*;
