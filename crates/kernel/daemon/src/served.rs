//! Keys that have answered a request, read from the usage ledger.
//!
//! A failed read notes nothing. Unknown is not "it worked".

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_plugin_api::MemberId;
use hofund_router::Cooldowns;
use hofund_router::quota::SERVED_RETENTION;
use hofund_usage::Ledger;

use crate::membership;

/// Notes configured members whose ledger rows show an answer since the
/// retention window. A query error notes nothing.
///
/// `current_keys` maps each single-key member to the key id it is using now.
/// A provider-level member is answered only by a row for that key.
pub(crate) fn load(
    cooldowns: &Cooldowns,
    configured: &HashSet<MemberId>,
    current_keys: &HashMap<MemberId, String>,
    usage: &Ledger,
) {
    let since = since_unix(SystemTime::now());
    match usage.answered_keys(since) {
        Ok(rows) => note(cooldowns, configured, current_keys, &rows),
        Err(error) => {
            tracing::warn!(%error, "served answers were not read");
        }
    }
}

/// Records the key a one-key provider is using.
///
/// A provider with several keys, or with no key, is left out. Those members
/// are named by [`note`] from the row's own key id.
pub(crate) fn remember_single_key(
    keys: &mut HashMap<MemberId, String>,
    provider_id: &str,
    several: bool,
    key_id: &str,
) {
    if several || key_id.is_empty() {
        return;
    }
    keys.insert(membership::member_id(provider_id), key_id.to_owned());
}

/// Notes each configured member named by an answered `(upstream, key id)` row.
///
/// The provider-level member is noted only when `key_id` is the key that
/// member is using now. A different key id is not an answer. A key id that
/// is not kebab-case is skipped.
pub(crate) fn note(
    cooldowns: &Cooldowns,
    configured: &HashSet<MemberId>,
    current_keys: &HashMap<MemberId, String>,
    rows: &[(String, String)],
) {
    for (upstream, key_id) in rows {
        let provider_member = membership::member_id(upstream);
        if configured.contains(&provider_member)
            && current_keys
                .get(&provider_member)
                .is_some_and(|current| current == key_id)
        {
            cooldowns.note_answered(&provider_member);
        }
        if key_id.is_empty() {
            continue;
        }
        let Some(id) = membership::try_key_member_id(upstream, key_id) else {
            continue;
        };
        if configured.contains(&id) {
            cooldowns.note_answered(&id);
        }
    }
}

