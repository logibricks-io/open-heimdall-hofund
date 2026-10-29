//! Per-topic event buses behind `GET /v1/hofund/events`.
//!
//! Each topic owns an independent broadcast channel and replay ring, so a
//! burst of request events can neither lag nor evict control events. `emit`
//! assigns the sequence, appends to the ring, and broadcasts while holding the
//! ring lock; subscribers take their broadcast receiver before reading the
//! ring, so every live frame is either in the replay range or arrives on the
//! receiver, and frames at or below the snapshot cursor are suppressed.

use std::collections::VecDeque;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use hofund_control_proto::{
    EventCursor, EventEnvelope, EventId, EventPayload, Resume, ResyncReason, ResyncRequiredEvent,
    ShutdownStartedEvent, ShutdownState, SnapshotEvent, TokenRotatedEvent, Topic, resume_plan,
};
use hofund_gateway::control::{EventSubscription, EventsQuery, SseFrame};
use tokio::sync::{broadcast, watch};

/// Broadcast capacity per topic. A subscriber that falls further behind than
/// this resynchronizes only the lagging topic.
pub(crate) const TOPIC_CAPACITY: usize = 4096;
/// Retained replay events per topic.
pub(crate) const RING_EVENTS: usize = 4096;
/// Retained replay bytes per topic, counted over serialized envelopes.
pub(crate) const RING_BYTES: usize = 4 * 1024 * 1024;

/// Builds the per-connection state snapshot. Cursors are filled by the bus.
pub(crate) type SnapshotSource = Arc<dyn Fn() -> SnapshotEvent + Send + Sync>;

/// One serialized event shared by the ring and every live subscriber.
#[derive(Debug)]
pub(crate) struct Frame {
    topic: Topic,
    seq: u64,
    event: &'static str,
    json: Arc<str>,
    closing: bool,
}

impl Frame {
    fn bytes(&self) -> usize {
        self.json.len()
    }
}

#[derive(Debug, Default)]
struct Ring {
    frames: VecDeque<Arc<Frame>>,
    bytes: usize,
    last: u64,
}

impl Ring {
    fn push(&mut self, frame: Arc<Frame>) {
        self.last = frame.seq;
        self.bytes += frame.bytes();
        self.frames.push_back(frame);
        while self.frames.len() > RING_EVENTS || self.bytes > RING_BYTES {
            let Some(evicted) = self.frames.pop_front() else {
                break;
            };
            self.bytes -= evicted.bytes();
        }
    }

    /// Frames after `cursor`, or `None` when part of that range was evicted
    /// or the cursor is ahead of this boot's sequence space.
    fn after(&self, cursor: u64) -> Option<Vec<Arc<Frame>>> {
        if cursor > self.last {
            return None;
        }
        let oldest = self.frames.front().map_or(self.last + 1, |frame| frame.seq);
        if cursor + 1 < oldest {
            return None;
        }
        Some(
            self.frames
                .iter()
                .filter(|frame| frame.seq > cursor)
                .cloned()
                .collect(),
        )
    }
}

struct TopicBus {
    sender: broadcast::Sender<Arc<Frame>>,
    ring: Mutex<Ring>,
}

impl TopicBus {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(TOPIC_CAPACITY);
        Self {
            sender,
            ring: Mutex::new(Ring::default()),
        }
    }

    fn ring(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn last(&self) -> u64 {
        self.ring().last
    }
}

/// Process-wide event buses for one daemon boot.
pub(crate) struct EventBus {
    boot_id: String,
    control: TopicBus,
    requests: TopicBus,
}

impl fmt::Debug for EventBus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventBus")
            .field("boot_id", &self.boot_id)
            .field("control", &self.control.last())
            .field("requests", &self.requests.last())
            .finish()
    }
}

impl EventBus {
    pub(crate) fn new() -> Self {
        Self::with_boot_id(new_boot_id())
    }

