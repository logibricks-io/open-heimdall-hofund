//! One-card quota refresh with a per-card vendor gap.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use hofund_config::Config;
use hofund_control_proto::Quota;

/// How soon the same card may be read from its vendor again.
pub(crate) const REFRESH_GAP: Duration = Duration::from_secs(30);

/// When each card was last read on its own through refresh.
#[derive(Default)]
pub(crate) struct RefreshStamps {
    stamps: HashMap<String, SystemTime>,
}

impl RefreshStamps {
    /// Whether the card is still inside the refresh gap; returns how long ago it was asked.
    pub(crate) fn within_gap(
        &self,
        provider: &str,
        user: &str,
        now: SystemTime,
    ) -> Option<Duration> {
        let key = stamp_key(provider, user);
        let at = *self.stamps.get(&key)?;
        let ago = now.duration_since(at).unwrap_or(Duration::ZERO);
        if ago < REFRESH_GAP { Some(ago) } else { None }
    }

    /// Records that the card was asked at `now`, dropping older stamps.
    pub(crate) fn note(&mut self, provider: &str, user: &str, now: SystemTime) {
        self.stamps
            .retain(|_, at| now.duration_since(*at).unwrap_or(Duration::ZERO) < REFRESH_GAP);
        self.stamps.insert(stamp_key(provider, user), now);
    }

    /// Test hook: set a stamp without a vendor read.
    #[cfg(test)]
    pub(crate) fn set_at(&mut self, provider: &str, user: &str, at: SystemTime) {
        self.stamps.insert(stamp_key(provider, user), at);
    }
}

fn stamp_key(provider: &str, user: &str) -> String {
    format!(
        "{}|{}",
        provider.to_ascii_lowercase(),
        user.to_ascii_lowercase()
    )
}

/// Whether `card` is the card a refresh asked for.
pub(crate) fn card_matches(card: &Quota, provider: &str, user: &str) -> bool {
    if !card.provider.eq_ignore_ascii_case(provider) {
        return false;
    }
    if user.is_empty() {
        return true;
    }
    card.user.eq_ignore_ascii_case(user)
        || (!card.alias.is_empty() && card.alias.eq_ignore_ascii_case(user))
}

/// Canonical provider or subscription id from config when `name` matches one.
pub(crate) fn resolve_id(config: &Config, name: &str) -> Option<String> {
    config
        .providers
        .iter()
        .find(|entry| entry.id.eq_ignore_ascii_case(name))
        .map(|entry| entry.id.clone())
        .or_else(|| {
            config
                .subscriptions
                .iter()
                .find(|entry| entry.id.eq_ignore_ascii_case(name))
                .map(|entry| entry.id.clone())
        })
}

/// Whether a provider key name or id matches the refresh account filter.
pub(crate) fn key_matches(id: &str, name: &str, user: Option<&str>) -> bool {
    match user {
        None | Some("") => true,
        Some(want) => {
            id.eq_ignore_ascii_case(want) || (!name.is_empty() && name.eq_ignore_ascii_case(want))
        }
    }
}

#[cfg(test)]
mod tests {
    use hofund_control_proto::{Quota, QuotaKind};

    use super::*;

    #[test]
    fn d38_stamp_key_is_case_folded() {
        assert_eq!(stamp_key("Relay", "Me@x.io"), "relay|me@x.io");
    }

    #[test]
    fn d38_within_gap_honours_the_interval() {
        let mut stamps = RefreshStamps::default();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        stamps.set_at("relay", "", at);
        let now = at + Duration::from_secs(10);
        assert_eq!(
            stamps.within_gap("relay", "", now),
            Some(Duration::from_secs(10))
        );
        let now = at + REFRESH_GAP;
        assert!(stamps.within_gap("relay", "", now).is_none());
    }

    #[test]
    fn d38_card_matches_provider_alias_and_account() {
        let mut card = Quota {
            provider: "codex".to_owned(),
            user: "a@x.io".to_owned(),
            alias: "work".to_owned(),
            kind: QuotaKind::Subscription,
            ..Quota::default()
        };
        assert!(card_matches(&card, "codex", ""));
        assert!(card_matches(&card, "Codex", "a@x.io"));
        assert!(card_matches(&card, "codex", "work"));
        assert!(!card_matches(&card, "codex", "other"));
        assert!(!card_matches(&card, "claude", ""));
        card.alias.clear();
        assert!(!card_matches(&card, "codex", "work"));
    }
}
