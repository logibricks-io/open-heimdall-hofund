//! Keys that have answered a request, read from the usage ledger.
//!
//! A failed read notes nothing. Unknown is not "it worked".

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_plugin_api::MemberId;
use hofund_router::Cooldowns;
use hofund_router::quota::SERVED_RETENTION;
use hofund_usage::Ledger;

use crate::membership;

/// Notes configured members whose ledger rows show an answer since the
/// retention window. A query error notes nothing.
pub(crate) fn load(cooldowns: &Cooldowns, configured: &HashSet<MemberId>, usage: &Ledger) {
    let since = since_unix(SystemTime::now());
    match usage.answered_keys(since) {
        Ok(rows) => note(cooldowns, configured, &rows),
        Err(error) => {
            tracing::warn!(%error, "served answers were not read");
        }
    }
}

/// Notes each configured member named by an answered `(upstream, key id)` row.
pub(crate) fn note(
    cooldowns: &Cooldowns,
    configured: &HashSet<MemberId>,
    rows: &[(String, String)],
) {
    for (upstream, key_id) in rows {
        let mut ids = vec![membership::member_id(upstream)];
        if !key_id.is_empty() {
            ids.push(membership::key_member_id(upstream, key_id));
        }
        for id in ids {
            if configured.contains(&id) {
                cooldowns.note_answered(&id);
            }
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
    use std::collections::HashSet;
    use std::sync::Arc;

    use hofund_plugin_api::PluginError;
    use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns};

    use super::note;
    use crate::membership::{key_member_id, member_id};

    fn book() -> Cooldowns {
        Cooldowns::with_clock(CooldownPolicy::default(), Arc::new(std::time::Instant::now))
    }

    #[test]
    fn d240_answered_rows_note_only_configured_members() {
        let cooldowns = book();
        let single = member_id("solo");
        let kept = key_member_id("pool", "aaaa");
        let other = key_member_id("pool", "bbbb");
        let configured = HashSet::from([single.clone(), kept.clone(), other.clone()]);
        note(
            &cooldowns,
            &configured,
            &[
                ("solo".to_owned(), String::new()),
                ("pool".to_owned(), "aaaa".to_owned()),
                ("gone".to_owned(), "cccc".to_owned()),
            ],
        );
        let marked = PluginError::status_error(401, "denied").subscription_suspended();
        let solo = cooldowns.record_failure(&single, &marked).expect("solo");
        assert_eq!(solo.cause(), CooldownCause::Suspended);
        let pool = cooldowns.record_failure(&kept, &marked).expect("kept");
        assert_eq!(pool.cause(), CooldownCause::Suspended);
        let unused = cooldowns.record_failure(&other, &marked).expect("other");
        assert_eq!(unused.cause(), CooldownCause::Failure);
        assert!(!unused.background_probe());
    }
}