    pub(crate) fn with_boot_id(boot_id: String) -> Self {
        Self {
            boot_id,
            control: TopicBus::new(),
            requests: TopicBus::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Encoded control-topic events still held in the replay ring.
    #[cfg(test)]
    pub(crate) fn control_frames(&self) -> Vec<Arc<str>> {
        self.control
            .ring()
            .frames
            .iter()
            .map(|frame| Arc::clone(&frame.json))
            .collect()
    }

    fn bus(&self, topic: Topic) -> &TopicBus {
        match topic {
            Topic::Requests => &self.requests,
            _ => &self.control,
        }
    }

    /// Emits `payload` on its own topic and returns the assigned sequence.
    /// Unknown payloads are not emitted.
    pub(crate) fn emit(&self, payload: EventPayload) -> Option<u64> {
        let topic = payload.topic()?;
        let bus = self.bus(topic);
        let closing = matches!(
            payload,
            EventPayload::TokenRotated(_) | EventPayload::ShutdownStarted(_)
        );
        let event = payload.name();
        let mut ring = bus.ring();
        let seq = ring.last + 1;
        let json = self.encode(topic, seq, payload);
        let frame = Arc::new(Frame {
            topic,
            seq,
            event,
            json,
            closing,
        });
        ring.push(Arc::clone(&frame));
        // Sending under the ring lock keeps broadcast order equal to seq order.
        let _ = bus.sender.send(frame);
        Some(seq)
    }

    fn encode(&self, topic: Topic, seq: u64, payload: EventPayload) -> Arc<str> {
        let mut envelope = EventEnvelope::default();
        envelope.boot_id.clone_from(&self.boot_id);
        envelope.topic = topic;
        envelope.seq = seq;
        envelope.time_ms = super::control::unix_milliseconds();
        envelope.payload = payload;
        serde_json::to_string(&envelope)
            .unwrap_or_else(|_| "{}".to_owned())
            .into()
    }

    /// Opens a subscription following the subscribe-before-snapshot order.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        query: &EventsQuery,
        snapshot: SnapshotSource,
    ) -> EventSubscription {
        self.open(query, snapshot, || {})
    }

    /// `after_receivers` runs between taking the receivers and reading the
    /// rings, which is where a concurrent emit lands in both.
    fn open(
        self: &Arc<Self>,
        query: &EventsQuery,
        snapshot: SnapshotSource,
        after_receivers: impl FnOnce(),
    ) -> EventSubscription {
        let mut topics: Vec<Topic> = query.topics.clone();
        if topics.is_empty() {
            topics.push(Topic::Control);
        }
        let mut states: Vec<TopicState> = topics
            .iter()
            .map(|topic| TopicState {
                topic: *topic,
                receiver: self.bus(*topic).sender.subscribe(),
                floor: 0,
                delivered: 0,
            })
            .collect();
        after_receivers();
        let plan = resume_plan(query.last_event_id.as_deref(), &self.boot_id, &topics);
        let mut replay = Vec::new();
        let mut resyncs = Vec::new();
        let mut fresh = false;
        for (topic, resume) in plan {
            let Some(state) = states.iter_mut().find(|state| state.topic == topic) else {
                continue;
            };
            let ring = self.bus(topic).ring();
            state.floor = ring.last;
            state.delivered = ring.last;
            match resume {
                Resume::After(cursor) => match ring.after(cursor) {
                    Some(frames) => {
                        state.delivered = cursor;
                        replay.extend(frames);
                    }
                    None => resyncs.push((topic, ResyncReason::Unreplayable)),
                },
                Resume::Resync(reason) => resyncs.push((topic, reason)),
                _ => fresh = true,
            }
        }
        // Until its snapshot is delivered, a resyncing topic must not appear
        // covered in emitted ids, so a reconnect in between resyncs again.
        for state in &mut states {
            if resyncs.iter().any(|(topic, _)| *topic == state.topic) {
                state.delivered = 0;
            }
        }
        let mut subscription = Subscription {
            bus: Arc::clone(self),
            topics: states,
            snapshot,
            pending: VecDeque::new(),
            done: false,
        };
        // Replays come first: the snapshot that follows any resync covers
        // every topic up to its floor, so nothing it covers is sent after it.
        for frame in replay {
            subscription.push_shared(&frame);
        }
        for (topic, reason) in &resyncs {
            subscription.push_resync(*topic, *reason);
        }
        let draining = if fresh || !resyncs.is_empty() {
            subscription.push_snapshot()
        } else {
            (subscription.snapshot)().shutdown == ShutdownState::Draining
        };
        // A subscriber arriving during drain gets its state and then the end.
        subscription.done = draining;
        let frames = futures_util::stream::unfold(subscription, |mut subscription| async move {
            subscription.next().await.map(|frame| (frame, subscription))
        });
        EventSubscription::new(frames.boxed())
    }
}

struct TopicState {
    topic: Topic,
    receiver: broadcast::Receiver<Arc<Frame>>,
    /// Live frames at or below this sequence are already covered.
    floor: u64,
    /// Last sequence reflected in emitted ids.
    delivered: u64,
}

struct Subscription {
    bus: Arc<EventBus>,
    topics: Vec<TopicState>,
    snapshot: SnapshotSource,
    pending: VecDeque<SseFrame>,
    done: bool,
}

impl Subscription {
    fn cursors(&self, field: fn(&TopicState) -> u64) -> Vec<EventCursor> {
        self.topics
            .iter()
            .map(|state| EventCursor::new(state.topic, field(state)))
            .collect()
    }

