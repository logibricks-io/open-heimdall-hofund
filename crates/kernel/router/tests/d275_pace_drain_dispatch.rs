//! Pace-drain dispatch state transitions (D-275).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::strategy::{Measurements, Needs, Seat};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router,
};

struct Fake {
    id: MemberId,
    name: &'static str,
    learns: AtomicBool,
    seen: Arc<Mutex<Vec<&'static str>>>,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.name
    }

    fn protocol(&self) -> &str {
        "test"
    }

    fn learns_allowance(&self) -> bool {
        self.learns.load(Ordering::Relaxed)
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(self.name);
        self.learns.store(false, Ordering::Relaxed);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

#[derive(Clone, Copy)]
struct Reading {
    known: bool,
    used: f64,
    pace: f64,
}

impl Reading {
    const fn known(used: f64, pace: f64) -> Self {
        Self {
            known: true,
            used,
            pace,
        }
    }

    const fn unknown() -> Self {
        Self {
            known: false,
            used: 0.0,
            pace: 0.0,
        }
    }
}

struct Readings(Mutex<Vec<Reading>>);

impl Readings {
    fn set(&self, index: usize, reading: Reading) {
        self.0.lock().expect("readings")[index] = reading;
    }
}

impl Measurements for Readings {
    fn weigh(
        &self,
        seats: &mut [Seat],
        _wire_models: &[String],
        _usage_model: &str,
        _needs: Needs,
        _now: SystemTime,
    ) {
        let readings = self.0.lock().expect("readings");
        assert_eq!(seats.len(), readings.len());
        for (seat, reading) in seats.iter_mut().zip(readings.iter()) {
            seat.known = reading.known;
            seat.used = reading.used;
            seat.pace = reading.pace;
        }
    }
}

struct Fixture {
    router: Router,
    readings: Arc<Readings>,
    cooldowns: Arc<Cooldowns>,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

fn ok() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn quota() -> PluginError {
    PluginError::quota_status(429, "quota exhausted")
}

fn fake(
    name: &'static str,
    learns: bool,
    answers: Vec<Result<Completion, PluginError>>,
    seen: &Arc<Mutex<Vec<&'static str>>>,
) -> Arc<Fake> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::from_static("fixture"),
            MemberKey::from_static(name),
        ),
        name,
        learns: AtomicBool::new(learns),
        seen: Arc::clone(seen),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn fixture(routing: &str, members: Vec<Arc<Fake>>, readings: Vec<Reading>) -> Fixture {
    let seen = Arc::clone(&members.first().expect("a member").seen);
    let readings = Arc::new(Readings(Mutex::new(readings)));
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let hops = members.into_iter().map(|member| Hop::new(member)).collect();
    let router = Router::from_routes(
        BTreeMap::from([(
            "demo".to_owned(),
            Route::new(hops).with_routing(routing).with_patience(None),
        )]),
        FailoverPolicy::default(),
    )
    .with_measurements(readings.clone())
    .with_cooldowns(cooldowns.clone());
    Fixture {
        router,
        readings,
        cooldowns,
        seen,
    }
}

async fn dispatch(fixture: &Fixture) {
    fixture
        .router
        .dispatch(&Request::new("demo"), "test")
        .await
        .expect("dispatch");
}

fn take_seen(fixture: &Fixture) -> Vec<&'static str> {
    std::mem::take(&mut *fixture.seen.lock().expect("seen"))
}

#[tokio::test(start_paused = true)]
async fn d275_dispatch_learner_answer_does_not_replace_the_kept_pace_seat() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let members = vec![
        fake("kept", false, vec![ok(), ok()], &seen),
        fake("other", false, vec![ok(), ok()], &seen),
        fake("learner", true, vec![ok(), ok()], &seen),
    ];
    let fixture = fixture(
        "pace-drain",
        members,
        vec![
            Reading::known(20.0, 2.0),
            Reading::known(10.0, 1.0),
            Reading::unknown(),
        ],
    );

    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["learner"]);
    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["kept"]);
}

#[tokio::test(start_paused = true)]
async fn d275_dispatch_member_quota_refusal_moves_on_after_renewal() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let members = vec![
        fake("refused", false, vec![Err(quota()), ok()], &seen),
        fake("next", false, vec![ok(), ok()], &seen),
    ];
    let fixture = fixture(
        "pace-drain",
        members,
        vec![Reading::known(80.0, 9.0), Reading::known(20.0, 2.0)],
    );

    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["refused", "next"]);

    let refused = &fixture.cooldowns;
    let id = MemberId::from_preset(
        PresetId::from_static("fixture"),
        MemberKey::from_static("refused"),
    );
    refused
        .record_failure_for_model(&id, "demo", &quota())
        .expect("quota rest");
    refused.record_success(&id);
    assert!(refused.cooling_for_model(&id, "demo").is_none());
    fixture.readings.set(0, Reading::known(0.0, 12.0));

    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["next"]);
}

#[tokio::test(start_paused = true)]
async fn d275_dispatch_model_quota_refusal_does_not_mark_the_pool_past() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let members = vec![
        fake(
            "kept",
            false,
            vec![Err(quota().with_model_scope()), ok()],
            &seen,
        ),
        fake("other", false, vec![ok(), ok()], &seen),
    ];
    let fixture = fixture(
        "pace-drain",
        members,
        vec![Reading::known(80.0, 9.0), Reading::known(20.0, 2.0)],
    );

    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["kept", "other"]);

    fixture.readings.set(0, Reading::known(0.0, 9.0));
    fixture.readings.set(1, Reading::known(20.0, 20.0));
    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["kept"]);
}

#[tokio::test(start_paused = true)]
async fn d275_dispatch_drain_sticks_only_to_the_member_that_answered() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let members = vec![
        fake("refused", false, vec![Err(quota()), ok()], &seen),
        fake("answerer", false, vec![ok(), ok()], &seen),
    ];
    let fixture = fixture(
        "drain",
        members,
        vec![Reading::known(80.0, 9.0), Reading::known(20.0, 2.0)],
    );

    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["refused", "answerer"]);
    dispatch(&fixture).await;
    assert_eq!(take_seen(&fixture), ["answerer"]);
}
