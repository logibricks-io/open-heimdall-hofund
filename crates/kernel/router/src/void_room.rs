//! Process-local reply room learned from length cuts without visible output.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;

pub(crate) const UNKNOWN_ROOM: u64 = 32_768;
const LEAST_REPLY_LEFT: u64 = 8_192;

#[derive(Clone, Copy)]
enum Room {
    Known(u64),
    Refused,
}

/// Reply limits learned for provider and model pairs.
///
/// The daemon shares this value across router snapshots. It is deliberately
/// process-local: a restart lets the upstream's default be observed again.
#[derive(Clone, Default)]
pub struct VoidRooms {
    rooms: Arc<Mutex<BTreeMap<(String, String), Room>>>,
}

impl VoidRooms {
    /// The reply limit still asked for this provider and model.
    ///
    /// `None` means no room was learned, or the upstream refused the field
    /// and the room was tombstoned.
    #[must_use]
    pub fn get(&self, provider: &str, model: &str) -> Option<u64> {
        match self
            .rooms
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(provider.to_owned(), model.to_owned()))
            .copied()
        {
            Some(Room::Known(room)) => Some(room),
            Some(Room::Refused) | None => None,
        }
    }

    pub(crate) fn learn(
        &self,
        provider: &str,
        model: &str,
        catalog_output: Option<u64>,
        spent: u64,
    ) -> Option<u64> {
        let room = match catalog_output.filter(|output| *output > 0) {
            Some(output) if output > spent => output,
            Some(_) => return None,
            None if spent < UNKNOWN_ROOM => UNKNOWN_ROOM,
            None => return None,
        };
        let key = (provider.to_owned(), model.to_owned());
        let mut rooms = self.rooms.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(rooms.get(&key), Some(Room::Refused)) {
            return None;
        }
        rooms.insert(key, Room::Known(room));
        Some(room)
    }

    pub(crate) fn forget(&self, provider: &str, model: &str) {
        self.rooms
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert((provider.to_owned(), model.to_owned()), Room::Refused);
    }
}

pub(crate) fn fitted_room(room: u64, window: Option<u64>, prompt_tokens: u64) -> u64 {
    let Some(window) = window.filter(|window| *window > 0) else {
        return room;
    };
    let reserved = prompt_tokens.saturating_mul(5) / 4;
    let available = window.saturating_sub(reserved).saturating_sub(1024);
    if (LEAST_REPLY_LEFT..room).contains(&available) {
        available
    } else {
        room
    }
}

static WINDOW_AND_PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)maximum context length is (\d+) tokens.{0,200}?(?:\((\d+) in the messages, \d+ in the completion\)|your prompt contains at least (\d+) input tokens)",
    )
    .expect("window and prompt pattern compiles")
});

static INPUT_PLUS_REPLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)input length and .{0,3}max_tokens.{0,3} exceed context limit:\s*(\d+)\s*\+\s*\d+\s*>\s*(\d+)",
    )
    .expect("input plus reply pattern compiles")
});

/// Tokens left for a reply after a refusal that counts the prompt and the
/// asked reply inside the window. Zero when the body is not that accounting,
/// or the prompt alone fills the window.
pub(crate) fn reply_left(message: &str) -> u64 {
    if let Some(caps) = WINDOW_AND_PROMPT.captures(message) {
        let window = number_at(&caps, 1);
        let prompt = match caps.get(2) {
            Some(prompt) => parse_number(prompt.as_str()),
            None => number_at(&caps, 3),
        };
        return leftover(window, prompt);
    }
    if let Some(caps) = INPUT_PLUS_REPLY.captures(message) {
        return leftover(number_at(&caps, 2), number_at(&caps, 1));
    }
    0
}

fn number_at(caps: &regex::Captures<'_>, index: usize) -> u64 {
    caps.get(index)
        .map(|matched| parse_number(matched.as_str()))
        .unwrap_or(0)
}

fn parse_number(text: &str) -> u64 {
    text.parse().unwrap_or(0)
}

fn leftover(window: u64, prompt: u64) -> u64 {
    if window == 0 || prompt == 0 || prompt >= window {
        0
    } else {
        window - prompt
    }
}

#[cfg(test)]
mod tests {
    use super::{UNKNOWN_ROOM, VoidRooms, fitted_room, reply_left};

    #[test]
    fn d252_room_uses_catalog_above_spent_then_unknown_and_refusal_tombstone() {
        let rooms = VoidRooms::default();
        assert_eq!(
            rooms.learn("one", "model", Some(65_536), 8_192),
            Some(65_536)
        );
        assert_eq!(rooms.get("one", "model"), Some(65_536));
        assert_eq!(rooms.learn("one", "model", Some(8_192), 8_192), None);
        assert_eq!(rooms.learn("two", "model", None, 8_192), Some(UNKNOWN_ROOM));
        assert_eq!(rooms.learn("two", "model", None, UNKNOWN_ROOM), None);
        rooms.forget("two", "model");
        assert_eq!(rooms.get("two", "model"), None);
        assert_eq!(rooms.learn("two", "model", None, 8_192), None);
    }

    #[test]
    fn d252_room_is_fitted_to_context_without_hiding_small_context() {
        assert_eq!(fitted_room(65_536, Some(100_000), 8_000), 65_536);
        assert_eq!(fitted_room(65_536, Some(60_000), 8_000), 48_976);
        assert_eq!(fitted_room(65_536, Some(12_000), 8_000), 65_536);
        assert_eq!(fitted_room(32_768, None, 100), 32_768);
    }

    #[test]
    fn d252_reply_left_counts_a_window_and_ignores_a_field_refusal() {
        let cases = [
            (
                "This model's maximum context length is 1000000 tokens. However, you requested 1100000 tokens (706784 in the messages, 393216 in the completion). Please reduce the length of the messages or completion.",
                293_216,
            ),
            (
                "This model's maximum context length is 32768 tokens. However, you requested 32000 output tokens and your prompt contains at least 769 input tokens, for a total of at least 32769 tokens.",
                31_999,
            ),
            (
                "input length and `max_tokens` exceed context limit: 970000 + 64000 > 1000000, decrease input length or `max_tokens` and try again",
                30_000,
            ),
            (
                "input length and max_tokens exceed context limit: 197202 + 21333 > 200000",
                2_798,
            ),
            (
                "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens (140000 in the messages, 0 in the completion).",
                0,
            ),
            (
                "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens",
                0,
            ),
            (
                "Invalid max_tokens value, the valid range of max_tokens is [1, 8192]",
                0,
            ),
            ("max_tokens must be at most 16384", 0),
        ];
        for (message, wanted) in cases {
            assert_eq!(reply_left(message), wanted, "{message}");
        }
    }
}