    fn id(&self) -> String {
        EventId::new(
            self.bus.boot_id.clone(),
            self.cursors(|state| state.delivered),
        )
        .map(|id| id.to_string())
        .unwrap_or_default()
    }

    fn push_local(&mut self, payload: EventPayload) {
        let event = payload.name();
        let seq = self.bus.control.last();
        let data = self.bus.encode(Topic::Control, seq, payload);
        let id = self.id();
        self.pending.push_back(SseFrame::new(id, event, data));
    }

    fn push_resync(&mut self, topic: Topic, reason: ResyncReason) {
        let mut event = ResyncRequiredEvent::default();
        event.topic = topic;
        event.reason = reason;
        event.cursors = self.cursors(|state| state.floor);
        self.push_local(EventPayload::ResyncRequired(event));
    }

    /// Queues a fresh snapshot and reports whether the daemon is draining.
    fn push_snapshot(&mut self) -> bool {
        let mut snapshot = (self.snapshot)();
        snapshot.cursors = self.cursors(|state| state.floor);
        let draining = snapshot.shutdown == ShutdownState::Draining;
        for state in &mut self.topics {
            state.delivered = state.floor;
        }
        self.push_local(EventPayload::Snapshot(snapshot));
        draining
    }

    fn push_shared(&mut self, frame: &Frame) {
        if let Some(state) = self
            .topics
            .iter_mut()
            .find(|state| state.topic == frame.topic)
        {
            state.delivered = frame.seq;
        }
        let id = self.id();
        self.pending
            .push_back(SseFrame::new(id, frame.event, Arc::clone(&frame.json)));
    }

