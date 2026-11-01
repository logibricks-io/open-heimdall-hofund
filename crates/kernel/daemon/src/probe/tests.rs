use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_plugin_api::{
    AccountId, Completion, MemberId, MemberKey, PluginError, PluginId, PresetId, Request,
};
use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns, Member, ModelListAnswer};

use super::ProbeRunner;

struct Fake {
    id: MemberId,
    route: String,
    plugin: String,
    hits: Arc<AtomicUsize>,
    answers: Mutex<VecDeque<Result<ModelListAnswer, PluginError>>>,
    pending: bool,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Fake {
    fn new(id: MemberId, answers: Vec<Result<ModelListAnswer, PluginError>>) -> Self {
        Self {
            route: id.to_string(),
            id,
            plugin: String::new(),
            hits: Arc::new(AtomicUsize::new(0)),
            answers: Mutex::new(answers.into_iter().collect()),
            pending: false,
            gate: None,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        &self.route
    }

    fn plugin_id(&self) -> &str {
        &self.plugin
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        Err(PluginError::internal("not a chat member"))
    }

    async fn list_models(&self) -> Result<ModelListAnswer, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if self.pending {
            std::future::pending().await
        }
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .unwrap_or_else(|| Err(PluginError::connect("still down")))
    }
}

fn id(name: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(name), MemberKey::from_static("row"))
}

fn make_book(base: Duration) -> Arc<Cooldowns> {
    let mut policy = CooldownPolicy::default();
    policy.base = base;
    policy.generic_max = base;
    Arc::new(Cooldowns::with_clock(
        policy,
        Arc::new(|| tokio::time::Instant::now().into_std()),
    ))
}

fn answer() -> Result<ModelListAnswer, PluginError> {
    Ok(ModelListAnswer)
}

fn failure() -> Result<ModelListAnswer, PluginError> {
    Err(PluginError::connect("still down"))
}

fn members(fake: &Arc<Fake>) -> Vec<Arc<dyn Member>> {
    vec![fake.clone() as Arc<dyn Member>]
}

