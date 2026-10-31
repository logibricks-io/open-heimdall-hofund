//! Pure member-ordering strategies and their explicit per-router state.
//!
//! Smart, usage, pace, fast, and cheap are registered here so configuration
//! and callers can use their names now. P0-2b supplies their measurement-based
//! ordering functions; until then they preserve the listed order.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::SystemTime;

/// One route member as a strategy weighs it.
#[derive(Clone, Debug)]
pub struct Seat {
    /// Stable member id text across router snapshots.
    pub key: String,
    /// Whether this is a subscription account rather than an API key.
    pub account: bool,
    /// Whether its allowance is known.
    pub known: bool,
    /// Whether an unknown account learns its allowance from replies.
    pub learns: bool,
    /// Share of its allowance used, from 0 through 100.
    pub used: f64,
    /// Allowance renewal times, largest window first.
    pub soon: Vec<SystemTime>,
    /// When remaining credits lapse, apart from allowance renewals.
    pub lapse: Option<SystemTime>,
    /// Allowance left per hour until renewal.
    pub pace: f64,
    /// Relative request share, at least one in practice.
    pub weight: u64,
    /// Whether the member is cooling down.
    pub resting: bool,
    /// Recently routed tokens, decayed over time.
    pub tokens: f64,
    /// Whether this model costs nothing on this member.
    pub free: bool,
    /// Whether the account may spend credits past its allowance.
    pub credits: bool,
    /// Recent time to first token, in milliseconds; zero when unknown.
    pub ttft: f64,
    /// Recent output tokens per second; zero when unknown.
    pub tps: f64,
    /// Prepaid wallet balance.
    pub left: f64,
    /// Provider and unit identifying the wallet balance.
    pub left_in: String,
    /// Whether the current price is known.
    pub priced: bool,
    /// Current input plus output price per million tokens.
    pub cost: f64,
    /// Current cache-read price per million tokens.
    pub cache_cost: f64,
}

impl Seat {
    /// A seat with default measurement fields and weight one.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            account: false,
            known: false,
            learns: false,
            used: 0.0,
            soon: Vec::new(),
            lapse: None,
            pace: 0.0,
            weight: 1,
            resting: false,
            tokens: 0.0,
            free: false,
            credits: false,
            ttft: 0.0,
            tps: 0.0,
            left: 0.0,
            left_in: String::new(),
            priced: false,
            cost: 0.0,
            cache_cost: 0.0,
        }
    }
}

/// A member-ordering callback.
pub type Order = dyn Fn(&str, &[Seat], &StrategyState) -> Vec<usize> + Send + Sync;

/// A pure member-ordering function and the measurements it consumes.
#[derive(Clone)]
pub struct Strategy {
    /// Returns a permutation of seat indexes. Pool identifies the route/model.
    pub order: Arc<Order>,
    /// Whether the strategy's order is exact rather than a preference.
    pub exact: bool,
    /// Whether it weighs recent token use.
    pub by_use: bool,
    /// Whether it weighs free models.
    pub by_free: bool,
    /// Whether it weighs measured speed.
    pub by_speed: bool,
    /// Whether it weighs current prices.
    pub by_price: bool,
}

impl Strategy {
    /// A strategy with no measurement flags.
    #[must_use]
    pub fn new(
        order: impl Fn(&str, &[Seat], &StrategyState) -> Vec<usize> + Send + Sync + 'static,
    ) -> Self {
        Self {
            order: Arc::new(order),
            exact: false,
            by_use: false,
            by_free: false,
            by_speed: false,
            by_price: false,
        }
    }

    /// Marks the strategy's order as exact.
    #[must_use]
    pub const fn exact(mut self) -> Self {
        self.exact = true;
        self
    }

    /// Marks recent token use as consumed.
    #[must_use]
    pub const fn by_use(mut self) -> Self {
        self.by_use = true;
        self
    }

    /// Marks free-model metadata as consumed.
    #[must_use]
    pub const fn by_free(mut self) -> Self {
        self.by_free = true;
        self
    }

    /// Marks measured speed as consumed.
    #[must_use]
    pub const fn by_speed(mut self) -> Self {
        self.by_speed = true;
        self
    }

    /// Marks current prices as consumed.
    #[must_use]
    pub const fn by_price(mut self) -> Self {
        self.by_price = true;
        self
    }
}

impl fmt::Debug for Strategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Strategy")
            .field("exact", &self.exact)
            .field("by_use", &self.by_use)
            .field("by_free", &self.by_free)
            .field("by_speed", &self.by_speed)
            .field("by_price", &self.by_price)
            .finish_non_exhaustive()
    }
}