    async fn next(&mut self) -> Option<SseFrame> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
            if self.done {
                return None;
            }
            let (index, received) = receive(&mut self.topics).await?;
            match received {
                Ok(frame) => {
                    let state = &mut self.topics[index];
                    if frame.seq <= state.floor {
                        continue;
                    }
                    state.floor = frame.seq;
                    self.push_shared(&frame);
                    // Replayed closing frames are history; a live one ends the
                    // stream after it is delivered.
                    self.done = frame.closing;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let topic = self.topics[index].topic;
                    let last = self.bus.bus(topic).last();
                    let state = &mut self.topics[index];
                    state.floor = last;
                    state.delivered = last;
                    self.push_resync(topic, ResyncReason::Lagged);
                    self.push_snapshot();
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

type Received = (usize, Result<Arc<Frame>, broadcast::error::RecvError>);

/// Receives from whichever topic is ready, preferring control.
async fn receive(topics: &mut [TopicState]) -> Option<Received> {
    match topics {
        [only] => Some((0, only.receiver.recv().await)),
        [first, second] => Some(tokio::select! {
            biased;
            received = first.receiver.recv() => (0, received),
            received = second.receiver.recv() => (1, received),
        }),
        _ => None,
    }
}

/// Emits `token_rotated` on every generation advance and `shutdown_started`
/// once when draining begins. Ends when both sources are gone.
pub(crate) fn spawn_lifecycle(
    bus: Arc<EventBus>,
    mut generation: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    // Mark the current values seen before spawning, so a change made right
    // after this call is observed rather than absorbed as the baseline.
    generation.borrow_and_update();
    let already_draining = *shutdown.borrow_and_update();
    tokio::spawn(async move {
        let mut watching_generation = true;
        let mut watching_shutdown = !already_draining;
        if already_draining {
            bus.emit(EventPayload::ShutdownStarted(draining_event()));
        }
        while watching_generation || watching_shutdown {
            tokio::select! {
                changed = generation.changed(), if watching_generation => {
                    if changed.is_ok() {
                        bus.emit(EventPayload::TokenRotated(TokenRotatedEvent::default()));
                    } else {
                        watching_generation = false;
                    }
                }
                changed = shutdown.changed(), if watching_shutdown => {
                    if changed.is_err() {
                        watching_shutdown = false;
                    } else if *shutdown.borrow_and_update() {
                        bus.emit(EventPayload::ShutdownStarted(draining_event()));
                        watching_shutdown = false;
                    }
                }
            }
        }
    })
}

fn draining_event() -> ShutdownStartedEvent {
    let mut event = ShutdownStartedEvent::default();
    event.draining = true;
    event
}

/// 128-bit random hex boot id from two independently seeded std hashers.
fn new_boot_id() -> String {
    let seed = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", seed(1), seed(2))
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;
    use hofund_control_proto::{
        EventEnvelope, MemberCooledEvent, RequestStartedEvent, ShutdownState, decode_response,
    };

    use super::*;

    const BOOT: &str = "00112233445566778899aabbccddeeff";

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::with_boot_id(BOOT.to_owned()))
    }

    fn snapshot() -> SnapshotSource {
        Arc::new(SnapshotEvent::default)
    }

    fn draining_snapshot() -> SnapshotSource {
        Arc::new(|| {
            let mut snapshot = SnapshotEvent::default();
            snapshot.shutdown = ShutdownState::Draining;
            snapshot
        })
    }

    fn cooled(member: &str) -> EventPayload {
        let mut event = MemberCooledEvent::default();
        event.member_id = member.to_owned();
        EventPayload::MemberCooled(event)
    }

    fn started(request: &str) -> EventPayload {
        let mut event = RequestStartedEvent::default();
        event.request_id = request.to_owned();
        EventPayload::RequestStarted(event)
    }

    fn query(topics: &[Topic], last: Option<&str>) -> EventsQuery {
        EventsQuery::new(topics, last.map(str::to_owned))
    }

    /// Frames that are ready now, and whether the stream has ended.
    fn ready(subscription: &mut EventSubscription) -> (Vec<SseFrame>, bool) {
        let mut frames = Vec::new();
        loop {
            match subscription.frames.next().now_or_never() {
                Some(Some(frame)) => frames.push(frame),
                Some(None) => return (frames, true),
                None => return (frames, false),
            }
        }
    }

    fn envelope(frame: &SseFrame) -> EventEnvelope {
        decode_response::<EventEnvelope>(hofund_control_proto::API_VERSION, frame.data.as_bytes())
            .expect("frame data is an envelope")
    }

    fn names(frames: &[SseFrame]) -> Vec<&str> {
        frames.iter().map(|frame| frame.event.as_str()).collect()
    }

    fn seqs(frames: &[SseFrame], topic: Topic) -> Vec<u64> {
        frames
            .iter()
            .map(envelope)
            .filter(|envelope| envelope.topic == topic && envelope.payload.topic() == Some(topic))
            .filter(|envelope| {
                !matches!(
                    envelope.payload,
                    EventPayload::Snapshot(_) | EventPayload::ResyncRequired(_)
                )
            })
            .map(|envelope| envelope.seq)
            .collect()
    }

    fn resyncs(frames: &[SseFrame]) -> Vec<(Topic, ResyncReason)> {
        frames
            .iter()
            .filter_map(|frame| match envelope(frame).payload {
                EventPayload::ResyncRequired(event) => Some((event.topic, event.reason)),
                _ => None,
            })
            .collect()
    }

    fn frame(seq: u64, bytes: usize) -> Arc<Frame> {
        Arc::new(Frame {
            topic: Topic::Requests,
            seq,
            event: "request_started",
            json: "x".repeat(bytes).into(),
            closing: false,
        })
    }

    #[test]
    fn ring_bounds_4096_events_and_4mib() {
        let mut ring = Ring::default();
        for seq in 1..=4097 {
            ring.push(frame(seq, 1));
        }
        assert_eq!(ring.frames.len(), RING_EVENTS);
        assert_eq!(ring.frames.front().map(|frame| frame.seq), Some(2));
        assert!(ring.after(0).is_none(), "evicted range must not replay");
        assert_eq!(ring.after(1).map(|frames| frames.len()), Some(4096));

        let mut ring = Ring::default();
        let quarter = RING_BYTES / 4;
        for seq in 1..=4 {
            ring.push(frame(seq, quarter));
        }
        assert_eq!(ring.bytes, RING_BYTES, "exactly 4 MiB is retained");
        assert_eq!(ring.frames.len(), 4);
        ring.push(frame(5, 1));
        assert_eq!(ring.frames.len(), 4);
        assert_eq!(ring.frames.front().map(|frame| frame.seq), Some(2));
        assert_eq!(ring.bytes, RING_BYTES - quarter + 1);
    }

    #[test]
    fn events_subscribe_before_snapshot_suppresses_duplicates() {
        let bus = bus();
        bus.emit(cooled("a"));
        let racing = Arc::clone(&bus);
        // The racing emit lands on the receiver and inside the snapshot range.
        let mut subscription = bus.open(&query(&[Topic::Control], None), snapshot(), move || {
            racing.emit(cooled("b"));
        });
        bus.emit(cooled("c"));
        let (frames, ended) = ready(&mut subscription);
        assert!(!ended);
        assert_eq!(names(&frames), ["snapshot", "member_cooled"]);
        match envelope(&frames[0]).payload {
            EventPayload::Snapshot(snapshot) => {
                assert_eq!(snapshot.cursors, [EventCursor::new(Topic::Control, 2)]);
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert_eq!(seqs(&frames, Topic::Control), [3]);
        assert_eq!(frames[1].id, format!("{BOOT}:control=3"));
    }

    #[test]
    fn events_last_event_id_replays_bounded_range() {
        let bus = bus();
        for member in ["a", "b", "c", "d", "e"] {
            bus.emit(cooled(member));
        }
        let last = format!("{BOOT}:control=2");
        let mut subscription = bus.subscribe(&query(&[Topic::Control], Some(&last)), snapshot());
        let (frames, _) = ready(&mut subscription);
        assert_eq!(names(&frames), ["member_cooled"; 3]);
        assert_eq!(seqs(&frames, Topic::Control), [3, 4, 5]);
        assert_eq!(frames[0].id, format!("{BOOT}:control=3"));
        assert_eq!(frames[2].id, format!("{BOOT}:control=5"));

        let ahead = format!("{BOOT}:control=9");
        let mut subscription = bus.subscribe(&query(&[Topic::Control], Some(&ahead)), snapshot());
        let (frames, _) = ready(&mut subscription);
        assert_eq!(
            resyncs(&frames),
            [(Topic::Control, ResyncReason::Unreplayable)]
        );
        assert_eq!(names(&frames), ["resync_required", "snapshot"]);

        let bus = self::bus();
        for seq in 0..=RING_EVENTS {
            bus.emit(cooled(&seq.to_string()));
        }
        let evicted = format!("{BOOT}:control=0");
        let mut subscription = bus.subscribe(&query(&[Topic::Control], Some(&evicted)), snapshot());
        let (frames, _) = ready(&mut subscription);
        assert_eq!(
            resyncs(&frames),
            [(Topic::Control, ResyncReason::Unreplayable)]
        );
    }

    #[test]
    fn events_unknown_boot_id_resyncs_all_topics() {
        let bus = bus();
        bus.emit(cooled("a"));
        bus.emit(started("r"));
        let both = [Topic::Control, Topic::Requests];
        let mut subscription = bus.subscribe(
            &query(&both, Some("another-boot:control=1,requests=1")),
            snapshot(),
        );
        let (frames, _) = ready(&mut subscription);
        assert_eq!(
            resyncs(&frames),
            [
                (Topic::Control, ResyncReason::UnknownBoot),
                (Topic::Requests, ResyncReason::UnknownBoot)
            ]
        );
        assert_eq!(names(&frames).last(), Some(&"snapshot"));

        let mut subscription = bus.subscribe(&query(&both, Some("not an id")), snapshot());
        let (frames, _) = ready(&mut subscription);
        assert_eq!(
            resyncs(&frames),
            [
                (Topic::Control, ResyncReason::MalformedId),
                (Topic::Requests, ResyncReason::MalformedId)
            ]
        );
    }

    #[test]
    fn events_missing_topic_resyncs_only_that_topic() {
        let bus = bus();
        bus.emit(cooled("a"));
        bus.emit(cooled("b"));
        bus.emit(started("r"));
        let last = format!("{BOOT}:control=1");
        let mut subscription = bus.subscribe(
            &query(&[Topic::Control, Topic::Requests], Some(&last)),
            snapshot(),
        );
        let (frames, _) = ready(&mut subscription);
        assert_eq!(
            resyncs(&frames),
            [(Topic::Requests, ResyncReason::MissingTopic)]
        );
        assert_eq!(
            names(&frames),
            ["member_cooled", "resync_required", "snapshot"]
        );
        assert_eq!(seqs(&frames, Topic::Control), [2]);
        // Until the snapshot arrives, ids do not claim the resyncing topic.
        assert_eq!(frames[0].id, format!("{BOOT}:control=2,requests=0"));
        assert_eq!(frames[1].id, format!("{BOOT}:control=2,requests=0"));
        assert_eq!(frames[2].id, format!("{BOOT}:control=2,requests=1"));
        let EventPayload::Snapshot(snapshot) = envelope(&frames[2]).payload else {
            panic!("snapshot");
        };
        assert_eq!(
            snapshot.cursors,
            [
                EventCursor::new(Topic::Control, 2),
                EventCursor::new(Topic::Requests, 1)
            ]
        );
    }

    #[test]
    fn eventsource_reconnect_with_composite_id_resumes_all_topics() {
        let bus = bus();
        let both = [Topic::Control, Topic::Requests];
        let mut subscription = bus.subscribe(&query(&both, None), snapshot());
        bus.emit(cooled("a"));
        bus.emit(started("r1"));
        bus.emit(started("r2"));
        let (frames, _) = ready(&mut subscription);
        assert_eq!(names(&frames)[0], "snapshot");
        let last = frames.last().expect("frames").id.clone();
        assert_eq!(last, format!("{BOOT}:control=1,requests=2"));
        drop(subscription);

        bus.emit(started("r3"));
        bus.emit(cooled("b"));
        let mut subscription = bus.subscribe(&query(&both, Some(&last)), snapshot());
        let (frames, _) = ready(&mut subscription);
        assert!(resyncs(&frames).is_empty());
        assert!(!names(&frames).contains(&"snapshot"));
        assert_eq!(seqs(&frames, Topic::Control), [2]);
        assert_eq!(seqs(&frames, Topic::Requests), [3]);
        assert_eq!(
            frames.last().expect("frames").id,
            format!("{BOOT}:control=2,requests=3")
        );
    }

    #[test]
    fn events_lagged_topic_resyncs_only_that_topic() {
        let bus = bus();
        let both = [Topic::Control, Topic::Requests];
        let mut subscription = bus.subscribe(&query(&both, None), snapshot());
        let _ = ready(&mut subscription);
        for index in 0..=TOPIC_CAPACITY {
            bus.emit(started(&index.to_string()));
        }
        bus.emit(cooled("a"));
        let (frames, _) = ready(&mut subscription);
        assert_eq!(resyncs(&frames), [(Topic::Requests, ResyncReason::Lagged)]);
        let names = names(&frames);
        let resync = names
            .iter()
            .position(|name| *name == "resync_required")
            .expect("resync frame");
        assert_eq!(
            names.get(resync + 1),
            Some(&"snapshot"),
            "a lag resync is followed by a fresh snapshot"
        );
        assert_eq!(seqs(&frames, Topic::Control), [1]);
        assert!(
            seqs(&frames, Topic::Requests).is_empty(),
            "frames covered by the resync snapshot are suppressed"
        );
        bus.emit(started("after"));
        let (frames, _) = ready(&mut subscription);
        let last = u64::try_from(TOPIC_CAPACITY).expect("capacity") + 2;
        assert_eq!(seqs(&frames, Topic::Requests), [last]);
    }

    #[test]
    fn control_topic_not_lagged_or_evicted_by_request_load() {
        let bus = bus();
        bus.emit(cooled("a"));
        let both = [Topic::Control, Topic::Requests];
        let mut subscription = bus.subscribe(&query(&both, None), snapshot());
        let _ = ready(&mut subscription);
        for index in 0..(2 * TOPIC_CAPACITY) {
            bus.emit(started(&index.to_string()));
        }
        assert_eq!(bus.control.ring().frames.len(), 1);
        let last = format!("{BOOT}:control=0");
        let mut replay = bus.subscribe(&query(&[Topic::Control], Some(&last)), snapshot());
        let (frames, _) = ready(&mut replay);
        assert_eq!(seqs(&frames, Topic::Control), [1]);
        let (frames, _) = ready(&mut subscription);
        assert!(
            resyncs(&frames)
                .iter()
                .all(|(topic, _)| *topic == Topic::Requests)
        );
    }

    #[test]
    fn request_event_load_100rps_slow_client_respects_per_topic_4096_events_and_4mib() {
        let bus = bus();
        let _slow = bus.subscribe(&query(&[Topic::Requests], None), snapshot());
        let large = "r".repeat(4096);
        // Emits never wait for the unread subscriber.
        for _ in 0..3000 {
            bus.emit(started(&large));
        }
        let ring = bus.requests.ring();
        assert!(ring.bytes <= RING_BYTES);
        assert!(ring.frames.len() < 3000, "byte bound evicted before count");
        assert_eq!(ring.last, 3000);
    }

    #[test]
    fn request_topic_is_opt_in() {
        let bus = bus();
        let mut subscription = bus.subscribe(&query(&[], None), snapshot());
        bus.emit(started("r"));
        bus.emit(cooled("a"));
        let (frames, _) = ready(&mut subscription);
        assert_eq!(names(&frames), ["snapshot", "member_cooled"]);
        assert_eq!(frames[1].id, format!("{BOOT}:control=1"));
    }

    #[tokio::test]
    async fn sse_closes_with_token_rotated_when_generation_advances() {
        let bus = bus();
        let (generation, generation_rx) = watch::channel(0_u64);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = spawn_lifecycle(Arc::clone(&bus), generation_rx, shutdown_rx);
        let mut subscription = bus.subscribe(&query(&[Topic::Control], None), snapshot());
        generation.send_replace(1);
        let mut names = Vec::new();
        while let Some(frame) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            subscription.frames.next(),
        )
        .await
        .expect("stream ends after token_rotated")
        {
            names.push(frame.event);
        }
        assert_eq!(names, ["snapshot", "token_rotated"]);

        // A replayed rotation is history and does not close the reconnect.
        let last = format!("{BOOT}:control=0");
        let mut replay = bus.subscribe(&query(&[Topic::Control], Some(&last)), snapshot());
        let (frames, ended) = ready(&mut replay);
        assert_eq!(super::tests::names(&frames), ["token_rotated"]);
        assert!(!ended);

        shutdown.send_replace(true);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), replay.frames.next())
            .await
            .expect("shutdown frame")
            .expect("frame");
        assert_eq!(frame.event, "shutdown_started");
        assert!(replay.frames.next().await.is_none());
        drop((generation, shutdown));
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("lifecycle task ends with its sources")
            .expect("task");
    }

    #[test]
    fn subscribe_while_draining_sends_state_then_ends() {
        let bus = bus();
        let mut subscription = bus.subscribe(&query(&[Topic::Control], None), draining_snapshot());
        let (frames, ended) = ready(&mut subscription);
        assert_eq!(names(&frames), ["snapshot"]);
        assert!(ended);
    }

    #[test]
    fn boot_ids_are_random_hex_and_valid_event_ids() {
        let first = EventBus::new();
        let second = EventBus::new();
        assert_eq!(first.boot_id().len(), 32);
        assert!(first.boot_id().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first.boot_id(), second.boot_id());
        EventId::new(first.boot_id(), vec![EventCursor::new(Topic::Control, 0)])
            .expect("boot id fits the event id grammar");
    }
}
