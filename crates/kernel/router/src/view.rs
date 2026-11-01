//! Bounded in-memory routing decision history for the management API.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_control_proto::{
    ModelMove, RouteCandidate, RouteListResponse, RouteQuery, RouteTry, RoutingDecision,
    SessionRouteResponse, SkipReason, UnroutedListResponse, UnroutedRequest,
};
use hofund_plugin_api::PluginError;

/// How many decisions each ring retains.
pub const TRACE_KEEP: usize = 60;

/// How long a pre-route hold waits before the unrouted API lists it.
pub const UNROUTED_SHOW: Duration = Duration::from_secs(15);
/// How long a hold may sit in no known wait before timing out.
pub const UNROUTED_UNKNOWN: Duration = Duration::from_secs(30);
/// How long a hold may sit before routing, excluding masking time.
pub const UNROUTED_MOST: Duration = Duration::from_secs(120);

/// Phase label while privacy masking runs; its time does not count toward show or most.
pub const UNROUTED_PHASE_MASKING: &str = "masking";

/// Milliseconds source for routing trace and unrouted timing.
pub type TraceNow = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Metadata supplied before dispatch begins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteMeta {
    /// Client session id when present.
    pub session: String,
    /// Agent label when known (subagent header, stream client User-Agent, or gateway key name).
    pub agent: String,
    /// Named gateway key id when present.
    pub gateway_key: String,
    /// Request correlation id when present.
    pub request_id: String,
    /// Model the client asked for.
    pub asked: String,
    /// Stand-in move before routing, when any.
    pub moved: Option<ModelMove>,
}

/// Shared routing trace storage.
pub struct RouteTraceBook {
    inner: Mutex<BookState>,
    wait: Condvar,
    next_id: AtomicU64,
    seq: AtomicU64,
    clock: TraceNow,
}

#[derive(Debug)]
struct UnroutedEntry {
    hold: UnroutedRequest,
    masking_depth: u32,
    masking_excused_ms: u64,
    masking_since_ms: Option<u64>,
    known_depth: u32,
    known_excused_ms: u64,
    known_since_ms: Option<u64>,
}

impl UnroutedEntry {
    /// Elapsed time that counts toward the hold limit (masking excluded).
    fn most_elapsed_ms(&self, now_ms: u64) -> u64 {
        let mut elapsed = now_ms.saturating_sub(self.hold.since_ms);
        elapsed = elapsed.saturating_sub(self.masking_excused_ms);
        if self.masking_depth > 0 {
            elapsed = elapsed.saturating_sub(
                self.masking_since_ms
                    .map_or(0, |since| now_ms.saturating_sub(since)),
            );
        }
        elapsed
    }

    /// Elapsed time outside known waits and masking (for the unknown timer).
    fn unknown_elapsed_ms(&self, now_ms: u64) -> u64 {
        let mut elapsed = self.most_elapsed_ms(now_ms);
        elapsed = elapsed.saturating_sub(self.known_excused_ms);
        if self.known_depth > 0 {
            elapsed = elapsed.saturating_sub(
                self.known_since_ms
                    .map_or(0, |since| now_ms.saturating_sub(since)),
            );
        }
        elapsed
    }
}

/// Why a pre-route hold ended with an error response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnroutedTimeout {
    /// Held too long outside known waits.
    Unknown,
    /// Held too long excluding masking.
    Most,
}

/// HTTP status the gateway uses for [`UnroutedTimeout`].
#[must_use]
pub const fn unrouted_timeout_status(kind: UnroutedTimeout) -> u16 {
    match kind {
        UnroutedTimeout::Unknown | UnroutedTimeout::Most => 504,
    }
}

#[derive(Debug)]
struct BookState {
    global: VecDeque<u64>,
    by_session: std::collections::BTreeMap<String, VecDeque<u64>>,
    by_agent: std::collections::BTreeMap<String, VecDeque<u64>>,
    by_id: std::collections::BTreeMap<u64, RoutingDecision>,
    unrouted: std::collections::BTreeMap<u64, UnroutedEntry>,
    unrouted_order: VecDeque<u64>,
    next_unrouted_id: u64,
}