type Entries = BTreeMap<String, Vec<(u64, Strategy)>>;

/// Named strategies. The last registration serves until it is removed.
#[derive(Clone)]
pub struct Registry {
    entries: Arc<RwLock<Entries>>,
    next_id: Arc<AtomicU64>,
}

impl Default for Registry {
    fn default() -> Self {
        let registry = Self {
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        registry.insert("smart", Strategy::new(|_, seats, _| identity(seats.len())));
        registry
    }
}

impl Registry {
    /// A registry with every built-in routing name.
    #[must_use]
    pub fn with_builtins() -> Self {
        let registry = Self::default();
        for (name, strategy) in builtins() {
            registry.insert(name, strategy);
        }
        registry
    }

    /// Registers `strategy` for `name`. Dropping the returned registration
    /// removes only that entry and reveals any earlier registration.
    #[must_use]
    pub fn register(&self, name: impl Into<String>, strategy: Strategy) -> Registration {
        let name = name.into();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.clone())
            .or_default()
            .push((id, strategy));
        Registration {
            registry: self.clone(),
            name,
            id,
            active: true,
        }
    }

    /// The strategy serving `name`. Empty selects the default smart strategy.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<Strategy> {
        let name = if name.is_empty() { "smart" } else { name };
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .and_then(|entries| entries.last())
            .map(|(_, strategy)| strategy.clone())
    }

    /// Whether `name` has a serving strategy.
    #[must_use]
    pub fn served(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    fn insert(&self, name: &str, strategy: Strategy) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .push((id, strategy));
    }

    fn remove(&self, name: &str, id: u64) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(stack) = entries.get_mut(name) {
            stack.retain(|(entry, _)| *entry != id);
            if stack.is_empty() {
                entries.remove(name);
            }
        }
    }
}

/// One removable registry entry.
pub struct Registration {
    registry: Registry,
    name: String,
    id: u64,
    active: bool,
}

impl Registration {
    /// Removes this entry. Repeated removal has no effect.
    pub fn remove(&mut self) {
        self.remove_inner();
    }

    fn remove_inner(&mut self) {
        if self.active {
            self.registry.remove(&self.name, self.id);
            self.active = false;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.remove_inner();
    }
}

#[derive(Default)]
struct Pools {
    turns: HashMap<String, u64>,
    rounds: HashMap<String, Round>,
}

#[derive(Default)]
struct Round {
    current: HashMap<String, i128>,
    total: u128,
    last: Option<String>,
}

/// Strategy registry plus mutable rounds shared by related router snapshots.
#[derive(Clone)]
pub struct StrategyState {
    registry: Registry,
    pools: Arc<Mutex<Pools>>,
}

impl StrategyState {
    /// State using `registry` and empty per-pool rounds.
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            pools: Arc::new(Mutex::new(Pools::default())),
        }
    }

    /// State with all built-in strategies registered.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::new(Registry::with_builtins())
    }

    /// The named strategy registry.
    #[must_use]
    pub const fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Applies a named strategy, or listed order for an unknown name.
    #[must_use]
    pub fn order(&self, name: &str, pool: &str, seats: &[Seat]) -> Vec<usize> {
        let Some(strategy) = self.registry.find(name) else {
            tracing::warn!(
                pool,
                routing = name,
                "unknown routing strategy; using listed order"
            );
            return identity(seats.len());
        };
        apply(&strategy, pool, seats, self)
    }

    /// Drops the turn and weighted round for one pool.
    pub fn forget(&self, pool: &str) {
        let mut pools = self.lock();
        pools.turns.remove(pool);
        pools.rounds.remove(pool);
    }

    fn next_turn(&self, pool: &str, members: usize) -> usize {
        if members == 0 {
            return 0;
        }
        let mut pools = self.lock();
        let turn = pools.turns.entry(pool.to_owned()).or_default();
        let start = (*turn % members as u64) as usize;
        *turn = turn.wrapping_add(1);
        start
    }

    fn round(&self, pool: &str, seats: &[Seat]) -> Option<usize> {
        let mut pools = self.lock();
        let round = pools.rounds.entry(pool.to_owned()).or_default();
        let present = seats
            .iter()
            .map(|seat| seat.key.clone())
            .collect::<std::collections::HashSet<_>>();
        round.current.retain(|key, _| present.contains(key));

        let mut best = None;
        let mut best_current = i128::MIN;
        let mut total = 0_u128;
        for (index, seat) in seats.iter().enumerate() {
            if seat.resting {
                continue;
            }
            let weight = seat.weight.max(1);
            let current = {
                let current = round.current.entry(seat.key.clone()).or_default();
                *current += i128::from(weight);
                *current
            };
            total += u128::from(weight);
            if best.is_none() || current > best_current {
                best = Some(index);
                best_current = current;
            }
        }
        let best = best?;
        let charge = i128::try_from(total).unwrap_or(i128::MAX);
        *round
            .current
            .get_mut(&seats[best].key)
            .expect("selected seat has a current weight") -= charge;
        round.total = total;
        round.last = Some(seats[best].key.clone());
        Some(best)
    }

    fn lock(&self) -> MutexGuard<'_, Pools> {
        self.pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Applies `strategy`, failing open to listed order after a panic or an
