//! Compaction's single, manually controlled summary budget.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};
use std::time::Duration;

use hofund_plugin_api::{Body, BodyStream, PluginError, StoppedWaiting};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HttpRequest, HttpResponse, async_trait, text_response};

use super::backend::{COMPACT_WITHIN, State};
use super::tests::block_on;
use super::{Kernel, backend};

#[derive(Clone, Default)]
struct Clock {
    fired: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
    asked: Arc<Mutex<Vec<Duration>>>,
}

impl Clock {
    fn state(&self) -> State {
        let clock = self.clone();
        State::with_compact_clock(move |within| {
            clock.asked.lock().expect("asked").push(within);
            let clock = clock.clone();
            Box::pin(poll_fn(move |context| {
                if clock.fired.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    *clock.waker.lock().expect("waker") = Some(context.waker().clone());
                    Poll::Pending
                }
            }))
        })
    }

    fn asked(&self) -> Vec<Duration> {
        self.asked.lock().expect("asked").clone()
    }

    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().expect("waker").take() {
            waker.wake();
        }
    }
}

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct DeadlineKernel {
    answers: Mutex<VecDeque<Option<Result<HttpResponse, PluginError>>>>,
    calls: Arc<AtomicUsize>,
    call_ready: Arc<(Mutex<()>, Condvar)>,
    dropped: Arc<AtomicUsize>,
    markers: Mutex<Vec<StoppedWaiting>>,
}

impl DeadlineKernel {
    fn answering(self, status: u16, body: String) -> Self {
        self.answers.lock().expect("answers").push_back(Some(Ok({
            let mut response = text_response(status, "");
            *response.body_mut() = BodyStream::from_bytes(body.into_bytes());
            response
        })));
        self
    }

    fn pending(self) -> Self {
        self.answers.lock().expect("answers").push_back(None);
        self
    }

    fn markers(&self) -> Vec<StoppedWaiting> {
        self.markers.lock().expect("markers").clone()
    }
}

#[async_trait]
impl Kernel for DeadlineKernel {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        {
            let (lock, ready) = &*self.call_ready;
            let _guard = lock.lock().expect("call wait");
            self.calls.fetch_add(1, Ordering::SeqCst);
            ready.notify_all();
        }
        self.markers.lock().expect("markers").push(
            request
                .extensions()
                .get::<StoppedWaiting>()
                .cloned()
                .expect("compaction cancellation marker"),
        );
        let answer = self.answers.lock().expect("answers").pop_front().flatten();
        match answer {
            Some(answer) => answer,
            None => {
                let _dropped = Dropped(Arc::clone(&self.dropped));
                std::future::pending().await
            }
        }
    }

    async fn send(
        &self,
        _request: HttpRequest,
        _within: Option<Duration>,
    ) -> Result<HttpResponse, PluginError> {
        Err(PluginError::unsupported("no outbound calls"))
    }

    fn codex_relay_fallbacks(&self) -> &[String] {
        &[]
    }
}

fn fire_after(
    calls: &Arc<AtomicUsize>,
    call_ready: &Arc<(Mutex<()>, Condvar)>,
    wanted: usize,
    clock: &Clock,
) -> std::thread::JoinHandle<()> {
    let calls = Arc::clone(calls);
    let call_ready = Arc::clone(call_ready);
    let clock = clock.clone();
    std::thread::spawn(move || {
        let (lock, ready) = &*call_ready;
        let mut guard = lock.lock().expect("call wait");
        while calls.load(Ordering::SeqCst) < wanted {
            guard = ready.wait(guard).expect("call wait");
        }
        drop(guard);
        clock.fire();
    })
}

fn conversation() -> String {
    json!({"model": "smith/forge", "stream": true, "input": [
        {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "fix the login bug"},
        ]},
        {"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "now add a test"},
        ]},
        {"type": "compaction_trigger"},
    ]})
    .to_string()
}

fn compact(kernel: &DeadlineKernel, state: &State) -> (u16, String, String) {
    let mut request = HttpRequest::new(Body::from_bytes(conversation().into_bytes()));
    *request.method_mut() = "POST".parse().expect("method");
    *request.uri_mut() = "/backend-api/codex/responses".parse().expect("URI");
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().expect("header"));
    block_on(async {
        let response = backend::serve_with_state(kernel, backend::BASES, request, state).await;
        let status = response.status().as_u16();
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut stream = response.into_body();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            body.extend_from_slice(&chunk.expect("chunk"));
        }
        (status, kind, String::from_utf8(body).expect("UTF-8"))
    })
}