fn since_unix(now: SystemTime) -> i64 {
    now.checked_sub(SERVED_RETENTION)
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use hofund_plugin_api::PluginError;
    use hofund_router::quota::SERVED_RETENTION;
    use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns};
    use hofund_usage::{Ledger, Row};

    use super::{load, note, remember_single_key};
    use crate::membership::{key_member_id, member_id};

    fn book() -> Cooldowns {
        Cooldowns::with_clock(CooldownPolicy::default(), Arc::new(std::time::Instant::now))
    }

    fn marked() -> PluginError {
        PluginError::status_error(401, "denied").subscription_suspended()
    }

    #[test]
    fn d240_answered_rows_note_only_configured_members() {
        let cooldowns = book();
        let single = member_id("solo");
        let kept = key_member_id("pool", "aaaa");
        let other = key_member_id("pool", "bbbb");
        let configured = HashSet::from([single.clone(), kept.clone(), other.clone()]);
        let mut current = HashMap::new();
        remember_single_key(&mut current, "solo", false, "1111111111");
        note(
            &cooldowns,
            &configured,
            &current,
            &[
                ("solo".to_owned(), "1111111111".to_owned()),
                ("pool".to_owned(), "aaaa".to_owned()),
                ("gone".to_owned(), "cccc".to_owned()),
            ],
        );
        let solo = cooldowns.record_failure(&single, &marked()).expect("solo");
        assert_eq!(solo.cause(), CooldownCause::Suspended);
        let pool = cooldowns.record_failure(&kept, &marked()).expect("kept");
        assert_eq!(pool.cause(), CooldownCause::Suspended);
        let unused = cooldowns.record_failure(&other, &marked()).expect("other");
        assert_eq!(unused.cause(), CooldownCause::Failure);
        assert!(!unused.background_probe());
    }

    fn unix_now() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_secs(),
        )
        .expect("unix")
    }

    fn window_start(now: i64) -> i64 {
        now.saturating_sub(i64::try_from(SERVED_RETENTION.as_secs()).expect("window"))
    }

    fn answered_row(now: i64, key: &str) -> Row {
        Row {
            created_unix: now,
            upstream: "solo".to_owned(),
            provider_key_id: key.to_owned(),
            status: "200".to_owned(),
            ..Row::default()
        }
    }

    /// Both rows sit inside the retention window. The old key alone must not
    /// answer the member, and the current key's row must. An empty query fails
    /// before either cooldown is recorded.
    #[test]
    fn d240_an_old_key_row_does_not_answer_the_current_single_key() {
        let now = unix_now();
        let since = window_start(now);
        let single = member_id("solo");
        let configured = HashSet::from([single.clone()]);
        let mut current = HashMap::new();
        remember_single_key(&mut current, "solo", false, "newkey0002");

        let old_dir = tempfile::tempdir().expect("dir");
        let old_ledger = Ledger::open(&old_dir.path().join("usage.sqlite")).expect("ledger");
        old_ledger
            .record(&answered_row(now, "oldkey0001"))
            .expect("old row");
        let old_rows = old_ledger.answered_keys(since).expect("old query");
        assert!(
            old_rows
                .iter()
                .any(|(upstream, key)| upstream == "solo" && key == "oldkey0001"),
            "an empty query must not satisfy the test"
        );
        let cooldowns = book();
        load(&cooldowns, &configured, &current, &old_ledger);
        let old = cooldowns
            .record_failure(&single, &marked())
            .expect("old key");
        assert_eq!(old.cause(), CooldownCause::Failure);
        assert!(!old.background_probe());

        let live_dir = tempfile::tempdir().expect("dir");
        let live_ledger = Ledger::open(&live_dir.path().join("usage.sqlite")).expect("ledger");
        live_ledger
            .record(&answered_row(now, "oldkey0001"))
            .expect("old row");
        live_ledger
            .record(&answered_row(now, "newkey0002"))
            .expect("current row");
        let live_rows = live_ledger.answered_keys(since).expect("current query");
        assert!(
            live_rows
                .iter()
                .any(|(upstream, key)| upstream == "solo" && key == "oldkey0001")
                && live_rows
                    .iter()
                    .any(|(upstream, key)| upstream == "solo" && key == "newkey0002"),
            "an empty query must not satisfy the test"
        );
        let cooldowns = book();
        load(&cooldowns, &configured, &current, &live_ledger);
        let live = cooldowns
            .record_failure(&single, &marked())
            .expect("current key");
        assert_eq!(live.cause(), CooldownCause::Suspended);
        assert!(live.background_probe());
    }

    #[test]
    fn d240_a_non_kebab_ledger_key_is_skipped() {
        let cooldowns = book();
        let single = member_id("solo");
        let configured = HashSet::from([single.clone()]);
        let mut current = HashMap::new();
        remember_single_key(&mut current, "solo", false, "1111111111");
        note(
            &cooldowns,
            &configured,
            &current,
            &[("solo".to_owned(), "Not_A_Key".to_owned())],
        );
        let cooling = cooldowns.record_failure(&single, &marked()).expect("rest");
        assert_eq!(cooling.cause(), CooldownCause::Failure);
        assert!(!cooling.background_probe());
    }

    #[test]
    fn d240_several_keys_are_not_stored_as_the_provider_member() {
        let mut keys = HashMap::new();
        remember_single_key(&mut keys, "pool", true, "aaaa");
        remember_single_key(&mut keys, "empty", false, "");
        assert!(keys.is_empty());
    }
}
