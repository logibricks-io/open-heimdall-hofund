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

/// Metadata supplied before dispatch begins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteMeta {
    /// Client session id when present.
    pub session: String,
    /// Agent label when known.
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
#[derive(Debug)]
pub struct RouteTraceBook {
    inner: Mutex<BookState>,
    wait: Condvar,
    next_id: AtomicU64,
    seq: AtomicU64,
}

#[derive(Debug)]
struct BookState {
    global: VecDeque<u64>,
    by_session: std::collections::BTreeMap<String, VecDeque<u64>>,
    by_agent: std::collections::BTreeMap<String, VecDeque<u64>>,
    by_id: std::collections::BTreeMap<u64, RoutingDecision>,
    unrouted: VecDeque<UnroutedRequest>,
    next_unrouted_id: u64,
}

impl RouteTraceBook {
    /// Constructs an empty book.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BookState {
                global: VecDeque::new(),
                by_session: std::collections::BTreeMap::new(),
                by_agent: std::collections::BTreeMap::new(),
                by_id: std::collections::BTreeMap::new(),
                unrouted: VecDeque::new(),
                next_unrouted_id: 1,
            }),
            wait: Condvar::new(),
            next_id: AtomicU64::new(1),
            seq: AtomicU64::new(0),
        })
    }

    fn bump_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64)
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
                    decision.time_ms = Self::now_ms();
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

    fn insert(&self, mut decision: RoutingDecision) {
        decision.seq = self.bump_seq();
        decision.done = true;
        let id = decision.id;
        let session = decision.session.clone();
        let agent = decision.agent.clone();
        let mut state = self.inner.lock().expect("route trace lock");
        state.by_id.insert(id, decision);
        push_ring(&mut state, id, TRACE_KEEP, &session, &agent);
        self.wait.notify_all();
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
    pub fn hold_unrouted(&self, session: &str, asked: &str, inbound_protocol: &str, phase: &str) {
        let mut state = self.inner.lock().expect("route trace lock");
        let id = state.next_unrouted_id;
        state.next_unrouted_id = state.next_unrouted_id.saturating_add(1);
        push_unrouted(
            &mut state.unrouted,
            {
                let mut hold = UnroutedRequest::default();
                hold.id = id;
                hold.since_ms = Self::now_ms();
                hold.session = session.to_owned();
                hold.asked = asked.to_owned();
                hold.inbound_protocol = inbound_protocol.to_owned();
                hold.phase = phase.to_owned();
                hold
            },
            TRACE_KEEP,
        );
        self.bump_seq();
        self.wait.notify_all();
    }

    /// Clears unrouted holds for a session once routing begins.
    pub fn clear_unrouted_session(&self, session: &str) {
        if session.is_empty() {
            return;
        }
        let mut state = self.inner.lock().expect("route trace lock");
        state.unrouted.retain(|hold| hold.session != session);
        self.bump_seq();
        self.wait.notify_all();
    }

    /// Lists active unrouted holds.
    #[must_use]
    pub fn unrouted(&self) -> UnroutedListResponse {
        let state = self.inner.lock().expect("route trace lock");
        let mut response = UnroutedListResponse::default();
        response.holds = state.unrouted.iter().cloned().collect();
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

fn push_unrouted(deque: &mut VecDeque<UnroutedRequest>, value: UnroutedRequest, keep: usize) {
    deque.push_back(value);
    while deque.len() > keep {
        deque.pop_front();
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
        if !self.builder.decision.session.is_empty() {
            self.book
                .clear_unrouted_session(&self.builder.decision.session);
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
    use super::{RouteMeta, RouteTraceBook, TRACE_KEEP};

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
    fn r1_5_unrouted_hold_lists_and_clears_on_finish() {
        let book = RouteTraceBook::new();
        book.hold_unrouted("s1", "demo", "plain", "waiting");
        assert_eq!(book.unrouted().holds.len(), 1);
        let handle = book.begin(RouteMeta {
            session: "s1".to_owned(),
            asked: "demo".to_owned(),
            ..RouteMeta::default()
        });
        handle.finish(None, None, Some(200), None, None);
        assert!(book.unrouted().holds.is_empty());
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
}
