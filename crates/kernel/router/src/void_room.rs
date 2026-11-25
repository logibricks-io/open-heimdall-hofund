//! Process-local reply room learned from length cuts without visible output.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
    pub(crate) fn get(&self, provider: &str, model: &str) -> Option<u64> {
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

#[cfg(test)]
mod tests {
    use super::{UNKNOWN_ROOM, VoidRooms, fitted_room};

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
}
