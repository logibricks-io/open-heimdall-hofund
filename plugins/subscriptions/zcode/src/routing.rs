use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::AccountId;
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::oauth::secret_text;

use crate::session::Stored;

const ROUTE_TTL: Duration = Duration::from_secs(10 * 60);
const UNCERTAIN_ROUTE_TTL: Duration = Duration::from_secs(60);
const MODEL_REST: Duration = Duration::from_secs(60);
const EDGE_HOLD: Duration = Duration::from_secs(3 * 60);

#[derive(Default)]
pub(crate) struct Routing {
    routes: Mutex<HashMap<AccountId, CachedRoute>>,
    blocked: Mutex<HashMap<(AccountId, String), Duration>>,
    sidelined: Mutex<HashMap<(AccountId, String), Duration>>,
    edge: Mutex<Option<EdgeHold>>,
}

#[derive(Clone, Copy)]
pub(crate) struct EdgeHold {
    pub(crate) monotonic_until: Duration,
    pub(crate) wall_until: SystemTime,
}

#[derive(Clone)]
pub(crate) struct GiftRoute {
    usable_models: Vec<String>,
}

impl GiftRoute {
    pub(crate) fn new(usable_models: Vec<String>) -> Self {
        Self { usable_models }
    }

    pub(crate) fn serves(&self, model: &str) -> bool {
        self.usable_models
            .iter()
            .any(|served| served.eq_ignore_ascii_case(model))
    }
}

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) start: bool,
    pub(crate) gift: Option<GiftRoute>,
}

struct CachedRoute {
    route: Route,
    credential: [u8; 32],
    read_at: Duration,
    ttl: Duration,
}

impl Routing {
    pub(crate) fn cached(
        &self,
        account: &AccountId,
        stored: &Stored,
        now: Duration,
    ) -> Option<Route> {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account)
            .filter(|cached| {
                cached.credential == credential_fingerprint(stored)
                    && now < cached.read_at.saturating_add(cached.ttl)
            })
            .map(|cached| cached.route.clone())
    }

    pub(crate) fn remember(
        &self,
        account: &AccountId,
        stored: &Stored,
        now: Duration,
        route: Route,
        certain: bool,
    ) {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                account.clone(),
                CachedRoute {
                    route,
                    credential: credential_fingerprint(stored),
                    read_at: now,
                    ttl: if certain {
                        ROUTE_TTL
                    } else {
                        UNCERTAIN_ROUTE_TTL
                    },
                },
            );
    }

    pub(crate) fn previously_had_coding_plan(&self, account: &AccountId) -> bool {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account)
            .is_some_and(|cached| !cached.route.start)
    }

    pub(crate) fn invalidate(&self, account: &AccountId) {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(account);
    }

    pub(crate) fn gift_rested(&self, account: &AccountId, model: &str, now: Duration) -> bool {
        rested(&self.blocked, account, model, now)
    }

    pub(crate) fn free_first_sidelined(
        &self,
        account: &AccountId,
        model: &str,
        now: Duration,
    ) -> bool {
        rested(&self.sidelined, account, model, now)
    }

    pub(crate) fn rest_gift(&self, account: &AccountId, model: &str, now: Duration, spent: bool) {
        let rests = if spent {
            &self.blocked
        } else {
            &self.sidelined
        };
        rests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(rest_key(account, model), now.saturating_add(MODEL_REST));
        self.invalidate(account);
    }

    pub(crate) fn hold_edge(&self, now: Duration, wall_now: SystemTime) {
        let hold = EdgeHold {
            monotonic_until: now.saturating_add(EDGE_HOLD),
            wall_until: wall_now + EDGE_HOLD,
        };
        let mut edge = self
            .edge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if edge.is_none_or(|current| current.monotonic_until < hold.monotonic_until) {
            *edge = Some(hold);
        }
    }

    pub(crate) fn edge_held(&self, now: Duration) -> Option<EdgeHold> {
        let mut edge = self
            .edge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if edge.is_some_and(|held| now < held.monotonic_until) {
            return *edge;
        }
        *edge = None;
        None
    }
}

fn rested(
    rests: &Mutex<HashMap<(AccountId, String), Duration>>,
    account: &AccountId,
    model: &str,
    now: Duration,
) -> bool {
    let key = rest_key(account, model);
    let mut rests = rests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rests.get(&key).is_some_and(|until| now < *until) {
        return true;
    }
    rests.remove(&key);
    false
}

fn rest_key(account: &AccountId, model: &str) -> (AccountId, String) {
    (account.clone(), model.to_ascii_lowercase())
}

fn credential_fingerprint(stored: &Stored) -> [u8; 32] {
    let mut material = secret_text(&stored.api_key).as_bytes().to_vec();
    material.push(0);
    if let Some(jwt) = &stored.auth.jwt {
        material.extend_from_slice(secret_text(jwt).as_bytes());
    }
    sha256(&material)
}
