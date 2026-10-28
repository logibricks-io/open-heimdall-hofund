use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MAX_BOOT_ID_LEN, ResyncReason, Topic};

/// One topic sequence in a composite SSE id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventCursor {
    /// Sequence space.
    pub topic: Topic,
    /// Last delivered sequence.
    pub seq: u64,
}

impl EventCursor {
    /// Constructs a cursor.
    #[must_use]
    pub const fn new(topic: Topic, seq: u64) -> Self {
        Self { topic, seq }
    }
}

/// Canonical composite SSE id. Fields are private so every value displayed by
/// this type has passed the same grammar checks as the parser. Boot ids use
/// nonempty ASCII graphic bytes (`0x21..=0x7e`) except `:`, `,`, and `=`, up to
/// 64 bytes. Control bytes are rejected because SSE ids cannot carry them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventId {
    boot_id: String,
    cursors: Vec<EventCursor>,
}

impl EventId {
    /// Constructs a canonical id after enforcing the complete grammar.
    pub fn new(
        boot_id: impl Into<String>,
        cursors: Vec<EventCursor>,
    ) -> Result<Self, EventIdError> {
        let id = Self {
            boot_id: boot_id.into(),
            cursors,
        };
        validate_boot_id(&id.boot_id)?;
        validate_cursors(&id.cursors)?;
        Ok(id)
    }

    /// Parses a canonical composite id.
    pub fn parse(value: &str) -> Result<Self, EventIdError> {
        value.parse()
    }

    /// Daemon boot identity.
    #[must_use]
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Cursors in canonical topic order.
    #[must_use]
    pub fn cursors(&self) -> &[EventCursor] {
        &self.cursors
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.boot_id)?;
        formatter.write_str(":")?;
        for (index, cursor) in self.cursors.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{}={}", cursor.topic.as_str(), cursor.seq)?;
        }
        Ok(())
    }
}

impl FromStr for EventId {
    type Err = EventIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(EventIdError::Empty);
        }
        let Some((boot_id, cursor_text)) = value.split_once(':') else {
            return Err(EventIdError::MissingColon);
        };
        validate_boot_id(boot_id)?;
        if cursor_text.is_empty() {
            return Err(EventIdError::BadCursor);
        }
        let mut cursors = Vec::new();
        for segment in cursor_text.split(',') {
            if segment.is_empty() {
                return Err(EventIdError::BadCursor);
            }
            let Some((topic, sequence)) = segment.split_once('=') else {
                return Err(EventIdError::BadCursor);
            };
            if topic.is_empty() || sequence.is_empty() || sequence.contains('=') {
                return Err(EventIdError::BadCursor);
            }
            let topic = match topic {
                "control" => Topic::Control,
                "requests" => Topic::Requests,
                _ => return Err(EventIdError::UnknownTopic),
            };
            if sequence.len() > 1 && sequence.starts_with('0')
                || !sequence.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(EventIdError::BadSequence);
            }
            let seq = sequence
                .parse::<u64>()
                .map_err(|_| EventIdError::BadSequence)?;
            cursors.push(EventCursor { topic, seq });
        }
        validate_cursors(&cursors)?;
        Ok(Self {
            boot_id: boot_id.to_owned(),
            cursors,
        })
    }
}

/// Composite SSE id parse or construction failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EventIdError {
    /// The complete id is empty.
    #[error("event id is empty")]
    Empty,
    /// No boot-id/cursor colon is present.
    #[error("event id is missing its colon")]
    MissingColon,
    /// The boot id violates its byte grammar.
    #[error("event id has an invalid boot id")]
    BadBootId,
    /// A cursor segment is empty or has the wrong shape.
    #[error("event id has an invalid cursor")]
    BadCursor,
    /// A cursor names a topic outside the canonical set.
    #[error("event id names an unknown topic")]
    UnknownTopic,
    /// A sequence is not canonical unsigned decimal or overflows `u64`.
    #[error("event id has an invalid sequence")]
    BadSequence,
    /// One topic occurs more than once.
    #[error("event id repeats a topic")]
    Duplicate,
    /// Cursors are not in canonical topic order.
    #[error("event id cursors are out of order")]
    OutOfOrder,
}

pub(crate) fn validate_boot_id(boot_id: &str) -> Result<(), EventIdError> {
    if boot_id.is_empty()
        || boot_id.len() > MAX_BOOT_ID_LEN
        || !boot_id
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && !matches!(byte, b':' | b',' | b'='))
    {
        return Err(EventIdError::BadBootId);
    }
    Ok(())
}

fn validate_cursors(cursors: &[EventCursor]) -> Result<(), EventIdError> {
    if cursors.is_empty() {
        return Err(EventIdError::BadCursor);
    }
    let mut previous = None;
    for cursor in cursors {
        let rank = match cursor.topic {
            Topic::Control => 0,
            Topic::Requests => 1,
            Topic::Unknown => return Err(EventIdError::UnknownTopic),
        };
        if previous == Some(rank) {
            return Err(EventIdError::Duplicate);
        }
        if previous.is_some_and(|previous| previous > rank) {
            return Err(EventIdError::OutOfOrder);
        }
        previous = Some(rank);
    }
    Ok(())
}

/// Resume decision for one requested topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resume {
    /// Start with a fresh snapshot on a new connection.
    Fresh,
    /// Replay events after this sequence when retained.
    After(u64),
    /// Replace this topic's state from a fresh snapshot.
    Resync(ResyncReason),
}

/// Computes per-topic replay decisions without performing I/O.
///
/// A malformed id resynchronizes every requested topic. Unknown boot ids do
/// the same, while a missing topic affects only that topic. Unrequested
/// cursors are ignored and requested topics are deduplicated in canonical
/// order. Retention checks are deliberately deferred to the event server:
/// this helper cannot know whether an `After` cursor remains in the ring.
#[must_use]
pub fn resume_plan(
    last_event_id: Option<&str>,
    boot_id: &str,
    requested: &[Topic],
) -> Vec<(Topic, Resume)> {
    let topics = canonical_requested(requested);
    let Some(last_event_id) = last_event_id else {
        return topics
            .into_iter()
            .map(|topic| (topic, Resume::Fresh))
            .collect();
    };
    let Ok(id) = EventId::parse(last_event_id) else {
        return resync_all(&topics, ResyncReason::MalformedId);
    };
    if id.boot_id != boot_id {
        return resync_all(&topics, ResyncReason::UnknownBoot);
    }
    topics
        .into_iter()
        .map(|topic| {
            let resume = id
                .cursors
                .iter()
                .find(|cursor| cursor.topic == topic)
                .map_or(Resume::Resync(ResyncReason::MissingTopic), |cursor| {
                    Resume::After(cursor.seq)
                });
            (topic, resume)
        })
        .collect()
}

fn canonical_requested(requested: &[Topic]) -> Vec<Topic> {
    Topic::CANONICAL
        .into_iter()
        .filter(|topic| requested.contains(topic))
        .collect()
}

fn resync_all(topics: &[Topic], reason: ResyncReason) -> Vec<(Topic, Resume)> {
    topics
        .iter()
        .map(|topic| (*topic, Resume::Resync(reason)))
        .collect()
}
