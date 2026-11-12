//! Client keepalive during a quiet Claude Code think phase (R-6f).
//!
//! Mirrors `keepBridgeHeard`: after `bridgeHeardAfter` (1 minute), while
//! quiet for at least `keepaliveEvery` (15 s) and less than
//! `translatedQuietLongest` (30 minutes), emit one placeholder
//! `MessageStart` if nothing has committed yet, then `StillWorking` on
//! each tick.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use hofund_plugin_api::{MessageStart, StillWorking, StreamEvent};

/// Reference `keepaliveEvery`.
pub(crate) const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);
/// Reference `bridgeHeardAfter`.
pub(crate) const BRIDGE_HEARD_AFTER: Duration = Duration::from_secs(60);
/// Reference `translatedQuietLongest`.
pub(crate) const TRANSLATED_QUIET_LONGEST: Duration = Duration::from_secs(30 * 60);

/// Placeholder id used on the first keepalive `MessageStart`.
pub(crate) const PLACEHOLDER_ID: &str = "msg_still_working";
/// Placeholder model used on the first keepalive `MessageStart`.
pub(crate) const PLACEHOLDER_MODEL: &str = "placeholder";

/// Timing knobs, injectable in tests.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeepaliveTiming {
    pub every: Duration,
    pub heard_after: Duration,
    pub quiet_longest: Duration,
}

impl Default for KeepaliveTiming {
    fn default() -> Self {
        Self {
            every: KEEPALIVE_EVERY,
            heard_after: BRIDGE_HEARD_AFTER,
            quiet_longest: TRANSLATED_QUIET_LONGEST,
        }
    }
}

/// Events to emit on a quiet tick, if any.
///
/// `started` is whether a `MessageStart` (the run's or a placeholder)
/// has already been sent.
#[must_use]
pub(crate) fn tick(
    began: Instant,
    last: Instant,
    now: Instant,
    started: bool,
    timing: KeepaliveTiming,
) -> Vec<StreamEvent> {
    let since_start = now.saturating_duration_since(began);
    let quiet = now.saturating_duration_since(last);
    if since_start < timing.heard_after || quiet < timing.every || quiet >= timing.quiet_longest {
        return Vec::new();
    }
    let mut out = Vec::new();
    if !started {
        out.push(placeholder_start());
    }
    out.push(StreamEvent::StillWorking(StillWorking::default()));
    out
}

/// Placeholder `MessageStart` with a fixed id/model and zero usage.
#[must_use]
pub(crate) fn placeholder_start() -> StreamEvent {
    StreamEvent::MessageStart(MessageStart {
        id: Some(PLACEHOLDER_ID.into()),
        model: Some(PLACEHOLDER_MODEL.into()),
        usage: None,
        extensions: Default::default(),
    })
}

/// Whether this event counts as the reply having started (`KStart`).
#[must_use]
pub(crate) fn event_starts(event: &StreamEvent) -> bool {
    matches!(event, StreamEvent::MessageStart(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> KeepaliveTiming {
        KeepaliveTiming {
            every: Duration::from_millis(15),
            heard_after: Duration::from_millis(60),
            quiet_longest: Duration::from_millis(300),
        }
    }

    #[test]
    fn nothing_is_emitted_before_heard_after() {
        let began = Instant::now();
        let now = began + Duration::from_millis(50);
        assert!(tick(began, began, now, false, timing()).is_empty());
    }

    #[test]
    fn after_heard_after_a_quiet_tick_emits_placeholder_then_still_working() {
        let began = Instant::now();
        let now = began + Duration::from_millis(80);
        let events = tick(began, began, now, false, timing());
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::MessageStart(start) => {
                assert_eq!(start.id.as_deref(), Some(PLACEHOLDER_ID));
                assert_eq!(start.model.as_deref(), Some(PLACEHOLDER_MODEL));
                assert!(start.usage.is_none());
            }
            other => panic!("expected start, got {other:?}"),
        }
        assert!(matches!(events[1], StreamEvent::StillWorking(_)));
    }

    #[test]
    fn later_ticks_emit_only_still_working() {
        let began = Instant::now();
        let now = began + Duration::from_millis(80);
        let events = tick(began, began, now, true, timing());
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::StillWorking(_)));
    }

    #[test]
    fn quiet_past_the_longest_emits_nothing() {
        let began = Instant::now();
        let now = began + Duration::from_millis(400);
        assert!(tick(began, began, now, false, timing()).is_empty());
    }
}