/// invalid permutation so every member remains eligible for failover.
#[must_use]
pub fn apply(strategy: &Strategy, pool: &str, seats: &[Seat], state: &StrategyState) -> Vec<usize> {
    let result = catch_unwind(AssertUnwindSafe(|| (strategy.order)(pool, seats, state)));
    let order = match result {
        Ok(order) => order,
        Err(payload) => {
            tracing::warn!(pool, panic = ?panic_text(payload.as_ref()), "routing strategy panicked; using listed order");
            return identity(seats.len());
        }
    };
    let mut seen = vec![false; seats.len()];
    let valid = order.len() == seats.len()
        && order
            .iter()
            .all(|index| *index < seats.len() && !std::mem::replace(&mut seen[*index], true));
    if valid {
        order
    } else {
        tracing::warn!(
            pool,
            ?order,
            members = seats.len(),
            "routing strategy returned an invalid permutation; using listed order"
        );
        identity(seats.len())
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text
    } else {
        "non-string panic payload"
    }
}

fn builtins() -> [(&'static str, Strategy); 9] {
    [
        ("smart", Strategy::new(|_, seats, _| identity(seats.len()))),
        (
            "order",
            Strategy::new(|_, seats, _| identity(seats.len())).exact(),
        ),
        ("rotate", Strategy::new(rotate)),
        (
            "usage",
            Strategy::new(|_, seats, _| identity(seats.len())).by_use(),
        ),
        (
            "pace",
            Strategy::new(|_, seats, _| identity(seats.len())).by_use(),
        ),
        ("weight", Strategy::new(weighted)),
        ("free", Strategy::new(free_first).by_free()),
        (
            "fast",
            Strategy::new(|_, seats, _| identity(seats.len())).by_speed(),
        ),
        (
            "cheap",
            Strategy::new(|_, seats, _| identity(seats.len())).by_price(),
        ),
    ]
}

fn identity(members: usize) -> Vec<usize> {
    (0..members).collect()
}

fn rotate(pool: &str, seats: &[Seat], state: &StrategyState) -> Vec<usize> {
    if seats.len() < 2 {
        return identity(seats.len());
    }
    let start = least_held(pool, seats, state);
    (0..seats.len())
        .map(|offset| (start + offset) % seats.len())
        .collect()
}

/// The rotation seam for affinity's least-held choice. Until conversation
/// holdings exist, it is the ordinary per-pool turn counter.
fn least_held(pool: &str, seats: &[Seat], state: &StrategyState) -> usize {
    state.next_turn(pool, seats.len())
}

fn weighted(pool: &str, seats: &[Seat], state: &StrategyState) -> Vec<usize> {
    first(state.round(pool, seats), seats.len())
}

fn first(best: Option<usize>, members: usize) -> Vec<usize> {
    let mut order = identity(members);
    if let Some(best) = best.filter(|best| *best > 0 && *best < members) {
        let selected = order.remove(best);
        order.insert(0, selected);
    }
    order
}

fn free_first(_: &str, seats: &[Seat], _: &StrategyState) -> Vec<usize> {
    seats
        .iter()
        .enumerate()
        .filter(|(_, seat)| seat.free)
        .map(|(index, _)| index)
        .chain(
            seats
                .iter()
                .enumerate()
                .filter(|(_, seat)| !seat.free)
                .map(|(index, _)| index),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Registry, Seat, Strategy, StrategyState, apply};

    fn seat(id: &'static str) -> Seat {
        Seat::new(id)
    }

    fn seats(ids: &[&'static str]) -> Vec<Seat> {
        ids.iter().map(|id| seat(id)).collect()
    }

    fn keys(seats: &[Seat], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|index| seats[*index].key.to_string())
            .collect()
    }

    #[test]
    fn a1_strategy_registry_and_apply_fail_open() {
        let registry = Registry::default();
        assert!(registry.find("").is_some());
        assert!(!registry.served("custom"));
        let first = registry.register("custom", Strategy::new(|_, _, _| vec![1, 0]).exact());
        let mut second = registry.register("custom", Strategy::new(|_, _, _| vec![0, 1]));
        assert!(!registry.find("custom").expect("second").exact);
        second.remove();
        second.remove();
        assert!(registry.find("custom").expect("first").exact);
        drop(first);
        assert!(!registry.served("custom"));

        let state = StrategyState::with_builtins();
        assert!(state.registry().find("").is_some());
        let pool_seats = seats(&["a", "b", "c"]);
        for strategy in [
            Strategy::new(|_, _, _| panic!("boom")),
            Strategy::new(|_, _, _| vec![2, 1]),
            Strategy::new(|_, _, _| vec![2, 2, 0]),
            Strategy::new(|_, _, _| vec![3, 1, 0]),
            Strategy::new(|_, _, _| Vec::new()),
        ] {
            assert_eq!(apply(&strategy, "pool", &pool_seats, &state), [0, 1, 2]);
        }
        let valid = Strategy::new(|_, _, _| vec![2, 0, 1]);
        assert_eq!(apply(&valid, "pool", &pool_seats, &state), [2, 0, 1]);
    }

    #[test]
    fn a4_order_is_identity_and_exact() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("order").expect("order");
        let seats = seats(&["a", "b", "c"]);
        assert!(strategy.exact);
        assert_eq!(apply(&strategy, "pool", &seats, &state), [0, 1, 2]);
    }

    #[test]
    fn a5_rotate_is_per_pool_and_least_held_hook_is_stubbed() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("rotate").expect("rotate");
        let seats = seats(&["a", "b", "c"]);
        let turns = (0..4)
            .map(|_| keys(&seats, &apply(&strategy, "first", &seats, &state)))
            .collect::<Vec<_>>();
        assert_eq!(
            turns,
            [
                keys(&seats, &[0, 1, 2]),
                keys(&seats, &[1, 2, 0]),
                keys(&seats, &[2, 0, 1]),
                keys(&seats, &[0, 1, 2]),
            ]
        );
        assert_eq!(
            keys(&seats, &apply(&strategy, "second", &seats, &state))[0],
            seats[0].key.to_string()
        );
        state.forget("first");
        assert_eq!(
            keys(&seats, &apply(&strategy, "first", &seats, &state))[0],
            seats[0].key.to_string()
        );
    }

    #[test]
    fn a6_weighted_round_robin_is_smooth_and_skips_resting_seat() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("weight").expect("weight");
        let mut seats = seats(&["a", "b", "c"]);
        seats[0].weight = 3;
        seats[1].weight = 1;
        seats[2].weight = 4;

        let mut counts = [0_usize; 3];
        let mut sequence = Vec::new();
        for turn in 0..400 {
            let order = apply(&strategy, "weighted", &seats, &state);
            counts[order[0]] += 1;
            sequence.push(order[0]);
            let tail = (0..3)
                .filter(|index| *index != order[0])
                .collect::<Vec<_>>();
            assert_eq!(order[1..], tail);
            if (turn + 1) % 8 == 0 {
                assert_eq!(
                    counts,
                    [3 * (turn + 1) / 8, (turn + 1) / 8, 4 * (turn + 1) / 8]
                );
            }
        }
        assert_eq!(counts, [150, 50, 200]);
        assert!(!sequence.windows(3).any(|window| window == [2, 2, 2]));

        state.forget("weighted");
        seats[2].resting = true;
        let mut counts = [0_usize; 3];
        for _ in 0..100 {
            let order = apply(&strategy, "weighted", &seats, &state);
            counts[order[0]] += 1;
            assert_eq!(order[2], 2);
        }
        assert_eq!(counts, [75, 25, 0]);

        for seat in &mut seats {
            seat.resting = true;
        }
        assert_eq!(apply(&strategy, "weighted", &seats, &state), [0, 1, 2]);
    }

    #[test]
    fn a7_free_first_uses_user_catalog_and_zero_price_flags() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("free").expect("free");
        assert!(strategy.by_free);
        for other in [
            "smart", "order", "rotate", "usage", "pace", "weight", "fast", "cheap",
        ] {
            assert!(!state.registry().find(other).expect(other).by_free);
        }

        let mut seats = seats(&["a", "b", "c", "d"]);
        seats[1].free = true;
        seats[3].free = true;
        assert_eq!(apply(&strategy, "pool", &seats, &state), [1, 3, 0, 2]);
        for seat in &mut seats {
            seat.free = false;
        }
        assert_eq!(apply(&strategy, "pool", &seats, &state), [0, 1, 2, 3]);
    }
}