/// RAII registration of one request waiting before routing begins.
pub struct UnroutedHold {
    book: Arc<RouteTraceBook>,
    id: u64,
}

impl UnroutedHold {
    /// Hold id for timeout polling.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Updates the model name once the body is decoded.
    pub fn set_asked(&self, asked: &str) {
        self.with_entry(|entry| entry.hold.asked = asked.to_owned());
    }

    /// Sets the phase shown on the unrouted view.
    pub fn set_phase(&self, phase: &str) {
        self.with_entry(|entry| entry.hold.phase = phase.to_owned());
    }

    /// Marks a known wait whose time counts toward the hold limit but not unknown.
    pub fn begin_known_wait(&self, phase: &str) {
        let now = self.book.now_ms();
        self.with_entry(|entry| {
            if !phase.is_empty() {
                entry.hold.phase = phase.to_owned();
            }
            if entry.known_depth == 0 {
                entry.known_since_ms = Some(now);
            }
            entry.known_depth = entry.known_depth.saturating_add(1);
        });
    }

    /// Ends one known wait started with [`Self::begin_known_wait`].
    pub fn end_known_wait(&self) {
        let now = self.book.now_ms();
        self.with_entry(|entry| {
            if entry.known_depth == 0 {
                return;
            }
            entry.known_depth -= 1;
            if entry.known_depth == 0
                && let Some(since) = entry.known_since_ms.take()
            {
                entry.known_excused_ms = entry
                    .known_excused_ms
                    .saturating_add(now.saturating_sub(since));
            }
        });
    }

    /// Marks privacy masking; its elapsed time does not count toward show or most.
    pub fn begin_masking(&self) {
        let now = self.book.now_ms();
        self.with_entry(|entry| {
            entry.hold.phase = UNROUTED_PHASE_MASKING.to_owned();
            if entry.masking_depth == 0 {
                entry.masking_since_ms = Some(now);
            }
            entry.masking_depth = entry.masking_depth.saturating_add(1);
        });
    }

    /// Ends one masking scope started with [`Self::begin_masking`].
    pub fn end_masking(&self) {
        let now = self.book.now_ms();
        self.with_entry(|entry| {
            if entry.masking_depth == 0 {
                return;
            }
            entry.masking_depth -= 1;
            if entry.masking_depth == 0 {
                if let Some(since) = entry.masking_since_ms.take() {
                    entry.masking_excused_ms = entry
                        .masking_excused_ms
                        .saturating_add(now.saturating_sub(since));
                }
                if entry.hold.phase == UNROUTED_PHASE_MASKING {
                    entry.hold.phase.clear();
                }
            }
        });
    }

    /// Returns how long until this hold should time out.
    #[must_use]
    pub fn timeout_after(&self) -> Option<(Duration, UnroutedTimeout)> {
        self.book.unrouted_timeout_after_id(self.id)
    }

    fn with_entry(&self, update: impl FnOnce(&mut UnroutedEntry)) {
        let mut state = self.book.inner.lock().expect("route trace lock");
        let Some(entry) = state.unrouted.get_mut(&self.id) else {
            return;
        };
        update(entry);
        self.book.bump_seq();
        self.book.wait.notify_all();
    }
}

impl Drop for UnroutedHold {
    fn drop(&mut self) {
        self.book.remove_unrouted_hold(self.id);
    }
}

