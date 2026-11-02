//! Per-conversation memory of context-overflow refusals by member.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::Hop;

/// How long an overflow refusal is remembered.
pub const OVERFLOW_KEEP: Duration = Duration::from_secs(60 * 60);
/// Maximum remembered conversation and member pairs.
pub const OVERFLOW_CAP: usize = 512;

/// Per-conversation memory of which members refused for context length.
#[derive(Clone)]
pub struct OverflowMemory {
    inner: Arc<Mutex<State>>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl Default for OverflowMemory {
    fn default() -> Self {
        Self::in_memory()
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<(String, String), Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    at: SystemTime,
    refused_tokens: u64,
}

impl OverflowMemory {
    /// In-memory overflow memory using the system clock.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::with_clock(Arc::new(SystemTime::now))
    }

    /// Overflow memory with an injected clock, for deterministic tests.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Fn() -> SystemTime + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
            clock,
        }
    }

    /// Reorders `hops` so members that refused conversation `key` for its
    /// length are asked last, keeping the relative order within each part.
    /// Expired entries are dropped first, and a request smaller than the one a
    /// member refused clears that member's entry for `key`.
    #[must_use]
    pub fn demote_hops(&self, key: &str, input_tokens: u64, hops: Vec<Hop>) -> Vec<Hop> {
        let demote = self.prune_and_list(key, input_tokens);
        if demote.is_empty() {
            return hops;
        }
        let (mut front, back): (Vec<Hop>, Vec<Hop>) = hops
            .into_iter()
            .partition(|hop| !demote.contains(&hop.member.id().to_string()));
        front.extend(back);
        front
    }

    /// Records that `member` refused `key` at `refused_tokens`.
    pub fn record(&self, key: &str, member: &str, refused_tokens: u64) {
        let now = (self.clock)();
        let mut state = self.inner.lock().expect("overflow memory");
        state.entries.insert(
            (key.to_owned(), member.to_owned()),
            Entry {
                at: now,
                refused_tokens,
            },
        );
        while state.entries.len() > OVERFLOW_CAP {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                state.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Clears overflow memory for one member and conversation.
    pub fn clear(&self, key: &str, member: &str) {
        let mut state = self.inner.lock().expect("overflow memory");
        state.entries.remove(&(key.to_owned(), member.to_owned()));
    }

    fn prune_and_list(&self, key: &str, input_tokens: u64) -> Vec<String> {
        let now = (self.clock)();
        let mut state = self.inner.lock().expect("overflow memory");
        state.entries.retain(|(entry_key, _), entry| {
            !expired(entry.at, now) && !(entry_key == key && input_tokens < entry.refused_tokens)
        });
        state
            .entries
            .keys()
            .filter(|(entry_key, _)| entry_key == key)
            .map(|(_, member)| member.clone())
            .collect()
    }
}

fn expired(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).unwrap_or(Duration::ZERO) > OVERFLOW_KEEP
}

#[cfg(test)]
mod tests {
    use super::{OVERFLOW_CAP, OVERFLOW_KEEP, OverflowMemory};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn d38_7_bound_drops_the_oldest_entry() {
        let start = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = Arc::new(Mutex::new(start));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().expect("clock"))
        };
        let memory = OverflowMemory::with_clock(clock);
        for index in 0..OVERFLOW_CAP {
            *now.lock().expect("clock") += Duration::from_secs(1);
            memory.record(&format!("key-{index}"), "member", 100);
        }
        assert_eq!(
            memory.prune_and_list("key-0", 100).len(),
            1,
            "oldest key still remembered"
        );
        *now.lock().expect("clock") += Duration::from_secs(1);
        memory.record("key-new", "member", 100);
        assert!(
            memory.prune_and_list("key-0", 100).is_empty(),
            "oldest entry dropped at cap"
        );
        assert_eq!(memory.prune_and_list("key-new", 100).len(), 1);
    }

    #[test]
    fn d38_8_expiry_drops_stale_entries() {
        let start = UNIX_EPOCH + Duration::from_secs(10_000);
        let now = Arc::new(Mutex::new(start));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().expect("clock"))
        };
        let memory = OverflowMemory::with_clock(clock);
        memory.record("demo|one", "first", 500);
        *now.lock().expect("clock") += OVERFLOW_KEEP + Duration::from_secs(1);
        assert!(memory.prune_and_list("demo|one", 500).is_empty());
    }

    #[test]
    fn d38_11_an_entry_is_kept_for_the_whole_hour() {
        let start = UNIX_EPOCH + Duration::from_secs(10_000);
        let now = Arc::new(Mutex::new(start));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().expect("clock"))
        };
        let memory = OverflowMemory::with_clock(clock);
        memory.record("demo|one", "first", 500);
        *now.lock().expect("clock") += OVERFLOW_KEEP;
        assert_eq!(memory.prune_and_list("demo|one", 500), ["first"]);
    }

    #[test]
    fn d38_12_a_smaller_request_clears_the_entry_for_good() {
        let memory = OverflowMemory::in_memory();
        memory.record("demo|one", "first", 500);
        memory.record("demo|two", "first", 500);
        assert!(memory.prune_and_list("demo|one", 499).is_empty());
        assert!(memory.prune_and_list("demo|one", 900).is_empty());
        assert_eq!(memory.prune_and_list("demo|two", 900), ["first"]);
    }
}