fn summary(body: &str) -> String {
    body.split("\n\n")
        .filter_map(|event| event.split_once("data: "))
        .filter_map(|(_, data)| json::from_str::<Value>(data).ok())
        .find_map(|event| {
            event
                .pointer("/item/encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .expect("compaction item")
        .strip_prefix("hofund1:")
        .expect("local summary marker")
        .to_owned()
}

fn good_summary(text: &str) -> String {
    json!({"id": "resp_fast", "status": "completed", "output": [
        {"type": "message", "content": [{"type": "output_text", "text": text}]},
    ]})
    .to_string()
}

fn assert_timeout_compaction(body: &str) {
    let summary = summary(body);
    assert!(summary.contains("without a model"), "{summary}");
    assert!(
        summary.contains("no summary came in 4m0s, and Codex stops waiting at 5 minutes"),
        "{summary}"
    );
    assert!(summary.contains("fix the login bug"), "{summary}");
    assert!(summary.contains("now add a test"), "{summary}");
}

#[test]
fn d84_1_a_silent_summary_compacts_locally_and_drops_the_call() {
    assert_eq!(COMPACT_WITHIN, Duration::from_secs(240));
    let clock = Clock::default();
    let kernel = DeadlineKernel::default().pending();
    let fired = fire_after(&kernel.calls, &kernel.call_ready, 1, &clock);
    let (status, kind, body) = compact(&kernel, &clock.state());
    fired.join().expect("clock thread");
    assert_eq!((status, kind.as_str()), (200, "text/event-stream"));
    assert_timeout_compaction(&body);
    assert_eq!(kernel.calls.load(Ordering::SeqCst), 1);
    assert_eq!(kernel.dropped.load(Ordering::SeqCst), 1);
    assert!(kernel.markers()[0].is_stopped());
    assert_eq!(clock.asked(), vec![COMPACT_WITHIN], "one 240 s budget");
}

#[test]
fn d84_2_the_same_budget_covers_a_pending_text_retry() {
    let clock = Clock::default();
    let kernel = DeadlineKernel::default()
        .answering(413, json!({"error": {"message": "too big"}}).to_string())
        .pending();
    let fired = fire_after(&kernel.calls, &kernel.call_ready, 2, &clock);
    let (status, _, body) = compact(&kernel, &clock.state());
    fired.join().expect("clock thread");
    assert_eq!(status, 200, "{body}");
    assert_timeout_compaction(&body);
    assert_eq!(kernel.calls.load(Ordering::SeqCst), 2, "no third ask");
    assert_eq!(
        clock.asked(),
        vec![COMPACT_WITHIN],
        "the retry shares the budget"
    );
    assert_eq!(kernel.dropped.load(Ordering::SeqCst), 1);
    let markers = kernel.markers();
    assert!(markers.iter().all(StoppedWaiting::is_stopped));
}

#[test]
fn d84_3_expiry_during_the_first_ask_never_starts_the_text_retry() {
    let clock = Clock::default();
    let kernel = DeadlineKernel::default().pending();
    let fired = fire_after(&kernel.calls, &kernel.call_ready, 1, &clock);
    let (status, _, body) = compact(&kernel, &clock.state());
    fired.join().expect("clock thread");
    assert_eq!(status, 200, "{body}");
    assert_timeout_compaction(&body);
    assert_eq!(kernel.calls.load(Ordering::SeqCst), 1, "no text retry");
}

#[test]
fn d84_4_a_summary_before_the_deadline_is_used_without_stopping_the_call() {
    let clock = Clock::default();
    let kernel = DeadlineKernel::default().answering(200, good_summary("FAST SUMMARY"));
    let (status, _, body) = compact(&kernel, &clock.state());
    assert_eq!(status, 200, "{body}");
    assert_eq!(summary(&body), "FAST SUMMARY");
    assert_eq!(kernel.calls.load(Ordering::SeqCst), 1);
    assert_eq!(kernel.dropped.load(Ordering::SeqCst), 0);
    assert!(!kernel.markers()[0].is_stopped());
    assert!(!clock.fired.load(Ordering::SeqCst));
}

#[test]
fn d84_5_an_already_expired_budget_does_not_poll_the_text_retry() {
    let clock = Clock::default();
    clock.fire();
    let kernel = DeadlineKernel::default()
        .answering(413, json!({"error": {"message": "too big"}}).to_string())
        .pending();
    let (status, _, body) = compact(&kernel, &clock.state());
    assert_eq!(status, 200, "{body}");
    assert_timeout_compaction(&body);
    assert_eq!(kernel.calls.load(Ordering::SeqCst), 1, "no text retry");
    assert_eq!(kernel.dropped.load(Ordering::SeqCst), 0);
    assert!(kernel.markers()[0].is_stopped());
}

#[test]
fn d84_6_state_debug_names_the_state_without_its_clock() {
    let shown = format!("{:?}", State::default());
    assert_eq!(shown, "State { .. }");
}