impl RouteTraceBook {
    /// Constructs an empty book using the system clock.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_clock(Arc::new(system_now_ms))
    }

    /// Constructs an empty book with an injected clock for tests.
    #[must_use]
    pub fn with_clock(clock: TraceNow) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BookState {
                global: VecDeque::new(),
                by_session: std::collections::BTreeMap::new(),
                by_agent: std::collections::BTreeMap::new(),
                by_id: std::collections::BTreeMap::new(),
                unrouted: std::collections::BTreeMap::new(),
                unrouted_order: VecDeque::new(),
                next_unrouted_id: 1,
            }),
            wait: Condvar::new(),
            next_id: AtomicU64::new(1),
            seq: AtomicU64::new(0),
            clock,
        })
    }

    fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    fn bump_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Opens a mutable builder for one dispatch.
    pub fn begin(self: &Arc<Self>, meta: RouteMeta) -> RouteTraceHandle {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let seq = self.bump_seq();
        RouteTraceHandle {
            book: Arc::clone(self),
            builder: RouteTraceBuilder {
                decision: {
                    let mut decision = RoutingDecision::default();
                    decision.id = id;
                    decision.seq = seq;
                    decision.time_ms = self.now_ms();
                    decision.session = meta.session;
                    decision.agent = meta.agent;
                    decision.gateway_key = meta.gateway_key;
                    decision.request_id = meta.request_id;
                    decision.asked = meta.asked;
                    decision.moved = meta.moved;
                    decision
                },
            },
            finished: false,
        }
    }

    fn upsert(&self, mut decision: RoutingDecision, done: bool) {
        decision.seq = self.bump_seq();
        decision.done = done;
        let id = decision.id;
        let session = decision.session.clone();
        let agent = decision.agent.clone();
        let mut state = self.inner.lock().expect("route trace lock");
        let first = !state.by_id.contains_key(&id);
        state.by_id.insert(id, decision);
        if first {
            push_ring(&mut state, id, TRACE_KEEP, &session, &agent);
        }
        self.wait.notify_all();
    }

    fn insert(&self, decision: RoutingDecision) {
        self.upsert(decision, true);
    }

    /// Lists decisions for the management API.
    #[must_use]
    pub fn list(&self, query: &RouteQuery) -> RouteListResponse {
        let state = self.inner.lock().expect("route trace lock");
        let mut decisions = collect_ids(&state, query);
        decisions.sort_by_key(|left| std::cmp::Reverse(left.id));
        if decisions.len() > query.limit {
            decisions.truncate(query.limit);
        }
        let mut response = RouteListResponse::default();
        response.seq = self.seq.load(Ordering::SeqCst);
        response.decisions = decisions;
        response
    }

    /// Waits until `after` is surpassed or `wait` elapses, then lists.
    pub fn list_wait(&self, query: RouteQuery, wait: Duration) -> RouteListResponse {
        let after = query.after.unwrap_or(0);
        if wait.is_zero() || self.seq.load(Ordering::SeqCst) > after {
            return self.list(&query);
        }
        let state = self.inner.lock().expect("route trace lock");
        let (state, _) = self
            .wait
            .wait_timeout_while(state, wait, |_book| {
                self.seq.load(Ordering::SeqCst) <= after
            })
            .expect("route trace wait");
        drop(state);
        self.list(&query)
    }

    /// Returns the latest decision for one session.
    #[must_use]
    pub fn session_latest(&self, session: &str, gateway_key: Option<&str>) -> SessionRouteResponse {
        let state = self.inner.lock().expect("route trace lock");
        let mut response = SessionRouteResponse::empty(session);
        let Some(ids) = state.by_session.get(session) else {
            return response;
        };
        for id in ids.iter().rev() {
            let Some(decision) = state.by_id.get(id) else {
                continue;
            };
            if gateway_key.is_some_and(|key| decision.gateway_key != key) {
                continue;
            }
            response.seq = decision.seq;
            response.decision = Some(decision.clone());
            break;
        }
        response
    }

    /// Waits for a session decision past `after`.
    pub fn session_wait(
        &self,
        session: &str,
        after: u64,
        wait: Duration,
        gateway_key: Option<&str>,
    ) -> SessionRouteResponse {
        if wait.is_zero() {
            return self.session_latest(session, gateway_key);
        }
        let deadline = std::time::Instant::now() + wait;
        loop {
            let response = self.session_latest(session, gateway_key);
            if response.seq > after || response.decision.is_none() {
                return response;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return response;
            }
            let state = self.inner.lock().expect("route trace lock");
            let (state, _) = self
                .wait
                .wait_timeout(state, remaining.min(Duration::from_secs(1)))
                .expect("route trace wait");
            drop(state);
        }
    }

    /// Records a request waiting before routing starts.
    #[must_use]
    pub fn hold_unrouted(
        self: &Arc<Self>,
        session: &str,
        asked: &str,
        inbound_protocol: &str,
        phase: &str,
    ) -> UnroutedHold {
        let mut state = self.inner.lock().expect("route trace lock");
        let id = state.next_unrouted_id;
        state.next_unrouted_id = state.next_unrouted_id.saturating_add(1);
        let mut hold = UnroutedRequest::default();
        hold.id = id;
        hold.since_ms = self.now_ms();
        hold.session = session.to_owned();
        hold.asked = asked.to_owned();
        hold.inbound_protocol = inbound_protocol.to_owned();
        hold.phase = phase.to_owned();
        state.unrouted.insert(
            id,
            UnroutedEntry {
                hold,
                masking_depth: 0,
                masking_excused_ms: 0,
                masking_since_ms: None,
                known_depth: 0,
                known_excused_ms: 0,
                known_since_ms: None,
            },
        );
        state.unrouted_order.push_back(id);
        while state.unrouted_order.len() > TRACE_KEEP {
            let Some(old) = state.unrouted_order.pop_front() else {
                break;
            };
            state.unrouted.remove(&old);
        }
        self.bump_seq();
        self.wait.notify_all();
        UnroutedHold {
            book: Arc::clone(self),
            id,
        }
    }

    fn remove_unrouted_hold(&self, id: u64) {
        let mut state = self.inner.lock().expect("route trace lock");
        if state.unrouted.remove(&id).is_some() {
            state.unrouted_order.retain(|entry| *entry != id);
            self.bump_seq();
            self.wait.notify_all();
        }
    }

    /// Returns how long until one hold should time out.
    #[must_use]
    pub fn unrouted_timeout_after_id(&self, id: u64) -> Option<(Duration, UnroutedTimeout)> {
        let state = self.inner.lock().expect("route trace lock");
        let now = self.now_ms();
        let entry = state.unrouted.get(&id)?;
        let unknown_ms = UNROUTED_UNKNOWN.as_millis() as u64;
        let most_ms = UNROUTED_MOST.as_millis() as u64;
        let most_elapsed = entry.most_elapsed_ms(now);
        if most_elapsed >= most_ms {
            return Some((Duration::ZERO, UnroutedTimeout::Most));
        }
        if entry.known_depth == 0 && entry.unknown_elapsed_ms(now) >= unknown_ms {
            return Some((Duration::ZERO, UnroutedTimeout::Unknown));
        }
        let until_most = Duration::from_millis(most_ms.saturating_sub(most_elapsed));
        if entry.known_depth == 0 {
            let until_unknown =
                Duration::from_millis(unknown_ms.saturating_sub(entry.unknown_elapsed_ms(now)));
            if until_unknown <= until_most {
                return Some((until_unknown, UnroutedTimeout::Unknown));
            }
        }
        Some((until_most, UnroutedTimeout::Most))
    }

    /// Lists active unrouted holds visible after the show delay.
    #[must_use]
    pub fn unrouted(&self) -> UnroutedListResponse {
        let state = self.inner.lock().expect("route trace lock");
        let now = self.now_ms();
        let show_ms = UNROUTED_SHOW.as_millis() as u64;
        let mut response = UnroutedListResponse::default();
        response.holds = state
            .unrouted_order
            .iter()
            .filter_map(|id| state.unrouted.get(id))
            .filter(|entry| entry.most_elapsed_ms(now) >= show_ms)
            .map(|entry| entry.hold.clone())
            .collect();
        response
    }

    /// Records a routing failure before any member was tried.
    pub fn record_unrouted(self: &Arc<Self>, meta: RouteMeta, error: &str) {
        let mut handle = self.begin(meta);
        handle.set_routing("smart");
        handle.mark_unrouted(error);
        handle.finish(None, None, None, None, None);
    }
}

