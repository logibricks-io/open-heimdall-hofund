//! Pure rules for which account an agent sign-in should use next.
//!
//! The reference keeps these decisions separate from vendor file writes.

use hofund_router::affinity::spent_share_of;

/// One remembered subscription account in user order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginRow {
    /// Lowercase account id or label key used on the CLI.
    pub key: String,
    /// Whether the gateway may route requests to this account.
    pub on: bool,
    /// Whether this is the account the agent is signed in to now.
    pub active: bool,
    /// Whether the account cannot be switched to (setup token, held elsewhere).
    pub switchable: bool,
}

/// Allowance reading used to pick the next sign-in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoginQuota {
    /// A poll succeeded and named windows.
    pub readable: bool,
    /// Highest used share on an account-wide blocking window.
    pub used_share: f64,
    /// Every blocking account-wide window is exhausted.
    pub used_up: bool,
    /// The vendor refused further use until reset.
    pub held: bool,
    /// The poll failed for a reason other than the account itself.
    pub error: bool,
}

/// Policy from subscription config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginPolicy {
    /// Member-ordering strategy name (`smart`, `order`, …).
    pub routing: String,
    /// Stay on the first account instead of moving when it is low.
    pub keep_login: bool,
    /// Only move the agent sign-in, not gateway routing follow-back.
    pub switch_only: bool,
    /// Sign-in spending does not follow gateway Smart routing.
    pub sign_in_spends: bool,
}

impl LoginPolicy {
    /// Whether the agent should return to the account it moved off once that
    /// account has room again. In-order routing and `keep_login` do. A
    /// sign-in that spends the account stays on the account it moved to.
    #[must_use]
    pub fn returns_to_first(&self) -> bool {
        if self.keep_login {
            return true;
        }
        if self.routing == "order" {
            return true;
        }
        if self.sign_in_spends {
            return false;
        }
        !self.switch_only
    }
}

const BACK_SHARE: f64 = 90.0;
const ROOM_MARGIN: f64 = 1.0;

fn spent_at(policy: &LoginPolicy) -> f64 {
    spent_share_of(&policy.routing)
}

fn used_past(quota: &LoginQuota, share: f64) -> bool {
    quota.readable && !quota.error && !quota.held && quota.used_share >= share
}

fn room_left(quota: &LoginQuota, share: f64) -> f64 {
    if !quota.readable || quota.error {
        return f64::NEG_INFINITY;
    }
    share - quota.used_share
}

/// Resolves `reference` to an account key by label, alias, or id (reference
/// commit 149e763e9).
///
/// # Errors
///
/// Returns an error when the reference is ambiguous or matches nothing.
pub fn account_by_name(
    rows: &[LoginRow],
    aliases: &[(String, String)],
    reference: &str,
) -> Result<String, String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("name an account to switch to".to_owned());
    }
    let _lower = reference.to_lowercase();
    let mut by_alias = Vec::new();
    for (key, alias) in aliases {
        if alias.eq_ignore_ascii_case(reference) {
            by_alias.push(key.clone());
        }
    }
    if by_alias.len() > 1 {
        return Err(format!(
            "more than one account is named `{reference}`; use its email or id"
        ));
    }
    if by_alias.len() == 1 {
        return Ok(by_alias[0].clone());
    }
    let mut by_key = Vec::new();
    for row in rows {
        if row.key.eq_ignore_ascii_case(reference) {
            by_key.push(row.key.clone());
        }
    }
    if by_key.len() > 1 {
        return Err(format!("more than one account matches `{reference}`"));
    }
    if by_key.len() == 1 {
        return Ok(by_key[0].clone());
    }
    Err(format!("no account `{reference}` is remembered"))
}

