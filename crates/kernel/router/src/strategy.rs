//! Pure member-ordering strategies and their explicit per-router state.

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
    /// Allowance renewal times, largest window first; `None` is not known.
    pub soon: Vec<Option<SystemTime>>,
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

/// Live measurements supplied by the process that owns quota and usage state.
pub trait Measurements: Send + Sync {
    /// Fills measurement fields on `seats`. `wire_models` is the model each
    /// member would be asked for; `usage_model` is the client-facing route id.
    /// Allowances are always filled; recent use, speed, and prices only when
    /// `needs` asks for them.
    fn weigh(
        &self,
        seats: &mut [Seat],
        wire_models: &[String],
        usage_model: &str,
        needs: Needs,
        now: SystemTime,
    );
}

/// Which costly measurements a routing strategy reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// Recent token use per member.
    pub usage: bool,
    /// Measured first-token latency and throughput.
    pub speed: bool,
    /// Current catalog prices.
    pub price: bool,
    /// Current free-model flags and prices.
    pub free: bool,
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
    probes: HashMap<String, Probe>,
}

#[derive(Default)]
struct Probe {
    requests: u64,
    turns: u64,
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

    /// The measurements the strategy registered as `name` reads; none for an
    /// unknown name.
    #[must_use]
    pub fn needs(&self, name: &str) -> Needs {
        self.registry
            .find(name)
            .map_or_else(Needs::default, |strategy| Needs {
                usage: strategy.by_use,
                speed: strategy.by_speed,
                price: strategy.by_price,
                free: strategy.by_free,
            })
    }

    /// Orders `seats` by the strategy registered as `name`.
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

    /// Drops the turn, weighted round, and speed probe for one pool.
    pub fn forget(&self, pool: &str) {
        let mut pools = self.lock();
        pools.turns.remove(pool);
        pools.rounds.remove(pool);
        pools.probes.remove(pool);
    }

