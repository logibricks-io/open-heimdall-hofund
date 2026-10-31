//! Per-caller model and account restrictions.

use crate::{Hop, Member};

/// Model and account patterns carried by an authenticated caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteScope {
    /// Effective model patterns. Empty allows every model.
    pub models: Vec<String>,
    /// Account ids or provider-key fingerprints. Empty allows every account.
    pub accounts: Vec<String>,
}

impl RouteScope {
    /// Whether a hop may serve a request for `asked` on `route`.
    #[must_use]
    pub fn allows_hop(&self, asked: &str, route: &str, hop: &Hop) -> bool {
        self.allows_model(asked, route, hop) && self.allows_member(hop.member.as_ref())
    }

    /// Whether the hop's provider and wire or route model is in scope.
    #[must_use]
    pub fn allows_model(&self, asked: &str, route: &str, hop: &Hop) -> bool {
        if self.models.is_empty() {
            return true;
        }
        let provider = hop.member.scope_provider();
        let mut candidates = vec![
            format!("{provider}/{route}"),
            format!("{provider}/{}", hop.wire_model.as_deref().unwrap_or(asked)),
        ];
        if asked == route && route.starts_with("group/") {
            candidates.push(route.to_owned());
        }
        if asked.contains('/') {
            candidates.push(asked.to_owned());
        }
        candidates
            .iter()
            .any(|candidate| model_allowed(&self.models, candidate))
    }

    /// Whether any stable account id or provider-key fingerprint is allowed.
    #[must_use]
    pub fn allows_member(&self, member: &dyn Member) -> bool {
        self.accounts.is_empty()
            || member
                .account_refs()
                .iter()
                .any(|reference| allows_account(&self.accounts, member.scope_provider(), reference))
    }
}

/// Case-insensitive model matching; `*` matches any byte run, including `/`.
#[must_use]
pub fn model_allowed(patterns: &[String], id: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            pattern.eq_ignore_ascii_case(id) || (pattern.contains('*') && glob_matches(pattern, id))
        })
}

fn glob_matches(pattern: &str, id: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let id = id.to_lowercase();
    let (pattern, id) = (pattern.as_bytes(), id.as_bytes());
    let mut previous = vec![false; id.len() + 1];
    previous[0] = true;
    for &token in pattern {
        let mut current = vec![false; id.len() + 1];
        if token == b'*' {
            current[0] = previous[0];
            for index in 1..=id.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=id.len() {
                current[index] = previous[index - 1] && token == id[index - 1];
            }
        }
        previous = current;
    }
    previous[id.len()]
}

fn allows_account(accounts: &[String], provider: &str, reference: &str) -> bool {
    let provider = provider.trim().to_lowercase();
    let wanted = format!("{provider}/{}", reference.trim().to_lowercase());
    let mut named = false;
    for account in accounts {
        let account = account.trim().to_lowercase();
        if account == wanted {
            return true;
        }
        if account.split_once('/').is_some_and(|(p, _)| p == provider) {
            named = true;
        }
    }
    !named
}