/// Picks the next account for `hofund accounts switch <agent> next` (reference
/// commit 15b83901a, 01813c681).
pub fn next_in_order(
    rows: &[LoginRow],
    quotas: &[(String, LoginQuota)],
    policy: &LoginPolicy,
    include_off: bool,
) -> Result<(String, bool), String> {
    let _active = rows
        .iter()
        .find(|row| row.active)
        .map(|row| row.key.as_str())
        .unwrap_or("");
    let start = rows
        .iter()
        .position(|row| row.active)
        .map_or(0, |index| index + 1);
    let mut order = Vec::new();
    for offset in 0..rows.len() {
        let row = &rows[(start + offset) % rows.len()];
        if row.active || !row.switchable {
            continue;
        }
        if !include_off && !row.on {
            continue;
        }
        order.push(row.key.clone());
    }
    if order.is_empty() {
        return Err("there is no other account to switch to".to_owned());
    }
    let share = spent_at(policy);
    let mut best = String::new();
    let mut most = f64::NEG_INFINITY;
    for key in &order {
        let Some(quota) = quotas.iter().find(|(id, _)| id == key).map(|(_, q)| q) else {
            continue;
        };
        if quota.held || quota.error || !quota.readable {
            continue;
        }
        if !used_past(quota, share) && !quota.used_up {
            return Ok((key.clone(), false));
        }
        let room = room_left(quota, share);
        if room > most {
            best.clone_from(key);
            most = room;
        }
    }
    if best.is_empty() {
        return Err(
            "no reading of the other accounts' usage; name the account to switch to".to_owned(),
        );
    }
    Ok((best, true))
}

/// Whether the active account should move, and where (reference NextLogin,
/// commit 41b5c0f19). The account tick calls this every five minutes.
pub fn next_login(
    rows: &[LoginRow],
    quotas: &[(String, LoginQuota)],
    policy: &LoginPolicy,
    return_key: Option<&str>,
) -> Option<(String, String, bool)> {
    let active = rows.iter().find(|row| row.active)?;
    if policy.keep_login {
        if let Some(first) = rows.first()
            && first.switchable
            && active.key != first.key
        {
            return Some((active.key.clone(), first.key.clone(), true));
        }
        return None;
    }
    let spares: Vec<_> = rows
        .iter()
        .filter(|row| !row.active && row.switchable && row.on)
        .collect();
    if policy.returns_to_first()
        && let Some(back) = return_key
        && let Some(quota) = quotas.iter().find(|(id, _)| id == back).map(|(_, q)| q)
        && quota.readable
        && !quota.error
        && !quota.held
        && quota.used_share < BACK_SHARE
        && !quota.used_up
    {
        return Some((active.key.clone(), back.to_owned(), true));
    }
    let share = spent_at(policy);
    let active_quota = quotas
        .iter()
        .find(|(id, _)| id == &active.key)
        .map(|(_, q)| q);
    let active_quota = active_quota?;
    if !used_past(active_quota, share) && !active_quota.used_up {
        return None;
    }
    let mut best = String::new();
    let mut most = if active_quota.used_up {
        f64::NEG_INFINITY
    } else {
        room_left(active_quota, share) + ROOM_MARGIN
    };
    for row in &spares {
        let Some(quota) = quotas.iter().find(|(id, _)| id == &row.key).map(|(_, q)| q) else {
            continue;
        };
        if quota.held || quota.error || !quota.readable {
            continue;
        }
        if !used_past(quota, share) && !quota.used_up {
            if Some(row.key.as_str()) == return_key && !policy.returns_to_first() {
                continue;
            }
            return Some((active.key.clone(), row.key.clone(), false));
        }
        if Some(row.key.as_str()) == return_key && !policy.returns_to_first() {
            continue;
        }
        let room = room_left(quota, share);
        if room > most {
            best.clone_from(&row.key);
            most = room;
        }
    }
    if best.is_empty() {
        return None;
    }
    Some((active.key.clone(), best, false))
}