#[tokio::test(start_paused = true)]
async fn b7a_generic_rests_are_probed_on_the_backoff_schedule() {
    let book = make_book(Duration::from_secs(30 * 60));
    let fake = Arc::new(Fake::new(
        id("schedule"),
        vec![failure(), failure(), failure()],
    ));
    book.record_failure(fake.id(), &PluginError::status_error(503, "down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(fake.hits(), 0);

    for (seconds, expected) in [(60, 1), (120, 2), (300, 3), (600, 4)] {
        tokio::time::advance(Duration::from_secs(seconds - 1)).await;
        runner.run_once(&book, &members(&fake)).await;
        assert_eq!(fake.hits(), expected - 1, "before {seconds} seconds");
        tokio::time::advance(Duration::from_secs(1)).await;
        runner.run_once(&book, &members(&fake)).await;
        assert_eq!(fake.hits(), expected, "after {seconds} seconds");
    }

    // The schedule stops once the next probe would land within 30 seconds
    // of the rest's end.
    let book = make_book(Duration::from_secs(80));
    let fake = Arc::new(Fake::new(id("short"), vec![answer()]));
    book.record_failure(fake.id(), &PluginError::status_error(503, "down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(
        fake.hits(),
        0,
        "a probe 20 seconds before the end is skipped"
    );

    for error in [
        PluginError::status_error(429, "rate"),
        PluginError::quota_status(429, "quota"),
        PluginError::credit_status(402, "credit"),
        PluginError::status_error(401, "auth"),
        PluginError::status_error(503, "hinted").with_retry_after(Duration::from_secs(60)),
        PluginError::decode("bad reply"),
    ] {
        let book = make_book(Duration::from_secs(2 * 60));
        let fake = Arc::new(Fake::new(id("excluded"), vec![answer()]));
        book.record_failure(fake.id(), &error);
        let mut runner = ProbeRunner::default();
        runner.run_once(&book, &members(&fake)).await;
        tokio::time::advance(Duration::from_secs(60)).await;
        runner.run_once(&book, &members(&fake)).await;
        assert_eq!(fake.hits(), 0, "{error}");
    }

    let book = make_book(Duration::from_secs(2 * 60));
    let model = Arc::new(Fake::new(id("model-rest"), vec![answer()]));
    book.record_failure_for_model(
        model.id(),
        "demo",
        &PluginError::status_error(400, "model is not supported"),
    );
    let subscription = Arc::new(Fake::new(
        MemberId::from_parts(
            PluginId::from_static("account"),
            AccountId::from_static("one"),
        ),
        vec![answer()],
    ));
    book.record_failure(subscription.id(), &PluginError::status_error(503, "down"));
    let plugin = Arc::new(Fake::new(id("plugin-rest"), vec![answer()]));
    let mut plugin = plugin;
    Arc::get_mut(&mut plugin).expect("unique fake").plugin = "plugin".to_owned();
    book.record_failure(plugin.id(), &PluginError::status_error(503, "down"));
    let all = vec![
        model.clone() as Arc<dyn Member>,
        subscription.clone() as Arc<dyn Member>,
        plugin.clone() as Arc<dyn Member>,
    ];
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &all).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &all).await;
    for fake in [&model, &subscription, &plugin] {
        assert_eq!(fake.hits(), 0, "{}", fake.id());
    }
}

#[tokio::test(start_paused = true)]
async fn b7b_model_list_answer_lifts_matching_rest() {
    let book = make_book(Duration::from_secs(2 * 60));
    let fake = Arc::new(Fake::new(id("lift"), vec![answer()]));
    book.record_failure(fake.id(), &PluginError::connect("down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;

    assert_eq!(fake.hits(), 1);
    assert!(book.cooling(fake.id()).is_none());
    let recoveries = book.drain_recoveries();
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].0, *fake.id());
    assert_eq!(recoveries[0].1, CooldownCause::Failure);

    // An answer for an older rest does not lift the newer one.
    let book = make_book(Duration::from_secs(5 * 60));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut held = Fake::new(id("renewed"), vec![answer()]);
    held.gate = Some(Arc::clone(&gate));
    let held = Arc::new(held);
    book.record_failure(held.id(), &PluginError::connect("down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&held)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&held)).await;
    assert_eq!(held.hits(), 1);
    let first = book.cooling(held.id()).expect("first rest").until();
    tokio::time::advance(Duration::from_secs(10)).await;
    book.record_failure(held.id(), &PluginError::connect("down again"));
    let renewed = book.cooling(held.id()).expect("renewed rest").until();
    assert_ne!(first, renewed);
    gate.notify_one();
    tokio::task::yield_now().await;
    runner.run_once(&book, &members(&held)).await;
    assert_eq!(
        book.cooling(held.id()).map(|cooling| cooling.until()),
        Some(renewed),
        "the stale answer keeps the renewed rest"
    );
    assert!(!book.lift_probe(held.id(), first));

    // A rest a probe may not lift stays, even at its exact deadline.
    let book = make_book(Duration::from_secs(2 * 60));
    let limited = id("limited");
    book.record_failure(&limited, &PluginError::status_error(429, "rate"));
    let cooling = book.cooling(&limited).expect("rate rest");
    assert!(!cooling.background_probe());
    assert!(!book.lift_probe(&limited, cooling.until()));
    assert!(book.cooling(&limited).is_some());
}

#[tokio::test(start_paused = true)]
async fn b7c_failure_after_lift_suppresses_probe_until_real_success() {
    let book = make_book(Duration::from_secs(2 * 60));
    let fake = Arc::new(Fake::new(id("miss"), vec![answer(), answer()]));
    book.record_failure(fake.id(), &PluginError::status_error(503, "down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(fake.hits(), 1);

    book.record_failure(fake.id(), &PluginError::status_error(503, "still down"));
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(fake.hits(), 1);

    book.record_success(fake.id());
    book.record_failure(fake.id(), &PluginError::status_error(503, "new outage"));
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(fake.hits(), 2);
}

#[tokio::test(start_paused = true)]
async fn b7d_probe_loop_is_runtime_scoped_and_stops_on_shutdown() {
    let book = make_book(Duration::from_secs(2 * 60));
    let fake = Arc::new(Fake::new(id("lifecycle"), Vec::new()));
    book.record_failure(fake.id(), &PluginError::connect("down"));
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    assert_eq!(fake.hits(), 0, "no runtime loop means no probe");

    book.record_failure(fake.id(), &PluginError::connect("down again"));
    let pending = Arc::new(Fake {
        id: fake.id.clone(),
        route: fake.route.clone(),
        plugin: String::new(),
        hits: Arc::clone(&fake.hits),
        answers: Mutex::new(VecDeque::new()),
        pending: true,
        gate: None,
    });
    let pending_member = pending.clone() as Arc<dyn Member>;
    let task_book = Arc::clone(&book);
    let task = super::spawn(task_book, move || vec![pending_member.clone()]);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(fake.hits(), 1);
    task.abort();
    tokio::time::advance(Duration::from_secs(10 * 60)).await;
    tokio::task::yield_now().await;
    assert_eq!(fake.hits(), 1);
}

#[tokio::test(start_paused = true)]
async fn b7e_slow_probe_does_not_delay_another_keys_probe() {
    let book = make_book(Duration::from_secs(2 * 60));
    let mut slow = Arc::new(Fake::new(id("slow"), Vec::new()));
    Arc::get_mut(&mut slow).expect("unique fake").pending = true;
    let quick = Arc::new(Fake::new(id("quick"), vec![answer()]));
    book.record_failure(slow.id(), &PluginError::connect("down"));
    book.record_failure(quick.id(), &PluginError::connect("down"));
    let members = vec![
        slow.clone() as Arc<dyn Member>,
        quick.clone() as Arc<dyn Member>,
    ];
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members).await;

    let started = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members).await;
    assert_eq!(started.elapsed(), Duration::from_secs(60));
    assert_eq!(slow.hits(), 1);
    assert_eq!(quick.hits(), 1);
    assert!(book.cooling(slow.id()).is_some());
    assert!(book.cooling(quick.id()).is_none());

    drop(runner);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(slow.hits(), 1);
}

#[tokio::test(start_paused = true)]
async fn b7f_quota_or_credit_5xx_is_not_probed() {
    for error in [
        PluginError::quota_status(503, "quota exhausted"),
        PluginError::credit_status(503, "credit exhausted"),
        PluginError::status_error(429, "upstream busy"),
    ] {
        let book = make_book(Duration::from_secs(2 * 60));
        let fake = Arc::new(Fake::new(id("classified"), vec![answer()]));
        book.record_failure(fake.id(), &error);
        let mut runner = ProbeRunner::default();
        runner.run_once(&book, &members(&fake)).await;
        tokio::time::advance(Duration::from_secs(60)).await;
        runner.run_once(&book, &members(&fake)).await;
        assert_eq!(fake.hits(), 0, "{error}");
    }
}

#[tokio::test(start_paused = true)]
async fn b7e_probe_in_flight_is_not_doubled_and_times_out_after_twenty_seconds() {
    let book = make_book(Duration::from_secs(30 * 60));
    let mut slow = Arc::new(Fake::new(id("hung"), Vec::new()));
    Arc::get_mut(&mut slow).expect("unique fake").pending = true;
    book.record_failure(slow.id(), &PluginError::connect("down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&slow)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&slow)).await;
    assert_eq!(slow.hits(), 1);
    tokio::time::advance(Duration::from_secs(19)).await;
    runner.run_once(&book, &members(&slow)).await;
    assert_eq!(slow.hits(), 1, "a probe in flight is not started again");

    // The 20-second timeout counts as a failed probe: the next one waits two
    // minutes from the timeout.
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    runner.run_once(&book, &members(&slow)).await;
    assert_eq!(slow.hits(), 1);
    tokio::time::advance(Duration::from_secs(119)).await;
    runner.run_once(&book, &members(&slow)).await;
    assert_eq!(slow.hits(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    runner.run_once(&book, &members(&slow)).await;
    assert_eq!(slow.hits(), 2);
}

#[tokio::test(start_paused = true)]
async fn b7e_unsupported_model_list_stops_the_schedule() {
    let book = make_book(Duration::from_secs(30 * 60));
    let fake = Arc::new(Fake::new(
        id("no-list"),
        vec![Err(PluginError::unsupported("no model list")), answer()],
    ));
    book.record_failure(fake.id(), &PluginError::connect("down"));
    let mut runner = ProbeRunner::default();
    runner.run_once(&book, &members(&fake)).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(fake.hits(), 1);
    tokio::time::advance(Duration::from_secs(10 * 60)).await;
    runner.run_once(&book, &members(&fake)).await;
    assert_eq!(
        fake.hits(),
        1,
        "a member without a model list is not probed again"
    );
    assert!(book.cooling(fake.id()).is_some());
}