fn push_ring(state: &mut BookState, id: u64, keep: usize, session: &str, agent: &str) {
    state.global.push_back(id);
    while state.global.len() > keep {
        let Some(old) = state.global.pop_front() else {
            break;
        };
        state.by_id.remove(&old);
        for index in state.by_session.values_mut() {
            index.retain(|entry| *entry != old);
        }
        for index in state.by_agent.values_mut() {
            index.retain(|entry| *entry != old);
        }
    }
    if !session.is_empty() {
        push_index(
            state.by_session.entry(session.to_owned()).or_default(),
            id,
            keep,
        );
    }
    if !agent.is_empty() {
        push_index(
            state.by_agent.entry(agent.to_owned()).or_default(),
            id,
            keep,
        );
    }
}

fn push_index(index: &mut VecDeque<u64>, id: u64, keep: usize) {
    index.push_back(id);
    while index.len() > keep {
        index.pop_front();
    }
}

fn collect_ids(state: &BookState, query: &RouteQuery) -> Vec<RoutingDecision> {
    let ids: Vec<u64> = if let Some(session) = &query.session {
        state
            .by_session
            .get(session)
            .map_or_else(Vec::new, |deque| deque.iter().copied().collect())
    } else {
        state.global.iter().copied().collect()
    };
    ids.into_iter()
        .filter_map(|id| state.by_id.get(&id).cloned())
        .filter(|decision| {
            query
                .gateway_key
                .as_ref()
                .is_none_or(|key| decision.gateway_key == *key)
        })
        .collect()
}