/// Account to sign in to before removing the active one (reference a7b47d57e).
pub fn next_on_forget(rows: &[LoginRow], removing: &str) -> Option<String> {
    let active = rows.iter().find(|row| row.active)?;
    if !active.key.eq_ignore_ascii_case(removing) {
        return None;
    }
    let candidates: Vec<_> = rows
        .iter()
        .filter(|row| row.switchable && !row.key.eq_ignore_ascii_case(removing))
        .collect();
    for pick_on in [true, false] {
        for row in &candidates {
            if pick_on && !row.on {
                continue;
            }
            if !pick_on && row.on {
                continue;
            }
            return Some(row.key.clone());
        }
    }
    candidates.first().map(|row| row.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, on: bool, active: bool) -> LoginRow {
        LoginRow {
            key: key.to_owned(),
            on,
            active,
            switchable: true,
        }
    }

    fn quota(used: f64) -> LoginQuota {
        LoginQuota {
            readable: true,
            used_share: used,
            used_up: used >= 100.0,
            held: false,
            error: false,
        }
    }

    #[test]
    fn d27_1_next_in_order_picks_room_then_most_left() {
        let rows = vec![
            row("a", true, true),
            row("b", true, false),
            row("c", true, false),
        ];
        let quotas = vec![
            ("a".to_owned(), quota(99.0)),
            ("b".to_owned(), quota(50.0)),
            ("c".to_owned(), quota(97.0)),
        ];
        let policy = LoginPolicy {
            routing: "smart".to_owned(),
            keep_login: false,
            switch_only: false,
            sign_in_spends: true,
        };
        let (to, over) = next_in_order(&rows, &quotas, &policy, true).expect("next");
        assert_eq!(to, "b");
        assert!(!over);
        let all_spent = vec![
            ("a".to_owned(), quota(99.0)),
            ("b".to_owned(), quota(99.5)),
            ("c".to_owned(), quota(99.0)),
        ];
        let (to, over) = next_in_order(&rows, &all_spent, &policy, true).expect("most left");
        assert_eq!(to, "c");
        assert!(over);
    }

    #[test]
    fn d27_2_alias_resolves_account_ref() {
        let rows = vec![row("one@example.test", true, false)];
        let aliases = vec![("one@example.test".to_owned(), "work".to_owned())];
        assert_eq!(
            account_by_name(&rows, &aliases, "work").expect("alias"),
            "one@example.test"
        );
        assert_eq!(
            account_by_name(&rows, &aliases, "one@example.test").expect("email"),
            "one@example.test"
        );
    }

    #[test]
    fn d27_3_next_on_forget_skips_the_removed_account() {
        let rows = vec![
            row("a", true, true),
            row("b", false, false),
            row("c", true, false),
        ];
        assert_eq!(next_on_forget(&rows, "a").as_deref(), Some("c"));
    }

    #[test]
    fn d27_5_next_in_order_includes_off_accounts() {
        let rows = vec![row("a", true, true), row("b", false, false)];
        let quotas = vec![("a".to_owned(), quota(99.0)), ("b".to_owned(), quota(10.0))];
        let policy = LoginPolicy {
            routing: "smart".to_owned(),
            keep_login: false,
            switch_only: false,
            sign_in_spends: true,
        };
        let (to, _) = next_in_order(&rows, &quotas, &policy, true).expect("off allowed");
        assert_eq!(to, "b");
        assert!(next_in_order(&rows, &quotas, &policy, false).is_err());
    }

    #[test]
    fn d27_6_next_login_stays_on_moved_to_until_spent() {
        let rows = vec![row("first", true, false), row("second", true, true)];
        let quotas = vec![
            ("first".to_owned(), quota(50.0)),
            ("second".to_owned(), quota(40.0)),
        ];
        let policy = LoginPolicy {
            routing: "smart".to_owned(),
            keep_login: false,
            switch_only: false,
            sign_in_spends: true,
        };
        assert!(next_login(&rows, &quotas, &policy, Some("first")).is_none());
        let spent_first = vec![
            ("first".to_owned(), quota(50.0)),
            ("second".to_owned(), quota(99.0)),
        ];
        assert!(next_login(&rows, &spent_first, &policy, Some("first")).is_none());
    }

    #[test]
    fn d27_8_next_login_returns_under_order_routing() {
        let rows = vec![row("first", true, false), row("second", true, true)];
        let quotas = vec![
            ("first".to_owned(), quota(50.0)),
            ("second".to_owned(), quota(99.0)),
        ];
        let policy = LoginPolicy {
            routing: "order".to_owned(),
            keep_login: false,
            switch_only: false,
            sign_in_spends: true,
        };
        let jump = next_login(&rows, &quotas, &policy, Some("first")).expect("back");
        assert_eq!(jump.1, "first");
        assert!(jump.2);
    }
}
