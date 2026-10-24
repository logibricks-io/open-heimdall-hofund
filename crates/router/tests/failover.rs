//! Failover happens only before a stream event is produced.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream;
use hofund_ir::{
    ContentPart, Extensions, Message, Request, Response, Role, StreamEvent, TextDelta,
};
use hofund_router::{
    Completion, FailoverPolicy, Provider, ProviderError, ProviderErrorKind, Router, RouterError,
};

struct Fake {
    id: &'static str,
    hits: Arc<AtomicUsize>,
    kind: Kind,
}

enum Kind {
    Fail(ProviderError),
    Unary,
    StreamOk,
    StreamFail(u16),
}

#[async_trait]
impl Provider for Fake {
    fn id(&self) -> &str {
        self.id
    }

    async fn execute(&self, _request: Request) -> Result<Completion, ProviderError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match &self.kind {
            Kind::Fail(error) => Err(ProviderError {
                kind: error.kind,
                message: error.message.clone(),
                status: error.status,
            }),
            Kind::Unary => Ok(Completion::Unary(Box::new(reply()))),
            Kind::StreamOk => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(text_event("hi")),
                Ok(text_event("!")),
            ])))),
            Kind::StreamFail(status) => Ok(Completion::Stream(Box::pin(stream::iter(vec![Err(
                ProviderError::status(*status, "down"),
            )])))),
        }
    }
}

fn reply() -> Response {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Response {
        id: None,
        model: None,
        message,
        stop: None,
        usage: None,
        extensions: Extensions::new(),
    }
}

fn text_event(text: &str) -> StreamEvent {
    StreamEvent::TextDelta(TextDelta {
        index: 0,
        text: text.to_owned(),
        extensions: hofund_ir::Extensions::new(),
    })
}

fn router(providers: Vec<Arc<dyn Provider>>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), providers);
    Router::new(routes, FailoverPolicy::default())
}

fn fake(id: &'static str, hits: &Arc<AtomicUsize>, kind: Kind) -> Arc<dyn Provider> {
    Arc::new(Fake {
        id,
        hits: Arc::clone(hits),
        kind,
    })
}

#[tokio::test]
async fn ordered_failover_skips_status_and_connect_failures() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let third_hits = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &first_hits,
            Kind::Fail(ProviderError::status(503, "busy")),
        ),
        fake(
            "second",
            &second_hits,
            Kind::Fail(ProviderError::connect("refused")),
        ),
        fake("third", &third_hits, Kind::Unary),
    ]);
    let routed = router
        .dispatch(Request::new("demo"))
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "third");
    assert!(matches!(routed.completion, Completion::Unary(_)));
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(third_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn client_error_does_not_try_the_next_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Fail(ProviderError::status(400, "bad")),
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let error = match router.dispatch(Request::new("demo")).await {
        Ok(_) => panic!("expected a status error"),
        Err(error) => error,
    };
    let RouterError::Upstream { upstream_id, error } = error else {
        panic!("unexpected error");
    };
    assert_eq!(upstream_id, "first");
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(400));
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_error_before_an_event_fails_over() {
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::StreamFail(502),
        ),
        fake("second", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
    ]);
    let routed = router
        .dispatch(Request::new("demo"))
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    let Completion::Stream(mut stream) = routed.completion else {
        panic!("expected a stream");
    };
    let first = stream.next().await.expect("event").expect("ok");
    assert!(matches!(first, StreamEvent::TextDelta(_)));
}

#[tokio::test]
async fn stream_event_commits_the_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = router
        .dispatch(Request::new("demo"))
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_model_has_no_route() {
    let router = router(vec![]);
    let error = match router.dispatch(Request::new("other")).await {
        Ok(_) => panic!("expected a missing route"),
        Err(error) => error,
    };
    assert!(matches!(error, RouterError::NoRoute { .. }));
    assert!(router.models().is_empty());
}