/// In-flight routing trace for one request.
pub struct RouteTraceHandle {
    book: Arc<RouteTraceBook>,
    builder: RouteTraceBuilder,
    finished: bool,
}

impl RouteTraceHandle {
    /// Sets the pool routing strategy name.
    pub fn set_routing(&mut self, routing: &str) {
        self.builder.decision.routing = routing.to_owned();
    }

    /// Records a stand-in model move before routing completes.
    pub fn set_moved(&mut self, to: &str, why: &str) {
        self.builder.decision.moved = Some(ModelMove::new(to, why));
    }

    /// Records ordered candidates.
    pub fn set_candidates(&mut self, candidates: Vec<RouteCandidate>) {
        self.builder.decision.candidates = candidates;
    }

    /// Records the routing group id when the client asked a pool route.
    pub fn set_group(&mut self, group: &str) {
        self.builder.decision.group = group.to_owned();
    }

    /// Records the conversation affinity decision before attempts run.
    pub fn set_affinity_decision(&mut self, decision: &str) {
        self.builder.decision.affinity = decision.to_owned();
    }

    /// Publishes the in-flight decision to the management API.
    pub fn sync_open(&mut self) {
        let decision = self.builder.decision.clone();
        self.book.upsert(decision, false);
    }

    /// Marks the decision as unrouted.
    pub fn mark_unrouted(&mut self, error: &str) {
        self.builder.decision.unrouted = true;
        self.builder.decision.error = error.to_owned();
    }

    /// Notes one attempt.
    pub fn push_try(&mut self, try_record: RouteTry) {
        self.builder.decision.tries.push(try_record);
        self.book.bump_seq();
    }

    /// Updates skip metadata for a candidate index.
    pub fn skip_candidate(&mut self, order: u32, reason: SkipReason) {
        if let Some(candidate) = self
            .builder
            .decision
            .candidates
            .iter_mut()
            .find(|candidate| candidate.order == order)
        {
            candidate.skipped = true;
            candidate.skip = Some(reason);
        }
    }

    /// Completes the trace after dispatch.
    pub fn finish(
        mut self,
        served_member: Option<&str>,
        served_provider: Option<&str>,
        status: Option<u16>,
        error: Option<&str>,
        affinity: Option<&str>,
    ) {
        if let Some(member) = served_member {
            self.builder.decision.served_member = member.to_owned();
        }
        if let Some(provider) = served_provider {
            self.builder.decision.served_provider = provider.to_owned();
        }
        if let Some(status) = status {
            self.builder.decision.status = Some(status);
        }
        if let Some(error) = error
            && self.builder.decision.error.is_empty()
        {
            self.builder.decision.error = error.to_owned();
        }
        if let Some(affinity) = affinity {
            self.builder.decision.affinity = affinity.to_owned();
        }
        self.finished = true;
        let decision = std::mem::take(&mut self.builder.decision);
        self.book.insert(decision);
        std::mem::forget(self);
    }
}

impl Drop for RouteTraceHandle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.builder.decision.error.is_empty() {
            self.builder.decision.error = "routing ended before a decision was recorded".to_owned();
        }
        self.builder.decision.unrouted = true;
        let decision = std::mem::take(&mut self.builder.decision);
        self.book.insert(decision);
    }
}