    fn speed_probe(&self, pool: &str, unmeasured: &[usize]) -> Option<usize> {
        let mut pools = self.lock();
        let probe = pools.probes.entry(pool.to_owned()).or_default();
        probe.requests = probe.requests.wrapping_add(1);
        if probe.requests < SPEED_PROBE_EVERY {
            return None;
        }
        probe.requests = 0;
        let selected = unmeasured[(probe.turns % unmeasured.len() as u64) as usize];
        probe.turns = probe.turns.wrapping_add(1);
        Some(selected)
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

    /// Charges the latest pick to the member affinity actually kept.
    pub fn move_round(&self, pool: &str, picked: &str, kept: &str) {
        if picked == kept {
            return;
        }
        let mut pools = self.lock();
        let Some(round) = pools.rounds.get_mut(pool) else {
            return;
        };
        if round.last.as_deref() != Some(picked) || !round.current.contains_key(kept) {
            return;
        }
        let charge = i128::try_from(round.total).unwrap_or(i128::MAX);
        if let Some(current) = round.current.get_mut(picked) {
            *current += charge;
        }
        if let Some(current) = round.current.get_mut(kept) {
            *current -= charge;
        }
        round.last = Some(kept.to_owned());
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
        ("smart", Strategy::new(smart)),
        (
            "order",
            Strategy::new(|_, seats, _| identity(seats.len())).exact(),
        ),
        ("rotate", Strategy::new(rotate)),
        ("usage", Strategy::new(least_used).by_use()),
        ("pace", Strategy::new(weekly_pace).by_use()),
        ("weight", Strategy::new(weighted)),
        ("free", Strategy::new(free_first).by_free()),
        ("fast", Strategy::new(fastest).by_speed()),
        ("cheap", Strategy::new(cheapest).by_price()),
    ]
}

const LOW_SHARE: f64 = 90.0;
const USED_SHARE: f64 = 98.0;
const SPEED_REFERENCE_TOKENS: f64 = 200.0;
const SPEED_PROBE_EVERY: u64 = 5;

fn tier(share: f64) -> u8 {
    if share >= USED_SHARE {
        2
    } else if share >= LOW_SHARE {
        1
    } else {
        0
    }
}

fn identity(members: usize) -> Vec<usize> {
    (0..members).collect()
}

fn smart(_: &str, seats: &[Seat], _: &StrategyState) -> Vec<usize> {
    let left = by_left(seats, false);
    let mut fine = Vec::new();
    let mut low = Vec::new();
    let mut spent = Vec::new();
    for (index, seat) in seats.iter().enumerate() {
        match tier(seat.used) {
            2 => spent.push(index),
            1 => low.push(index),
            _ => fine.push(index),
        }
    }
    fine.sort_by(|left_index, right_index| {
        let a = &seats[*left_index];
        let b = &seats[*right_index];
        b.learns
            .cmp(&a.learns)
            .then_with(|| compare_renewals(&a.soon, &b.soon))
            .then_with(|| compare_deadlines(a.lapse, b.lapse))
            .then_with(|| left[*left_index].cmp(&left[*right_index]))
    });
    low.sort_by(|a, b| seats[*a].used.total_cmp(&seats[*b].used));
    spent.sort_by(|a, b| spent_before(&seats[*a], &seats[*b]));
    fine.into_iter().chain(low).chain(spent).collect::<Vec<_>>()
}

fn compare_renewals(
    left: &[Option<SystemTime>],
    right: &[Option<SystemTime>],
) -> std::cmp::Ordering {
    let length = left.len().max(right.len());
    for index in 0..length {
        let ordering = compare_deadlines(
            left.get(index).copied().flatten(),
            right.get(index).copied().flatten(),
        );
        if ordering.is_ne() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

fn compare_deadlines(left: Option<SystemTime>, right: Option<SystemTime>) -> std::cmp::Ordering {
    match (left.map(hour), right.map(hour)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn hour(at: SystemTime) -> u64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 3600)
}

fn spent_before(a: &Seat, b: &Seat) -> std::cmp::Ordering {
    b.credits
        .cmp(&a.credits)
        .then_with(|| a.used.total_cmp(&b.used))
}

fn by_left(seats: &[Seat], tokens_first: bool) -> Vec<usize> {
    let mut told = seats
        .iter()
        .enumerate()
        .filter(|(_, seat)| !seat.left_in.is_empty())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if told.is_empty() {
        return vec![0; seats.len()];
    }
    if tokens_first {
        told.sort_by(|a, b| seats[*a].tokens.total_cmp(&seats[*b].tokens));
    }
    let mut places = vec![seats.len(); seats.len()];
    let mut kinds = BTreeMap::<&str, Vec<usize>>::new();
    for (place, index) in told.iter().enumerate() {
        kinds.entry(&seats[*index].left_in).or_default().push(place);
    }
    for positions in kinds.values() {
        let mut indexes = positions
            .iter()
            .map(|place| told[*place])
            .collect::<Vec<_>>();
        indexes.sort_by(|a, b| seats[*b].left.total_cmp(&seats[*a].left));
        for (place, index) in positions.iter().zip(indexes) {
            places[index] = *place;
        }
    }
    places
}

fn least_used(_: &str, seats: &[Seat], _: &StrategyState) -> Vec<usize> {
    let left = by_left(seats, true);
    let mut order = identity(seats.len());
    order.sort_by(|a, b| {
        let (a_index, b_index) = (*a, *b);
        let a = &seats[a_index];
        let b = &seats[b_index];
        let credits = if tier(a.used) == 2 && tier(b.used) == 2 {
            b.credits.cmp(&a.credits)
        } else {
            std::cmp::Ordering::Equal
        };
        credits
            .then_with(|| a.used.total_cmp(&b.used))
            .then_with(|| left[a_index].cmp(&left[b_index]))
            .then_with(|| a.tokens.total_cmp(&b.tokens))
    });
    order
}

fn weekly_pace(_: &str, seats: &[Seat], _: &StrategyState) -> Vec<usize> {
    let mut by_pace = seats
        .iter()
        .enumerate()
        .filter(|(_, seat)| tier(seat.used) == 0 && !seat.learns)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    by_pace.sort_by(|a, b| seats[*b].pace.total_cmp(&seats[*a].pace));
    let mut bands = vec![0_usize; seats.len()];
    let mut top = 0.0;
    for (position, index) in by_pace.iter().enumerate() {
        let pace = seats[*index].pace;
        if position == 0 || pace < 0.9 * top {
            top = pace;
            if position > 0 {
                bands[*index] = bands[by_pace[position - 1]] + 1;
                continue;
            }
        }
        bands[*index] = bands[by_pace[position.saturating_sub(1)]];
    }
    let left = by_left(seats, true);
    let mut order = identity(seats.len());
    order.sort_by(|a, b| {
        let (a_index, b_index) = (*a, *b);
        let a = &seats[a_index];
        let b = &seats[b_index];
        let a_tier = tier(a.used);
        let b_tier = tier(b.used);
        a_tier
            .cmp(&b_tier)
            .then_with(|| {
                if a_tier == 2 && b_tier == 2 {
                    b.credits.cmp(&a.credits)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| {
                if a_tier > 0 && a_tier == b_tier {
                    a.used.total_cmp(&b.used)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| {
                if a_tier == 0 && b_tier == 0 {
                    b.learns.cmp(&a.learns)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| {
                if a_tier == 0 && b_tier == 0 {
                    bands[a_index].cmp(&bands[b_index])
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| {
                if a_tier == 0 && b_tier == 0 {
                    left[a_index].cmp(&left[b_index])
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.tokens.total_cmp(&b.tokens))
    });
    order
}

fn fastest(pool: &str, seats: &[Seat], state: &StrategyState) -> Vec<usize> {
    let mut fast = Vec::new();
    let mut unmeasured = Vec::new();
    let mut resting = Vec::new();
    let mut spent = Vec::new();
    for (index, seat) in seats.iter().enumerate() {
        if seat.resting {
            resting.push(index);
        } else if tier(seat.used) == 2 {
            spent.push(index);
        } else if seat.ttft > 0.0 {
            fast.push(index);
        } else {
            unmeasured.push(index);
        }
    }
    let mut paces = fast
        .iter()
        .filter_map(|index| (seats[*index].tps > 0.0).then_some(seats[*index].tps))
        .collect::<Vec<_>>();
    paces.sort_by(f64::total_cmp);
    let middle = paces.get(paces.len() / 2).copied().unwrap_or(0.0);
    let estimates = fast
        .iter()
        .map(|index| (*index, estimate_speed(&seats[*index], middle)))
        .collect::<HashMap<_, _>>();
    fast.sort_by(|a, b| estimates[a].total_cmp(&estimates[b]));
    let mut bands = HashMap::new();
    let mut top = 0.0;
    for (position, index) in fast.iter().enumerate() {
        let band = if position == 0 {
            top = estimates[index];
            0
        } else {
            let previous = fast[position - 1];
            let mut band = bands[&previous];
            if estimates[index] > 1.1 * top {
                band += 1;
                top = estimates[index];
            }
            band
        };
        bands.insert(*index, band);
    }
    fast.sort_by(|a, b| bands[a].cmp(&bands[b]).then_with(|| a.cmp(b)));
    let mut awake = fast;
    let measured = !awake.is_empty();
    awake.extend(unmeasured.iter().copied());
    if measured
        && !unmeasured.is_empty()
        && let Some(probe) = state.speed_probe(pool, &unmeasured)
    {
        let selected = awake.remove(
            awake
                .iter()
                .position(|index| *index == probe)
                .expect("unmeasured seat is awake"),
        );
        awake.insert(0, selected);
    }
    awake
        .into_iter()
        .chain(resting)
        .chain(spent)
        .collect::<Vec<_>>()
}

fn estimate_speed(seat: &Seat, middle: f64) -> f64 {
    let tps = if seat.tps > 0.0 { seat.tps } else { middle };
    if tps > 0.0 {
        seat.ttft + SPEED_REFERENCE_TOKENS * 1000.0 / tps
    } else {
        seat.ttft
    }
}

fn cheapest(_: &str, seats: &[Seat], _: &StrategyState) -> Vec<usize> {
    let mut order = identity(seats.len());
    stable_by_less(&mut order, |a, b| {
        let a = &seats[a];
        let b = &seats[b];
        if a.priced != b.priced {
            return a.priced;
        }
        if !a.priced {
            return false;
        }
        if !same_cost(a.cost, b.cost) {
            return a.cost < b.cost;
        }
        !same_cost(a.cache_cost, b.cache_cost) && a.cache_cost < b.cache_cost
    });
    order
}

/// A stable insertion sort by a less-than that need not be a total order.
/// `same_cost` is not transitive, so `sort_by` may panic on it; insertion
/// sort is what the reference's stable sort does for pools of up to twenty.
fn stable_by_less(order: &mut [usize], less: impl Fn(usize, usize) -> bool) {
    for index in 1..order.len() {
        let mut at = index;
        while at > 0 && less(order[at], order[at - 1]) {
            order.swap(at, at - 1);
            at -= 1;
        }
    }
}

fn same_cost(a: f64, b: f64) -> bool {
    let difference = (a - b).abs();
    difference <= 1e-6 || difference <= 1e-6 * a.max(b)
}

fn rotate(pool: &str, seats: &[Seat], state: &StrategyState) -> Vec<usize> {
    // The turn advances for a single member too, as in the reference, so a
    // pool that grows keeps counting from the requests it already routed.
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
        let pool_seats = self::seats(&["a", "b", "c"]);
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
        let seats = self::seats(&["a", "b", "c"]);
        assert!(strategy.exact);
        assert_eq!(apply(&strategy, "pool", &seats, &state), [0, 1, 2]);
    }

    #[test]
    fn a5_rotate_is_per_pool_and_least_held_hook_is_stubbed() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("rotate").expect("rotate");
        let seats = self::seats(&["a", "b", "c"]);
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
        let mut seats = self::seats(&["a", "b", "c"]);
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

        let mut seats = self::seats(&["a", "b", "c", "d"]);
        seats[1].free = true;
        seats[3].free = true;
        assert_eq!(apply(&strategy, "pool", &seats, &state), [1, 3, 0, 2]);
        for seat in &mut seats {
            seat.free = false;
        }
        assert_eq!(apply(&strategy, "pool", &seats, &state), [0, 1, 2, 3]);
    }

    fn picks(state: &StrategyState, pool: &str, seats: &[Seat], turns: usize) -> Vec<String> {
        let strategy = state.registry().find("weight").expect("weight");
        (0..turns)
            .map(|_| keys(seats, &apply(&strategy, pool, seats, state))[0].clone())
            .collect()
    }

    #[test]
    fn a6_equal_weights_tie_goes_to_the_listed_first() {
        let state = StrategyState::with_builtins();
        let seats = self::seats(&["a", "b"]);
        assert_eq!(picks(&state, "pool", &seats, 4), ["a", "b", "a", "b"]);
    }

    #[test]
    fn a6_rounds_are_kept_per_pool_and_forget_restarts_one() {
        let state = StrategyState::with_builtins();
        let seats = self::seats(&["a", "b"]);
        assert_eq!(picks(&state, "first", &seats, 1), ["a"]);
        assert_eq!(picks(&state, "second", &seats, 1), ["a"]);
        state.forget("first");
        assert_eq!(picks(&state, "first", &seats, 1), ["a"]);
        assert_eq!(picks(&state, "second", &seats, 1), ["b"]);
    }

    #[test]
    fn a6_a_member_no_longer_listed_leaves_the_round() {
        let state = StrategyState::with_builtins();
        let mut both = self::seats(&["a", "b"]);
        both[1].weight = 3;
        assert_eq!(picks(&state, "pool", &both, 1), ["b"]);
        // b leaves the pool: its negative current weight is dropped, so
        // it starts again from zero when it comes back.
        assert_eq!(picks(&state, "pool", &both[..1], 1), ["a"]);
        assert_eq!(picks(&state, "pool", &both, 1), ["b"]);
    }

    #[test]
    fn a5_rotate_counts_turns_while_the_pool_has_one_member() {
        let state = StrategyState::with_builtins();
        let strategy = state.registry().find("rotate").expect("rotate");
        let one = self::seats(&["a"]);
        assert_eq!(apply(&strategy, "pool", &one, &state), [0]);
        // One request already went to the pool, so the next starts one along.
        let two = self::seats(&["a", "b"]);
        assert_eq!(apply(&strategy, "pool", &two, &state), [1, 0]);
        assert!(apply(&strategy, "empty", &[], &state).is_empty());
    }

    #[test]
    fn a1_unknown_name_orders_as_listed() {
        let state = StrategyState::with_builtins();
        let seats = self::seats(&["a", "b", "c"]);
        assert_eq!(state.order("no-such-routing", "pool", &seats), [0, 1, 2]);
        assert_eq!(state.order("", "pool", &seats), [0, 1, 2]);
    }

    fn routed(state: &StrategyState, name: &str, seats: &[Seat]) -> Vec<String> {
        keys(seats, &state.order(name, "pool", seats))
    }

    #[test]
    fn b1a_smart_uses_allowance_tiers_renewals_and_learning() {
        use std::time::{Duration, SystemTime};

        let state = StrategyState::with_builtins();
        let now = SystemTime::now();
        let mut seats = self::seats(&["spent", "later", "low", "unknown", "sooner", "learns"]);
        seats[0].used = 99.0;
        seats[1].used = 10.0;
        seats[1].soon = vec![Some(now + Duration::from_secs(48 * 3600))];
        seats[2].used = 92.0;
        seats[4].used = 50.0;
        seats[4].soon = vec![Some(now + Duration::from_secs(2 * 3600))];
        seats[5].learns = true;
        assert_eq!(
            routed(&state, "smart", &seats),
            ["learns", "sooner", "later", "unknown", "low", "spent"]
        );
    }

    #[test]
    fn b1b_smart_orders_wallets_only_within_provider_and_unit() {
        let state = StrategyState::with_builtins();
        let wallet = |key, left, left_in: &str| {
            let mut seat = seat(key);
            seat.account = true;
            seat.known = true;
            seat.pace = 100.0 / 168.0;
            seat.left = left;
            seat.left_in = left_in.to_owned();
            seat
        };
        for name in ["smart", "pace", "usage"] {
            let seats = vec![
                wallet("small", 750.0, "first\0points"),
                wallet("none", 0.0, ""),
                wallet("big", 6291.0, "first\0points"),
            ];
            assert_eq!(routed(&state, name, &seats), ["big", "small", "none"]);
            let seats = vec![
                wallet("small", 750.0, "first\0points"),
                wallet("big", 6291.0, "second\0points"),
            ];
            assert_eq!(routed(&state, name, &seats), ["small", "big"]);
            let seats = vec![
                wallet("other-small", 40.0, "other\0credits"),
                wallet("632", 282.0, "first\0points"),
                wallet("710", 119.0, "first\0points"),
                wallet("other-big", 900.0, "other\0credits"),
                wallet("051", 6635.0, "first\0points"),
            ];
            assert_eq!(
                routed(&state, name, &seats),
                ["other-big", "051", "632", "other-small", "710"]
            );
        }
        let mut seats = vec![
            wallet("632", 282.0, "first\0points"),
            wallet("other", 50.0, "other\0credits"),
            wallet("051", 6635.0, "first\0points"),
        ];
        seats[0].tokens = 10.0;
        seats[1].tokens = 20.0;
        seats[2].tokens = 30.0;
        assert_eq!(routed(&state, "usage", &seats), ["051", "other", "632"]);
    }

    #[test]
    fn b1c_smart_spent_credits_and_lapses_follow_their_tie_rules() {
        use std::time::{Duration, SystemTime};

        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&["spent", "credits", "low", "fine"]);
        seats[0].used = 98.0;
        seats[1].used = 100.0;
        seats[1].credits = true;
        seats[2].used = 92.0;
        seats[2].credits = true;
        seats[3].used = 20.0;
        for name in ["smart", "pace", "usage"] {
            assert_eq!(
                routed(&state, name, &seats),
                ["fine", "low", "credits", "spent"]
            );
        }

        let reset = SystemTime::UNIX_EPOCH + Duration::from_secs(30 * 86400);
        let mut seats = self::seats(&["later", "sooner", "none"]);
        for (index, lapse) in [200, 100, 0].into_iter().enumerate() {
            seats[index].soon = vec![Some(reset)];
            seats[index].lapse = (lapse > 0)
                .then_some(SystemTime::UNIX_EPOCH + Duration::from_secs(lapse * 3600 + 10));
        }
        assert_eq!(routed(&state, "smart", &seats), ["sooner", "later", "none"]);
    }

    #[test]
    fn b2a_usage_sorts_by_share_then_decayed_tokens() {
        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&["a", "b", "c"]);
        seats[0].used = 50.0;
        seats[1].used = 10.0;
        seats[1].tokens = 9.0;
        seats[2].used = 10.0;
        seats[2].tokens = 1.0;
        assert_eq!(routed(&state, "usage", &seats), ["c", "b", "a"]);
        assert!(state.registry().find("usage").expect("usage").by_use);
    }

    #[test]
    fn b3a_pace_uses_ten_percent_bands_and_allowance_tiers() {
        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&["slow", "low", "fast"]);
        seats[0].pace = 1.0;
        seats[1].used = 95.0;
        seats[1].pace = 50.0;
        seats[2].pace = 10.0;
        assert_eq!(routed(&state, "pace", &seats), ["fast", "slow", "low"]);

        let mut seats = self::seats(&["first", "second", "third", "learner"]);
        seats[0].pace = 12.0;
        seats[1].pace = 11.0;
        seats[2].pace = 10.0;
        seats[3].pace = 100.0;
        seats[3].learns = true;
        assert_eq!(
            routed(&state, "pace", &seats),
            ["learner", "first", "second", "third"]
        );
    }

    #[test]
    fn b4a_fast_estimates_short_replies_and_handles_unknown_speed() {
        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&["a", "b", "c"]);
        seats[0].ttft = 2000.0;
        seats[1].ttft = 600.0;
        seats[2].ttft = 1200.0;
        assert_eq!(routed(&state, "fast", &seats), ["b", "c", "a"]);

        let mut seats = self::seats(&["a", "b"]);
        seats[0].ttft = 500.0;
        seats[0].tps = 20.0;
        seats[1].ttft = 900.0;
        seats[1].tps = 100.0;
        assert_eq!(routed(&state, "fast", &seats), ["b", "a"]);

        let mut seats = self::seats(&["a", "b", "c", "d"]);
        for (seat, ttft, tps) in seats
            .iter_mut()
            .zip([2500.0, 1000.0, 3000.0, 3000.0])
            .zip([1000.0, 0.0, 100.0, 50.0])
            .map(|((seat, ttft), tps)| (seat, ttft, tps))
        {
            seat.ttft = ttft;
            seat.tps = tps;
        }
        assert_eq!(routed(&state, "fast", &seats), ["a", "b", "c", "d"]);

        let mut seats = self::seats(&["resting", "spent", "awake"]);
        seats[0].ttft = 100.0;
        seats[0].resting = true;
        seats[1].ttft = 100.0;
        seats[1].used = 99.0;
        seats[2].ttft = 5000.0;
        assert_eq!(
            routed(&state, "fast", &seats),
            ["awake", "resting", "spent"]
        );
        assert_eq!(
            routed(&state, "fast", &self::seats(&["a", "b"])),
            ["a", "b"]
        );
    }

    #[test]
    fn b4b_fast_probes_each_unmeasured_member_every_fifth_request() {
        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&["a", "b", "c"]);
        seats[0].ttft = 500.0;
        let mut probes = Vec::new();
        let mut counts = [0; 3];
        for request in 1..=20 {
            let order = state.order("fast", "pool", &seats);
            counts[order[0]] += 1;
            if order[0] != 0 {
                probes.push((request, seats[order[0]].key.clone()));
            }
        }
        assert_eq!(
            probes,
            [
                (5, "b".to_owned()),
                (10, "c".to_owned()),
                (15, "b".to_owned()),
                (20, "c".to_owned())
            ]
        );
        assert_eq!(counts, [16, 2, 2]);
        assert_eq!(state.order("fast", "other", &seats)[0], 0);

        seats[1].resting = true;
        seats.remove(2);
        for _ in 0..10 {
            assert_eq!(state.order("fast", "resting", &seats)[0], 0);
        }
        state.forget("pool");
        assert_eq!(state.order("fast", "pool", &seats)[0], 0);
    }

    #[test]
    fn b2c_only_strategies_that_read_measurements_ask_for_them() {
        use super::Needs;
        let state = StrategyState::with_builtins();
        let none = Needs::default();
        for name in ["smart", "order", "rotate", "weight", "missing"] {
            assert_eq!(state.needs(name), none, "{name}");
        }
        assert_eq!(state.needs("free"), Needs { free: true, ..none });
        let usage = Needs {
            usage: true,
            ..none
        };
        assert_eq!(state.needs("usage"), usage);
        assert_eq!(state.needs("pace"), usage);
        assert_eq!(
            state.needs("fast"),
            Needs {
                speed: true,
                ..none
            }
        );
        assert_eq!(
            state.needs("cheap"),
            Needs {
                price: true,
                ..none
            }
        );
    }

    #[test]
    fn b5c_cheap_keeps_listed_order_through_non_transitive_ties() {
        let state = StrategyState::with_builtins();
        // a ~ b and b ~ c within a millionth, but a < c: insertion sort keeps
        // the listed order where neighbours tie, as the reference does
        let mut seats = self::seats(&["c", "b", "a", "dear"]);
        for (index, cost) in [(0, 1.0 + 1.6e-6), (1, 1.0 + 0.8e-6), (2, 1.0), (3, 2.0)] {
            seats[index].priced = true;
            seats[index].cost = cost;
        }
        assert_eq!(routed(&state, "cheap", &seats), ["c", "b", "a", "dear"]);
        seats.swap(0, 3);
        assert_eq!(routed(&state, "cheap", &seats), ["b", "a", "c", "dear"]);
        // a long chain of near ties never panics and keeps every member
        let ids: Vec<&'static str> = (0..64)
            .map(|index| &*Box::leak(format!("m{index}").into_boxed_str()))
            .collect();
        let mut many = self::seats(&ids);
        for (index, seat) in many.iter_mut().enumerate() {
            seat.priced = index % 7 != 0;
            seat.cost = 1.0 + (64 - index) as f64 * 0.7e-6;
            seat.cache_cost = (index % 3) as f64 * 0.4e-6;
        }
        let mut order = state.order("cheap", "pool", &many);
        assert!(many[order[0]].priced);
        assert!(!many[*order.last().expect("members")].priced);
        order.sort_unstable();
        assert_eq!(order, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn b5a_cheap_uses_hourly_price_cache_tie_and_unpriced_last() {
        let state = StrategyState::with_builtins();
        let mut seats = self::seats(&[
            "unpriced-a",
            "dear",
            "cheap-b",
            "cheap-a",
            "tie-1",
            "unpriced-b",
            "tie-2",
            "free",
        ]);
        for (index, cost, cache) in [
            (1, 0.84, 0.0),
            (2, 0.6, 0.02),
            (3, 0.6, 0.01),
            (4, 0.7, 0.0),
            (6, 0.7 + 1e-9, 0.0),
            (7, 0.0, 0.0),
        ] {
            seats[index].priced = true;
            seats[index].cost = cost;
            seats[index].cache_cost = cache;
        }
        assert_eq!(
            routed(&state, "cheap", &seats),
            [
                "free",
                "cheap-a",
                "cheap-b",
                "tie-1",
                "tie-2",
                "dear",
                "unpriced-a",
                "unpriced-b"
            ]
        );
        assert!(state.registry().find("cheap").expect("cheap").by_price);
    }
}
