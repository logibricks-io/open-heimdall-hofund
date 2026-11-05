//! Gateway handoff (section 4) without embedding the gateway crate.

mod handoff;
mod status_fields;

pub use handoff::{
    HandoffDeps, HandoffOutcome, HandoffState, QuitDecision, SPAWN_WAIT, WATCH_INTERVAL,
    decide_quit, probe_timeout, run_handoff_tick,
};
pub use status_fields::StatusServeFields;