#[derive(Debug)]
struct RouteTraceBuilder {
    decision: RoutingDecision,
}

/// Builds candidate rows from ordered hops and cooldown state.
#[must_use]
pub fn candidates_from_hops(
    hops: &[crate::Hop],
    bare_model: &str,
    spent: &[bool],
    cooling: &[Option<String>],
) -> Vec<RouteCandidate> {
    hops.iter()
        .enumerate()
        .map(|(index, hop)| {
            let model = hop.wire_model.as_deref().unwrap_or(bare_model).to_owned();
            let skip = if spent.get(index).copied().unwrap_or(false) {
                Some(SkipReason::Quota)
            } else {
                cooling
                    .get(index)
                    .and_then(|value| value.clone())
                    .map(|cause| SkipReason::Cooldown { cause })
            };
            RouteCandidate::new(
                hop.member.id().to_string(),
                hop.member.route_id(),
                model,
                index as u32,
                skip.is_some(),
                skip,
            )
        })
        .collect()
}

/// Sanitized failure text for a trace try.
#[must_use]
pub fn fail_text(error: &PluginError) -> String {
    let message = error.message();
    if message.len() <= 240 {
        return message.to_owned();
    }
    let mut end = 240;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

/// HTTP status for a trace try when present.
#[must_use]
pub fn try_status(error: &PluginError) -> Option<u16> {
    error.status()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use std::time::Duration;

    use super::{
        RouteMeta, RouteTraceBook, TRACE_KEEP, TraceNow, UNROUTED_MOST, UNROUTED_SHOW,
        UNROUTED_UNKNOWN, UnroutedTimeout, unrouted_timeout_status,
    };
    use std::collections::VecDeque;

    use hofund_control_proto::RouteQuery;

    fn test_clock(start: u64) -> (TraceNow, Arc<AtomicU64>) {
        let millis = Arc::new(AtomicU64::new(start));
        let shared = Arc::clone(&millis);
        let clock: TraceNow = Arc::new(move || shared.load(Ordering::SeqCst));
        (clock, millis)
    }

    #[test]
    fn r1_3_trace_ring_drops_oldest_finished_decisions() {
        let book = RouteTraceBook::new();
        for index in 0..(TRACE_KEEP + 5) {
            let handle = book.begin(RouteMeta {
                asked: format!("m{index}"),
                ..RouteMeta::default()
            });
            handle.finish(None, None, None, None, None);
        }
        let list = book.list(&Default::default());
        assert_eq!(list.decisions.len(), TRACE_KEEP);
    }

    #[test]
    fn r1_4_session_index_returns_latest_matching_row() {
        let book = RouteTraceBook::new();
        for (session, asked) in [("s1", "a"), ("s1", "b"), ("s2", "c")] {
            let handle = book.begin(RouteMeta {
                session: session.to_owned(),
                asked: asked.to_owned(),
                ..RouteMeta::default()
            });
            handle.finish(None, None, None, None, None);
        }
        let latest = book.session_latest("s1", None);
        assert_eq!(
            latest.decision.as_ref().map(|row| row.asked.as_str()),
            Some("b")
        );
    }

    #[test]
    fn r1_5_unrouted_hold_lists_and_clears_when_routing_begins() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        assert!(book.unrouted().holds.is_empty());
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        assert_eq!(book.unrouted().holds.len(), 1);
        let handle = book.begin(RouteMeta {
            session: "s1".to_owned(),
            asked: "demo".to_owned(),
            ..RouteMeta::default()
        });
        drop(hold);
        assert!(book.unrouted().holds.is_empty());
        handle.finish(None, None, Some(200), None, None);
    }

    #[test]
    fn r1_7_moved_to_and_why_are_stored_on_the_decision() {
        let book = RouteTraceBook::new();
        let mut handle = book.begin(RouteMeta {
            asked: "asked-model".to_owned(),
            ..RouteMeta::default()
        });
        handle.set_moved("wire-model", "stand-in");
        handle.finish(None, None, Some(200), None, None);
        let list = book.list(&Default::default());
        let row = list.decisions[0].moved.as_ref().expect("moved");
        assert_eq!(row.to, "wire-model");
        assert_eq!(row.why, "stand-in");
    }

    #[test]
    fn r1_9_group_and_affinity_decision_are_stored() {
        let book = RouteTraceBook::new();
        let mut handle = book.begin(RouteMeta {
            asked: "pool/demo".to_owned(),
            ..RouteMeta::default()
        });
        handle.set_group("pool/demo");
        handle.set_affinity_decision("kept previous answerer");
        handle.sync_open();
        let row = book.list(&Default::default()).decisions[0].clone();
        assert_eq!(row.group, "pool/demo");
        assert_eq!(row.affinity, "kept previous answerer");
        assert!(!row.done);
    }

    #[test]
    fn r1_10_agent_field_is_stored_on_the_decision() {
        let book = RouteTraceBook::new();
        let handle = book.begin(RouteMeta {
            agent: "agent-alpha".to_owned(),
            asked: "demo".to_owned(),
            ..RouteMeta::default()
        });
        handle.finish(None, None, Some(200), None, None);
        assert_eq!(
            book.list(&Default::default()).decisions[0].agent,
            "agent-alpha"
        );
    }

    #[test]
    fn r1_11_masking_time_does_not_count_toward_show_or_most() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        hold.begin_masking();
        now.store(UNROUTED_MOST.as_millis() as u64, Ordering::SeqCst);
        assert!(book.unrouted().holds.is_empty());
        hold.end_masking();
        now.store(
            UNROUTED_MOST.as_millis() as u64 + UNROUTED_SHOW.as_millis() as u64,
            Ordering::SeqCst,
        );
        assert_eq!(book.unrouted().holds.len(), 1);
    }

    #[test]
    fn r1_12_unrouted_show_delay_hides_young_holds() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let _hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        assert!(book.unrouted().holds.is_empty());
        now.store(UNROUTED_SHOW.as_millis() as u64 - 1, Ordering::SeqCst);
        assert!(book.unrouted().holds.is_empty());
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        assert_eq!(book.unrouted().holds.len(), 1);
    }

    #[test]
    fn r1_13_unrouted_most_and_unknown_timeouts_use_504() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        now.store(UNROUTED_UNKNOWN.as_millis() as u64, Ordering::SeqCst);
        let (wait, kind) = hold.timeout_after().expect("unknown timeout");
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(kind, UnroutedTimeout::Unknown);
        assert_eq!(unrouted_timeout_status(kind), 504);

        drop(hold);
        now.store(0, Ordering::SeqCst);
        let hold = book.hold_unrouted("s2", "demo", "plain", "waiting");
        now.store(UNROUTED_MOST.as_millis() as u64, Ordering::SeqCst);
        let (wait, kind) = hold.timeout_after().expect("most timeout");
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(kind, UnroutedTimeout::Most);
        assert_eq!(unrouted_timeout_status(kind), 504);
    }

    #[test]
    fn r1_15_empty_session_hold_drops_without_leaking() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        {
            let _hold = book.hold_unrouted("", "demo", "plain", "waiting");
        }
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        assert!(book.unrouted().holds.is_empty());
    }

    #[test]
    fn r1_16_concurrent_holds_with_the_same_session_stay_independent() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let first = book.hold_unrouted("shared", "first", "plain", "waiting");
        let second = book.hold_unrouted("shared", "second", "plain", "waiting");
        second.set_asked("updated");
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        let listed = book.unrouted();
        let visible: Vec<_> = listed.holds.iter().map(|row| row.asked.as_str()).collect();
        assert_eq!(visible.len(), 2);
        assert!(visible.contains(&"first"));
        assert!(visible.contains(&"updated"));
        drop(first);
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        assert_eq!(book.unrouted().holds.len(), 1);
        assert_eq!(book.unrouted().holds[0].asked, "updated");
    }

    #[test]
    fn r1_18_released_hold_has_no_timeout() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        let id = hold.id();
        drop(hold);
        now.store(UNROUTED_MOST.as_millis() as u64, Ordering::SeqCst);
        assert!(book.unrouted_timeout_after_id(id).is_none());
    }

    #[test]
    fn r1_19_long_known_wait_times_out_as_most() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "body");
        hold.begin_known_wait("body");
        now.store(130_000, Ordering::SeqCst);
        let (wait, kind) = hold.timeout_after().expect("most after known wait");
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(kind, UnroutedTimeout::Most);
    }

    #[test]
    fn r1_20_timeout_after_returns_true_remaining_time() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        hold.begin_known_wait("body");
        now.store(UNROUTED_MOST.as_millis() as u64 - 1, Ordering::SeqCst);
        let (wait, kind) = hold.timeout_after().expect("near most");
        assert_eq!(wait, Duration::from_millis(1));
        assert_eq!(kind, UnroutedTimeout::Most);
    }

    #[test]
    fn r1_21_ended_masking_stays_excused_from_most() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "body");
        hold.begin_known_wait("body");
        hold.begin_masking();
        now.store(100_000, Ordering::SeqCst);
        hold.end_masking();
        now.store(
            100_000 + UNROUTED_MOST.as_millis() as u64 - 1,
            Ordering::SeqCst,
        );
        let (wait, kind) = hold.timeout_after().expect("timeout");
        assert_eq!(kind, UnroutedTimeout::Most);
        assert_eq!(wait, Duration::from_millis(1));
    }

    #[test]
    fn r1_22_ended_known_wait_stays_excused_from_unknown() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "body");
        hold.begin_known_wait("body");
        now.store(50_000, Ordering::SeqCst);
        hold.end_known_wait();
        now.store(
            50_000 + UNROUTED_UNKNOWN.as_millis() as u64 - 1,
            Ordering::SeqCst,
        );
        let (wait, kind) = hold.timeout_after().expect("timeout");
        assert_eq!(kind, UnroutedTimeout::Unknown);
        assert_eq!(wait, Duration::from_millis(1));
    }

    #[test]
    fn r1_23_running_known_wait_suspends_the_unknown_timer() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        let late = UNROUTED_UNKNOWN.as_millis() as u64 + 1_000;
        now.store(late, Ordering::SeqCst);
        hold.begin_known_wait("body");
        let (wait, kind) = hold.timeout_after().expect("timeout");
        assert_eq!(kind, UnroutedTimeout::Most);
        assert_eq!(
            wait,
            Duration::from_millis(UNROUTED_MOST.as_millis() as u64 - late)
        );
    }

    #[test]
    fn r1_24_fresh_hold_reports_the_nearer_unknown_timeout() {
        let (clock, _now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let hold = book.hold_unrouted("s1", "demo", "plain", "waiting");
        let (wait, kind) = hold.timeout_after().expect("timeout");
        assert_eq!(kind, UnroutedTimeout::Unknown);
        assert_eq!(wait, UNROUTED_UNKNOWN);
    }

    #[test]
    fn r1_25_unrouted_holds_are_capped_at_trace_keep() {
        let (clock, now) = test_clock(0);
        let book = RouteTraceBook::with_clock(clock);
        let holds: Vec<_> = (0..TRACE_KEEP + 5)
            .map(|index| book.hold_unrouted(&format!("s{index}"), "demo", "plain", "waiting"))
            .collect();
        now.store(UNROUTED_SHOW.as_millis() as u64, Ordering::SeqCst);
        assert_eq!(book.unrouted().holds.len(), TRACE_KEEP);
        drop(holds);
    }

    #[test]
    fn r1_26_ring_cap_and_list_limit_are_separate() {
        let book = RouteTraceBook::new();
        for index in 0..(TRACE_KEEP + 5) {
            let handle = book.begin(RouteMeta {
                session: format!("s{index}"),
                asked: format!("m{index}"),
                ..RouteMeta::default()
            });
            handle.finish(None, None, None, None, None);
        }
        let wide = RouteQuery {
            limit: TRACE_KEEP * 10,
            ..RouteQuery::default()
        };
        assert_eq!(book.list(&wide).decisions.len(), TRACE_KEEP);
        let narrow = RouteQuery {
            limit: 2,
            ..RouteQuery::default()
        };
        assert_eq!(book.list(&narrow).decisions.len(), 2);
        let state = book.inner.lock().expect("route trace lock");
        let indexed: usize = state.by_session.values().map(VecDeque::len).sum();
        assert_eq!(
            indexed, TRACE_KEEP,
            "evicted decisions leave the session index"
        );
    }
}
